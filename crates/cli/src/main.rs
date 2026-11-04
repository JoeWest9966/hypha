//! hypha — the CLI. Talks HTTP + SSE to the hub; no ssh anywhere.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use hypha_proto::*;
use serde_json::json;
use std::io::{Read, Write};
use std::path::PathBuf;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(name = "hypha", version, about = "your fleet, one command away")]
struct Args {
    /// Hub URL override, e.g. https://hub.example.com
    #[arg(long, global = true, env = "HYPHA_HUB")]
    hub: Option<String>,
    #[arg(long, global = true, env = "HYPHA_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    no_color: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Set the hub URL in your local config (no credentials needed).
    Use { hub_url: Option<String> },
    /// List hosts.
    Hosts,
    /// Print an install command (one-shot by default).
    Add {
        #[arg(long, default_value = "24h")]
        ttl: String,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Reusable for 30 days; use as many times as you like.
        #[arg(long)]
        reusable: bool,
        /// Name for a reusable key.
        #[arg(short = 'n', long)]
        name: Option<String>,
    },
    /// List / revoke saved install keys.
    Keys {
        #[arg(long)]
        revoke: Option<String>,
    },
    /// Run a command on hosts. `--` separates the command.
    Run(Box<RunArgs>),
    /// Open an interactive shell on a host.
    Sh { host: String },
    /// Remove a host from the fleet.
    Rm { host: String },
    /// Recent commands.
    History,
    /// Cancel a running command by exec id.
    Cancel { exec: String },
    /// Audit trail: who did what, newest first.
    Audit {
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Copy a local file to hosts.
    Cp {
        file: String,
        path: String,
        #[arg(long, default_value = "0644")]
        mode: String,
        #[arg(short = 't', long = "tag")]
        tags: Vec<String>,
        /// Hosts (name or id). Empty = every host.
        hosts: Vec<String>,
    },
    /// Install hypha on a machine over ssh, from this machine.
    ///
    /// The credentials never leave your box: this builds an install command and
    /// runs it through your own ssh, so the hub stays a hub and not a vault.
    Push {
        /// ssh target: `root@10.0.0.5`, `host:2222`, or a ~/.ssh/config alias.
        target: String,
        /// Hostname to record (defaults to what the agent reports).
        #[arg(short = 'n', long)]
        name: Option<String>,
        #[arg(short = 't', long = "tag")]
        tags: Vec<String>,
        /// Print the ssh command instead of running it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Hosts (name or id). Empty = every host.
    hosts: Vec<String>,
    /// Target hosts with this tag (repeatable).
    #[arg(short = 't', long = "tag")]
    tags: Vec<String>,
    /// Seconds before the command is killed.
    #[arg(long)]
    timeout: Option<u64>,
    /// Working directory on the remote.
    #[arg(long)]
    cwd: Option<String>,
    /// Don't prefix output lines with the hostname (single-host use).
    #[arg(short, long)]
    quiet: bool,
    /// The command. Everything after `--`.
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
struct Config {
    hub: String,
}

fn config_path(args: &Args) -> PathBuf {
    if let Some(p) = &args.config {
        return p.clone();
    }
    if let Ok(p) = std::env::var("HYPHA_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/hypha/config.json")
}

fn load_config(args: &Args) -> Result<Config> {
    let p = config_path(args);
    let raw = std::fs::read_to_string(&p)
        .with_context(|| format!("no hub configured — run `hypha use <url>` (looked at {})", p.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

fn save_config(args: &Args, cfg: &Config) -> Result<()> {
    let p = config_path(args);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&p, serde_json::to_string_pretty(cfg)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn hub_url(args: &Args, cfg: &Config) -> String {
    args.hub
        .clone()
        .unwrap_or_else(|| cfg.hub.clone())
        .trim_end_matches('/')
        .to_string()
}

fn ws_base(hub: &str) -> String {
    if let Some(r) = hub.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = hub.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        hub.to_string()
    }
}

// ---------------------------------------------------------------------------
// colour
// ---------------------------------------------------------------------------

struct Paint(bool);

impl Paint {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    /// Stable colour per hostname, so the same box always looks the same.
    fn host(&self, name: &str) -> String {
        const CODES: [&str; 6] = ["36", "35", "34", "33", "32", "96"];
        let mut sum = 0u32;
        for b in name.bytes() {
            sum = sum.wrapping_mul(31).wrapping_add(b as u32);
        }
        self.wrap(CODES[(sum as usize) % CODES.len()], name)
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    let paint = Paint(!args.no_color && std::env::var("NO_COLOR").is_err());
    // `use` is the only command that works without a config.
    if let Cmd::Use { hub_url } = &args.cmd {
        return set_hub(&args, hub_url.clone(), &paint).await;
    }
    let mut cfg = load_config(&args)?;
    if let Some(h) = &args.hub {
        cfg.hub = h.clone();
    }

    let cmd = std::mem::replace(&mut args.cmd, Cmd::History);
    match cmd {
        Cmd::Use { .. } => unreachable!(),
        Cmd::Hosts => hosts(&args, &cfg, &paint).await,
        Cmd::Add { ttl, tags, reusable, name } => add(&args, &cfg, &paint, ttl, tags, reusable, name).await,
        Cmd::Keys { revoke } => keys(&args, &cfg, &paint, revoke).await,
        Cmd::Run(run) => run_cmd(&args, &cfg, &paint, *run).await,
        Cmd::Sh { host } => shell(&args, &cfg, &paint, host).await,
        Cmd::Rm { host } => rm(&args, &cfg, &paint, host).await,
        Cmd::History => history(&args, &cfg, &paint).await,
        Cmd::Cancel { exec } => cancel(&cfg, &paint, &exec).await,
        Cmd::Audit {
            action,
            host,
            limit,
        } => audit(&cfg, &paint, action, host, limit).await,
        Cmd::Cp {
            file,
            path,
            mode,
            tags,
            hosts,
        } => cp(&cfg, &paint, &file, &path, &mode, tags, hosts).await,
        Cmd::Push {
            target,
            name,
            tags,
            dry_run,
        } => push(&cfg, &paint, &target, name, tags, dry_run).await,
    }
}

// ---------------------------------------------------------------------------
// cancel / audit / cp / push
// ---------------------------------------------------------------------------

async fn cancel(cfg: &Config, paint: &Paint, exec: &str) -> Result<()> {
    let url = format!("{}/api/exec/{}/cancel", cfg.hub, urlencoding::encode(exec));
    let resp = client_with(cfg).post(&url).send().await?;
    if !resp.status().is_success() {
        bail!("cancel failed: {}", resp.text().await.unwrap_or_default());
    }
    let v: serde_json::Value = resp.json().await?;
    let hosts = v["cancelled"].as_array().cloned().unwrap_or_default();
    if hosts.is_empty() {
        eprintln!("{} nothing was running", paint.dim(".."));
    } else {
        eprintln!(
            "{} signalled {} host(s)",
            paint.green("ok"),
            hosts.len()
        );
    }
    Ok(())
}

async fn audit(
    cfg: &Config,
    paint: &Paint,
    action: Option<String>,
    host: Option<String>,
    limit: usize,
) -> Result<()> {
    let mut url = format!("{}/api/audit?limit={}", cfg.hub, limit);
    if let Some(a) = action {
        url.push_str(&format!("&action={}", urlencoding::encode(&a)));
    }
    if let Some(h) = host {
        url.push_str(&format!("&host={}", urlencoding::encode(&h)));
    }
    let rows: Vec<serde_json::Value> = client_with(cfg).get(&url).send().await?.json().await?;
    if rows.is_empty() {
        eprintln!("{}", paint.dim("no audit entries"));
        return Ok(());
    }
    for r in rows {
        let ts = r["ts"].as_i64().unwrap_or(0);
        let when = stamp(ts);
        let action = r["action"].as_str().unwrap_or("");
        let host = r["host"].as_str().unwrap_or("");
        let detail = r["detail"].as_str().unwrap_or("");
        let detail = if detail.chars().count() > 70 {
            format!("{}…", detail.chars().take(70).collect::<String>())
        } else {
            detail.to_string()
        };
        println!(
            "{}  {:<22} {:<16} {}",
            paint.dim(&when),
            paint.cyan(action),
            host,
            paint.dim(&detail)
        );
    }
    Ok(())
}

/// Local-time-ish stamp without pulling a date library: UTC is honest here.
fn stamp(ts: i64) -> String {
    let secs = ts.max(0) as u64;
    let days = secs / 86400;
    let rem = secs % 86400;
    // Civil-from-days (Howard Hinnant's algorithm), so this does not need chrono.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

async fn cp(
    cfg: &Config,
    paint: &Paint,
    file: &str,
    path: &str,
    mode: &str,
    tags: Vec<String>,
    hosts: Vec<String>,
) -> Result<()> {
    use base64::Engine;
    let bytes = std::fs::read(file).with_context(|| format!("read {file}"))?;
    let mode = u32::from_str_radix(mode.trim_start_matches("0o"), 8)
        .with_context(|| format!("bad mode {mode}"))?;
    let body = serde_json::json!({
        "targets": { "hosts": hosts, "tags": tags, "all": hosts_empty(&hosts, &tags) },
        "path": path,
        "mode": mode,
        "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
    });
    let resp = client_with(cfg)
        .post(format!("{}/api/files", cfg.hub))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("{}", resp.text().await.unwrap_or_default());
    }
    let v: serde_json::Value = resp.json().await?;
    let results = v["results"].as_array().cloned().unwrap_or_default();
    let mut failed = 0;
    for r in results {
        let name = r["hostname"].as_str().unwrap_or("?");
        if r["ok"].as_bool().unwrap_or(false) {
            println!(
                "{} {:<20} {}",
                paint.green("ok"),
                name,
                paint.dim(&format!("{} bytes", r["bytes"]))
            );
        } else {
            failed += 1;
            println!(
                "{} {:<20} {}",
                paint.red("xx"),
                name,
                r["error"].as_str().unwrap_or("failed")
            );
        }
    }
    if failed > 0 {
        bail!("{failed} host(s) failed");
    }
    Ok(())
}

fn hosts_empty(hosts: &[String], tags: &[String]) -> bool {
    hosts.is_empty() && tags.is_empty()
}

async fn push(
    cfg: &Config,
    paint: &Paint,
    target: &str,
    name: Option<String>,
    tags: Vec<String>,
    dry_run: bool,
) -> Result<()> {
    // A one-shot token: if this command leaks through shell history it is worth
    // exactly one enrollment for 24 hours.
    let body = serde_json::json!({
        "reusable": false,
        "label": name.clone().unwrap_or_else(|| format!("push to {target}")),
        "tags": tags,
    });
    let resp = client_with(cfg)
        .post(format!("{}/api/enroll-tokens", cfg.hub))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("{}", resp.text().await.unwrap_or_default());
    }
    let v: serde_json::Value = resp.json().await?;
    let cmd = v["install_cmd"]
        .as_str()
        .context("hub did not return an install command")?
        .to_string();

    let ssh_cmd = format!("ssh {} {}", shell_quote(target), shell_quote(&cmd));
    if dry_run {
        println!("{ssh_cmd}");
        return Ok(());
    }

    eprintln!("{} installing on {target}", paint.dim(".."));
    let status = std::process::Command::new("ssh")
        .arg(target)
        .arg(&cmd)
        .status()
        .context("run ssh (is it installed and is the key loaded?)")?;
    if !status.success() {
        bail!("ssh exited with {}", status.code().unwrap_or(-1));
    }

    // Give the satellite a moment to dial back, then name it.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let hosts = list_hosts_json(cfg).await.unwrap_or_default();
        if let Some(h) = hosts.iter().find(|h| {
            h["online"].as_bool().unwrap_or(false)
                && name
                    .as_deref()
                    .map(|n| h["hostname"].as_str() == Some(n))
                    .unwrap_or(true)
        }) {
            println!(
                "{} {} is online ({})",
                paint.green("ok"),
                h["hostname"].as_str().unwrap_or("?"),
                h["id"].as_str().unwrap_or("?")
            );
            return Ok(());
        }
    }
    eprintln!(
        "{} installed, but no new host showed up yet — check `hypha hosts`",
        paint.dim("..")
    );
    Ok(())
}

async fn list_hosts_json(cfg: &Config) -> Result<Vec<serde_json::Value>> {
    let v: Vec<serde_json::Value> = client_with(cfg)
        .get(format!("{}/api/hosts", cfg.hub))
        .send()
        .await?
        .json()
        .await?;
    Ok(v)
}

/// Quote for a POSIX shell. Single quotes, with embedded quotes escaped, is the
/// only form that is safe for arbitrary command text.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn set_hub(args: &Args, url: Option<String>, paint: &Paint) -> Result<()> {
    let hub = url
        .or_else(|| args.hub.clone())
        .context("usage: hypha use https://hub.example.com")?
        .trim_end_matches('/')
        .to_string();
    // Touch the hub so a typo fails now instead of on the first `run`.
    let resp = reqwest::get(format!("{hub}/api/meta")).await?;
    if !resp.status().is_success() {
        bail!("hub at {hub} returned {}", resp.status());
    }
    save_config(args, &Config { hub: hub.clone() })?;
    eprintln!("{} using {}", paint.green("ok"), hub);
    Ok(())
}

fn client_with(_cfg: &Config) -> reqwest::Client {
    reqwest::Client::new()
}

async fn fetch_hosts(hub: &str, cfg: &Config) -> Result<Vec<Host>> {
    let resp = client_with(cfg).get(format!("{hub}/api/hosts")).send().await?;
    if !resp.status().is_success() {
        bail!("hub error: {}", resp.text().await.unwrap_or_default());
    }
    Ok(resp.json().await?)
}

async fn hosts(args: &Args, cfg: &Config, paint: &Paint) -> Result<()> {
    let hub = hub_url(args, cfg);
    let mut list = fetch_hosts(&hub, cfg).await?;
    list.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    if list.is_empty() {
        println!("{}", paint.dim("no hosts yet — `hypha add` to get an install command"));
        return Ok(());
    }
    let online = list.iter().filter(|h| h.online).count();
    println!(
        "{}",
        paint.dim(&format!("{} hosts · {} online", list.len(), online))
    );
    for h in list {
        let dot = if h.online {
            paint.green("●")
        } else {
            paint.dim("○")
        };
        let tags = if h.tags.is_empty() {
            String::new()
        } else {
            paint.dim(&format!("  [{}]", h.tags.join(",")))
        };
        println!(
            "{} {:<28} {}{}",
            dot,
            paint.bold(&h.hostname),
            paint.dim(&format!(
                "{} {} · v{} · {}",
                h.os,
                h.arch,
                h.version,
                if h.online {
                    format!("load {:.2}", h.load)
                } else {
                    "offline".to_string()
                }
            )),
            tags
        );
    }
    Ok(())
}

async fn add(
    args: &Args,
    cfg: &Config,
    paint: &Paint,
    ttl: String,
    tags: Vec<String>,
    reusable: bool,
    name: Option<String>,
) -> Result<()> {
    let hub = hub_url(args, cfg);
    let secs = if reusable { parse_duration("30d")? } else { parse_duration(&ttl)? };
    let mut body = json!({ "ttl_sec": secs, "tags": tags, "reusable": reusable });
    if let Some(n) = name {
        body["label"] = json!(n);
    }
    let resp = client_with(cfg)
        .post(format!("{hub}/api/enroll-tokens"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("{}", resp.text().await.unwrap_or_default());
    }
    let out: EnrollTokenResponse = resp.json().await?;
    println!("{}", paint.dim("run this on the server, as root:"));
    println!();
    println!("  {}", paint.cyan(&out.install_cmd));
    println!();
    if reusable {
        println!("{}", paint.yellow("  keep this safe: it enrolls unlimited hosts until it expires or is revoked"));
        println!();
    }
    println!(
        "{}",
        paint.dim(&format!(
            "{} · expires {} · {}",
            out.label,
            chrono_like(out.expires_at),
            if reusable { "reusable" } else { "single use" }
        ))
    );
    Ok(())
}

async fn keys(args: &Args, cfg: &Config, paint: &Paint, revoke: Option<String>) -> Result<()> {
    let hub = hub_url(args, cfg);
    if let Some(token) = revoke {
        let resp = client_with(cfg)
            .delete(format!("{hub}/api/enroll-tokens/{token}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("{}", resp.text().await.unwrap_or_default());
        }
        println!("{} revoked {}", paint.green("ok"), token);
        return Ok(());
    }
    let resp = client_with(cfg)
        .get(format!("{hub}/api/enroll-tokens"))
        .send()
        .await?;
    let list: Vec<EnrollTokenInfo> = resp.json().await?;
    let keys: Vec<EnrollTokenInfo> = list.into_iter().filter(|t| t.reusable).collect();
    if keys.is_empty() {
        println!("{}", paint.dim("no reusable keys — `hypha add --reusable -n <name>`"));
        return Ok(());
    }
    for k in keys {
        let state = if k.revoked {
            paint.red("REVOKED")
        } else if k.expires_at < hypha_proto::now() {
            paint.dim("expired")
        } else {
            paint.green("active")
        };
        println!("{} {:<20} {} uses  {}  {}", state, k.label, k.uses, k.token, paint.dim(&k.install_cmd));
    }
    Ok(())
}

fn chrono_like(ts: i64) -> String {
    let secs = ts - hypha_proto::now();
    if secs <= 0 {
        return "now".into();
    }
    format!("in {}h{}m", secs / 3600, (secs % 3600) / 60)
}

fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().unwrap_or(0);
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return s.parse::<i64>().map_err(|_| anyhow::anyhow!("bad duration: {s}")),
    };
    Ok(n * mult)
}

async fn rm(args: &Args, cfg: &Config, paint: &Paint, host: String) -> Result<()> {
    let hub = hub_url(args, cfg);
    let list = fetch_hosts(&hub, cfg).await?;
    let target = find_host(&list, &host)?;
    let resp = client_with(cfg)
        .delete(format!("{hub}/api/hosts/{}", target.id))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("{}", resp.text().await.unwrap_or_default());
    }
    println!("{} removed {}", paint.green("ok"), target.hostname);
    Ok(())
}

fn find_host(list: &[Host], needle: &str) -> Result<Host> {
    if let Some(h) = list
        .iter()
        .find(|h| h.hostname == needle || h.id == needle)
    {
        return Ok(h.clone());
    }
    // unique prefix match
    let matches: Vec<&Host> = list.iter().filter(|h| h.hostname.starts_with(needle)).collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => bail!("no such host: {needle}"),
        _ => bail!(
            "ambiguous host {needle}: {}",
            matches.iter().map(|h| h.hostname.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

async fn history(args: &Args, cfg: &Config, paint: &Paint) -> Result<()> {
    let hub = hub_url(args, cfg);
    let resp = client_with(cfg).get(format!("{hub}/api/execs")).send().await?;
    let rows: Vec<serde_json::Value> = resp.json().await?;
    if rows.is_empty() {
        println!("{}", paint.dim("nothing yet"));
        return Ok(());
    }
    for r in rows {
        println!(
            "{}  {:<40} {}",
            paint.dim(&r["created_at"].to_string()),
            r["command"].as_str().unwrap_or(""),
            paint.dim(r["hosts"].as_str().unwrap_or(""))
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

async fn run_cmd(args: &Args, cfg: &Config, paint: &Paint, run: RunArgs) -> Result<()> {
    let hub = hub_url(args, cfg);
    let list = fetch_hosts(&hub, cfg).await?;

    let mut host_ids = Vec::new();
    for h in &run.hosts {
        host_ids.push(find_host(&list, h)?.id);
    }
    let targets = Targets {
        hosts: host_ids,
        tags: run.tags.clone(),
        all: run.hosts.is_empty() && run.tags.is_empty(),
    };
    let command = run.command.join(" ");

    let resp = client_with(cfg)
        .post(format!("{hub}/api/exec"))
        .json(&ExecRequest {
            targets,
            command: command.clone(),
            timeout_sec: run.timeout,
            cwd: run.cwd.clone(),
        })
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("{}", resp.text().await.unwrap_or_default());
    }
    let exec: ExecResponse = resp.json().await?;
    if exec.jobs.is_empty() {
        bail!("no hosts matched");
    }

    let prefix = !run.quiet && exec.jobs.len() > 1;
    let width = exec.jobs.iter().map(|j| j.hostname.len()).max().unwrap_or(0).min(24);

    let url = format!("{hub}/api/exec/{}/events", exec.exec_id);
    let resp = reqwest::get(&url).await?;
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut failures = 0usize;
    let mut done = 0usize;
    let mut at_line_start = true;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(ev) = serde_json::from_str::<ExecEvent>(payload) else {
                continue;
            };
            match ev {
                ExecEvent::Started { hostname, pid, .. } => {
                    if prefix {
                        println!("{} {}", paint.dim("→"), paint.dim(&format!("{hostname} pid {pid}")));
                    }
                }
                ExecEvent::Output {
                    host_id,
                    hostname,
                    stream,
                    data,
                    ..
                } => {
                    let _ = host_id;
                    let bytes = b64d(&data).unwrap_or_default();
                    let text = String::from_utf8_lossy(&bytes);
                    if prefix {
                        for (i, part) in text.split_inclusive('\n').enumerate() {
                            if i == 0 && at_line_start {
                                let label = format!("{:<width$}", hostname, width = width);
                                print!("{} ", paint.host(&label));
                            }
                            if stream == Stream::Stderr {
                                print!("{}", paint.yellow(part.trim_end_matches('\n')));
                                if part.ends_with('\n') {
                                    println!();
                                }
                            } else {
                                print!("{part}");
                            }
                            at_line_start = part.ends_with('\n');
                        }
                    } else if stream == Stream::Stderr {
                        eprint!("{text}");
                    } else {
                        print!("{text}");
                    }
                    let _ = std::io::stdout().flush();
                }
                ExecEvent::Exit {
                    hostname,
                    code,
                    duration_ms,
                    ..
                } => {
                    done += 1;
                    let ok = code == Some(0);
                    if !ok {
                        failures += 1;
                    }
                    if prefix || done > 1 {
                        let status = if ok {
                            paint.green(&format!("exit {}", code.unwrap_or(-1)))
                        } else {
                            paint.red(&format!("exit {}", code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())))
                        };
                        println!(
                            "{} {} {}",
                            paint.dim("←"),
                            paint.host(&hostname),
                            paint.dim(&format!("{status} {}ms", duration_ms))
                        );
                    }
                }
                ExecEvent::Failed { hostname, message, .. } => {
                    failures += 1;
                    done += 1;
                    eprintln!("{} {} {}", paint.red("!"), hostname, message);
                }
                ExecEvent::Done => {}
            }
        }
    }

    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

async fn shell(args: &Args, cfg: &Config, paint: &Paint, host: String) -> Result<()> {
    let hub = hub_url(args, cfg);
    let list = fetch_hosts(&hub, cfg).await?;
    let target = find_host(&list, &host)?;
    if !target.online {
        bail!("{} is offline", target.hostname);
    }

    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let url = format!(
        "{}/api/pty?host_id={}&cols={}&rows={}",
        ws_base(&hub),
        urlencoding::encode(&target.id),
        cols,
        rows
    );

    let ws = tokio_tungstenite::connect_async(&url)
        .await
        .context("connect to hub")?;
    let (mut sink, mut stream) = ws.0.split();

    eprintln!(
        "{} {} — detach with Ctrl-] , exit with `exit`",
        paint.green("shell"),
        paint.bold(&target.hostname)
    );

    // stdin -> socket, on a blocking thread because terminals are blocking.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let raw = crossterm::terminal::enable_raw_mode();
    let result = shell_loop(&mut sink, &mut stream, &mut rx, cols, rows).await;
    if raw.is_ok() {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    result
}

async fn shell_loop<S, R>(
    sink: &mut S,
    stream: &mut R,
    stdin_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    cols: u16,
    rows: u16,
) -> Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut size = (cols, rows);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut stdin_open = true;
    // When stdin hits EOF (piped input, `echo cmd | hypha sh host`) we stop
    // feeding the pty but keep draining output until it goes quiet.
    let mut drain_until: Option<std::time::Instant> = None;

    loop {
        tokio::select! {
            input = stdin_rx.recv(), if stdin_open => {
                let Some(bytes) = input else {
                    stdin_open = false;
                    drain_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
                    continue;
                };
                // Ctrl-] detaches.
                if bytes == [0x1d] { break; }
                let frame = serde_json::to_string(&TermIn::Input { data: String::from_utf8_lossy(&bytes).to_string() })?;
                sink.send(Message::Text(frame.into())).await?;
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(out) = serde_json::from_str::<TermOut>(&t) {
                            match out {
                                TermOut::Output { data } => {
                                    if drain_until.is_some() {
                                        drain_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                                    }
                                    let bytes = b64d(&data).unwrap_or_default();
                                    let mut stdout = std::io::stdout();
                                    stdout.write_all(&bytes)?;
                                    stdout.flush()?;
                                }
                                TermOut::Exit { .. } => break,
                                TermOut::Error { message } => {
                                    let _ = crossterm::terminal::disable_raw_mode();
                                    eprintln!("\r\nhypha: {message}");
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                if let Some(deadline) = drain_until {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                }
                if let Ok(new) = crossterm::terminal::size() {
                    if new != size {
                        size = new;
                        let frame = serde_json::to_string(&TermIn::Resize { cols: new.0, rows: new.1 })?;
                        sink.send(Message::Text(frame.into())).await?;
                    }
                }
            }
        }
    }
    Ok(())
}
