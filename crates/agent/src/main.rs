//! hypha-sat — the satellite. Dials the hub, holds a shell open, dies quietly.
//!
//! Design notes that matter:
//!  * It never listens on a port. Outbound only. That is the whole point.
//!  * Every exec runs in its own session (setsid) so signals can kill the
//!    whole tree, not just the leader.
//!  * Ptys outlive the client. Reattaching is just a new relay through the hub.

mod exec;
mod files;
mod term;
mod sysstat;
mod tls;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use hypha_proto::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(name = "hypha-sat", version, about = "hypha satellite agent")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
    /// Config path
    #[arg(long, global = true, default_value = "/etc/hypha/agent.json", env = "HYPHA_AGENT_CONFIG")]
    config: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Register this machine with a hub using a one-time token.
    Enroll {
        #[arg(long)]
        hub: String,
        #[arg(long)]
        token: String,
        /// Skip TLS verification (self-signed hub).
        #[arg(long)]
        insecure: bool,
        /// Optional comma separated tags; the enroll token's tags win if set.
        #[arg(long)]
        tags: Option<String>,
    },
    /// Run in the foreground (systemd does this for you).
    Run,
    /// Print the local config, minus the key.
    Status,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct AgentConfig {
    pub hub: String,
    pub host_id: String,
    /// hex ed25519 secret key (32 bytes)
    pub secret: String,
    pub hostname: String,
    #[serde(default)]
    pub insecure: bool,
}

impl AgentConfig {
    fn load(path: &PathBuf) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("no config at {} — run `hypha-sat enroll` first", path.display()))?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..end]) {
                return s.to_string();
            }
        }
    }
    "unknown".into()
}

fn ws_url(hub: &str) -> String {
    let h = hub.trim_end_matches('/');
    let base = if let Some(rest) = h.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = h.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if h.starts_with("ws://") || h.starts_with("wss://") {
        h.to_string()
    } else {
        format!("ws://{h}")
    };
    format!("{base}/agent")
}

fn http_url(hub: &str) -> String {
    let h = hub.trim_end_matches('/');
    if let Some(rest) = h.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = h.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else {
        format!("http://{h}")
    }
}

fn signing_key(cfg: &AgentConfig) -> Result<ed25519_dalek::SigningKey> {
    let bytes = hex::decode(&cfg.secret)?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad secret key length"))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&arr))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hypha_sat=info".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    match args.cmd {
        Cmd::Enroll {
            hub,
            token,
            insecure,
            tags,
        } => {
            use rand_core::OsRng;
            let key = ed25519_dalek::SigningKey::generate(&mut OsRng);
            let pubkey = hex::encode(key.verifying_key().to_bytes());
            let body = EnrollRequest {
                token,
                pubkey: pubkey.clone(),
                hostname: hostname(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                version: env!("CARGO_PKG_VERSION").into(),
            };
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(insecure)
                .build()?;
            let url = format!("{}/api/enroll", http_url(&hub));
            let resp = client.post(&url).json(&body).send().await?;
            if !resp.status().is_success() {
                let code = resp.status();
                let text = resp.text().await.unwrap_or_default();
                bail!("enroll failed ({code}): {text}");
            }
            let out: EnrollResponse = resp.json().await?;
            let cfg = AgentConfig {
                hub: http_url(&hub),
                host_id: out.host_id.clone(),
                secret: hex::encode(key.to_bytes()),
                hostname: out.hostname.clone(),
                insecure,
            };
            cfg.save(&args.config)?;
            println!("enrolled as {} ({})", out.hostname, out.host_id);
            println!("config: {}", args.config.display());
            if let Some(t) = tags {
                println!("note: tags come from the enroll token, ignoring --tags={t}");
            }
        }
        Cmd::Status => {
            let cfg = AgentConfig::load(&args.config)?;
            println!("hub:      {}", cfg.hub);
            println!("host_id:  {}", cfg.host_id);
            println!("hostname: {}", cfg.hostname);
            println!("config:   {}", args.config.display());
        }
        Cmd::Run => {
            let cfg = AgentConfig::load(&args.config)?;
            run(cfg).await?;
        }
    }
    Ok(())
}

async fn run(cfg: AgentConfig) -> Result<()> {
    let key = Arc::new(signing_key(&cfg)?);
    let mut backoff = 1u64;
    loop {
        match session(&cfg, key.clone()).await {
            Ok(()) => {
                tracing::info!("connection closed by hub");
                backoff = 1;
            }
            Err(e) => {
                tracing::warn!("connection failed: {e:#}");
            }
        }
        let wait = Duration::from_secs(backoff);
        tracing::info!("reconnecting in {}s", backoff);
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn session(cfg: &AgentConfig, key: Arc<ed25519_dalek::SigningKey>) -> Result<()> {
    let url = ws_url(&cfg.hub);
    tracing::info!("connecting to {url}");

    let mut tls: Option<tokio_tungstenite::Connector> = None;
    if cfg.insecure {
        tls = Some(tokio_tungstenite::Connector::Rustls(Arc::new(
            tls::insecure_tls_config(),
        )));
    }

    let (mut socket, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(&url, None, false, tls).await?;

    // --- challenge ----------------------------------------------------------
    let nonce = match next_json(&mut socket).await? {
        Some(HubMsg::Challenge { nonce }) => nonce,
        other => bail!("expected challenge, got {other:?}"),
    };
    use ed25519_dalek::Signer;
    let sig = hex::encode(key.sign(nonce.as_bytes()).to_bytes());

    socket
        .send(Message::Text(
            serde_json::to_string(&AgentMsg::Join {
                protocol: PROTOCOL_VERSION,
                host_id: cfg.host_id.clone(),
                sig,
                version: env!("CARGO_PKG_VERSION").into(),
                hostname: cfg.hostname.clone(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
            })?
            .into(),
        ))
        .await?;

    match next_json(&mut socket).await? {
        Some(HubMsg::Welcome { version, .. }) => {
            tracing::info!("joined hub (hypha {version})");
        }
        Some(HubMsg::Rejected { reason }) => bail!("rejected: {reason}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }

    // --- split and pump -----------------------------------------------------
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AgentMsg>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    let ctx = Arc::new(exec::Ctx {
        out: out_tx.clone(),
        jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
        ptys: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    // periodic stat
    {
        let tx = out_tx.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(30));
            // cpu_pct is a delta and needs the previous jiffies kept across ticks.
            let mut prev_cpu = None;
            loop {
                iv.tick().await;
                let load = std::fs::read_to_string("/proc/loadavg")
                    .ok()
                    .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f32>().ok()))
                    .unwrap_or(0.0);
                let m = crate::sysstat::collect_with(&mut prev_cpu);
                if tx
                    .send(AgentMsg::Stat {
                        load,
                        procs: m.procs,
                        cpu_pct: m.cpu_pct,
                        mem_used: m.mem_used,
                        mem_total: m.mem_total,
                        disk_used: m.disk_used,
                        disk_total: m.disk_total,
                        uptime_sec: m.uptime_sec,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    while let Some(frame) = stream.next().await {
        let Ok(Message::Text(text)) = frame else {
            continue;
        };
        let Ok(msg) = serde_json::from_str::<HubMsg>(&text) else {
            continue;
        };
        match msg {
            HubMsg::Ping => {
                let _ = out_tx.send(AgentMsg::Pong);
            }
            HubMsg::Shutdown => break,
            HubMsg::Exec {
                id,
                command,
                cwd,
                timeout_sec,
            } => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    exec::run(ctx, id, command, cwd, timeout_sec).await;
                });
            }
            HubMsg::ExecSignal { id, signal } => {
                exec::signal(&ctx, &id, signal);
            }
            HubMsg::FileWrite {
                id,
                path,
                mode,
                data,
            } => {
                use base64::Engine;
                let res = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .map_err(|e| anyhow::anyhow!("base64: {e}"))
                    .and_then(|bytes| files::write(&path, mode, &bytes));
                let msg = match res {
                    Ok(bytes) => AgentMsg::FileResult {
                        id,
                        ok: true,
                        bytes,
                        error: None,
                    },
                    Err(e) => AgentMsg::FileResult {
                        id,
                        ok: false,
                        bytes: 0,
                        error: Some(format!("{e:#}")),
                    },
                };
                let _ = ctx.out.send(msg);
            }
            HubMsg::PtyOpen {
                id,
                term,
                cols,
                rows,
            } => {
                if let Err(e) = term::open(&ctx, id.clone(), term, cols, rows) {
                    let _ = ctx.out.send(AgentMsg::Error {
                        id,
                        message: format!("pty: {e:#}"),
                    });
                }
            }
            HubMsg::PtyInput { id, data } => term::input(&ctx, &id, &data),
            HubMsg::PtyResize { id, cols, rows } => term::resize(&ctx, &id, cols, rows),
            HubMsg::PtyClose { id } => term::close(&ctx, &id),
            HubMsg::Challenge { .. } | HubMsg::Welcome { .. } | HubMsg::Rejected { .. } => {}
        }
    }

    exec::kill_all(&ctx);
    term::close_all(&ctx);
    writer.abort();
    Ok(())
}

async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<Option<HubMsg>> {
    let msg = tokio::time::timeout(Duration::from_secs(15), socket.next())
        .await
        .map_err(|_| anyhow::anyhow!("handshake timeout"))?;
    match msg {
        Some(Ok(Message::Text(t))) => Ok(serde_json::from_str::<HubMsg>(&t).ok()),
        Some(Ok(_)) => Ok(None),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}
