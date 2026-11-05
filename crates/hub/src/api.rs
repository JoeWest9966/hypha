//! HTTP API + SSE + static panel.

use crate::state::{AppState, ExecChannel};
use crate::util::gen_id;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hypha_proto::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub type ApiErr = (StatusCode, Json<ApiError>);

pub fn err<T>(code: StatusCode, msg: impl Into<String>) -> Result<T, ApiErr> {
    Err((code, Json(ApiError { error: msg.into() })))
}

type Api<T> = Result<Json<T>, ApiErr>;

// ---------------------------------------------------------------------------
// access control
//
// There is deliberately no login. The hub is meant to sit inside a private
// network (Tailscale / WireGuard / loopback) and the network is the boundary.
// Do not expose port 5522 to the internet: an unauthenticated caller can run
// arbitrary commands as root on every enrolled host.
// ---------------------------------------------------------------------------

/// Base URL for install commands.
///
/// The origin the caller just used wins, because an address that served this
/// request is by definition one the operator can reach — whether that is a
/// tailnet IP, a MagicDNS name or a proxy hostname. `public_url` is the
/// fallback for callers with no Host header (a local script), and it is what
/// the hub advertises when nothing else is known.
fn public_base(state: &AppState, headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty())
        // A loopback origin is reachable from this box and nowhere else. An
        // install command built from it would be pasted into a remote shell and
        // fail, so fall through to public_url instead.
        .filter(|h| {
            let name = h.rsplit_once(':').map(|(n, _)| n).unwrap_or(h);
            !(name == "localhost" || name == "[::1]" || name.starts_with("127."))
        });
    if let Some(host) = host {
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("http");
        return format!("{proto}://{host}").trim_end_matches('/').to_string();
    }
    if !state.cfg.public_url.is_empty() {
        return state.cfg.public_url.trim_end_matches('/').to_string();
    }
    "http://localhost:5522".to_string()
}

// ---------------------------------------------------------------------------
// public endpoints
// ---------------------------------------------------------------------------

pub async fn meta(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let hosts = st.db.list_hosts().unwrap_or_default();
    let online = st.online_ids().await.len();
    Json(json!({
        "name": "hypha",
        "version": env!("CARGO_PKG_VERSION"),
        // The panel is a separate project that ships on its own schedule. This
        // is the only thing stopping it from rendering a contract it does not
        // understand.
        "api_version": API_VERSION,
        "hosts_total": hosts.len(),
        "hosts_online": online,
    }))
}

pub async fn enroll(State(st): State<Arc<AppState>>, Json(req): Json<EnrollRequest>) -> Api<EnrollResponse> {
    let host_id = gen_id("h", 10);
    let tags = match st.db.consume_enroll_token(&req.token, &host_id) {
        Some(t) => t,
        None => return err(StatusCode::FORBIDDEN, "invalid or expired enroll token"),
    };
    if hex::decode(&req.pubkey).map(|v| v.len()).unwrap_or(0) != 32 {
        return err(StatusCode::BAD_REQUEST, "pubkey must be 32 hex bytes");
    }
    let host = Host {
        id: host_id.clone(),
        hostname: req.hostname.clone(),
        os: req.os,
        arch: req.arch,
        version: req.version,
        tags,
        created_at: now(),
        last_seen: now(),
        online: false,
        load: 0.0,
        metrics: None,
    };
    // A reinstall mints a new identity. If we kept the old row, every
    // "Reinstall" in the panel would add a host that is offline forever. So the
    // newest claim on a hostname retires the older ones: hostnames are how a
    // human names a machine, and two machines wearing the same name is a config
    // error where the newer one wins.
    for stale in st.db.hosts_by_hostname(&req.hostname) {
        retire_host(&st, &stale).await;
        st.db.audit("host.replace", &stale, &req.hostname);
        tracing::info!(host = %req.hostname, old = %stale, "host replaced by re-enrollment");
    }
    if let Err(e) = st.db.insert_host(&host, &req.pubkey) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}"));
    }
    st.db.audit("host.enroll", &host.id, &host.hostname);
    tracing::info!(host = %host.hostname, id = %host.id, "host enrolled");
    Ok(Json(EnrollResponse {
        host_id,
        hostname: req.hostname,
        hub: String::new(),
    }))
}

// ---------------------------------------------------------------------------
// hosts
// ---------------------------------------------------------------------------

pub async fn list_hosts(State(st): State<Arc<AppState>>) -> Api<Vec<Host>> {
    let mut hosts = st.db.list_hosts().unwrap_or_default();
    let agents = st.agents.read().await;
    for h in hosts.iter_mut() {
        if let Some(a) = agents.get(&h.id) {
            h.online = true;
            h.load = a.load();
            h.metrics = a.metrics();
        }
    }
    Ok(Json(hosts))
}

pub async fn delete_host(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Api<serde_json::Value> {
    retire_host(&st, &id).await;
    Ok(Json(json!({ "ok": true })))
}

/// Everything that has to die with a host record: the row, the live connection,
/// and any shell attached to it. Shells are the part people forget — they keep
/// an agent process alive forever with nobody able to reach it.
async fn retire_host(st: &Arc<AppState>, id: &str) {
    let _ = st.db.delete_host(id);
    if let Some(a) = st.agent(id).await {
        a.send(HubMsg::Shutdown);
    }
    let doomed: Vec<Arc<crate::state::PtySession>> = st
        .ptys
        .read()
        .await
        .values()
        .filter(|p| p.host_id == id)
        .cloned()
        .collect();
    for p in doomed {
        if let Some(a) = st.agent(&p.host_id).await {
            a.send(HubMsg::PtyClose { id: p.id.clone() });
        }
        st.ptys.write().await.remove(&p.id);
    }
}

pub async fn set_tags(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Api<serde_json::Value> {
    let tags: Vec<String> = body
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    st.db.set_tags(&id, &tags);
    st.db.audit("host.tags", &id, &tags.join(","));
    Ok(Json(json!({ "ok": true })))
}

fn install_command(base: &str, token: &str) -> String {
    // `curl ... | sudo sh` reports the *pipeline's* last status. When curl cannot
    // reach the hub, sh gets an empty stdin, exits 0, and the install looks
    // successful on a machine where nothing happened — which is exactly how a
    // 130-second hang got reported as "installed, but the host never showed up".
    //
    // So fetch first, then feed the script to sh on stdin: the download's exit
    // status survives, and nothing is written to disk. The previous version
    // downloaded to `mktemp` and deleted it afterwards, which left a file in /tmp
    // containing a live enroll token whenever the run was interrupted (Ctrl-C, a
    // dropped ssh session), and its closing `exit $rc` took the caller's shell
    // down with it — pasted into an ssh session, that is a logout.
    //
    // `--max-time` matters as much as `--connect-timeout`: a hub that accepts the
    // connection and then stalls would otherwise hang until the operator hits
    // Ctrl-C, which is the case that used to leak the token.
    format!(
        "if s=$(curl -fsSL --connect-timeout 8 --max-time 60 '{base}/install.sh?token={token}'); then \
           printf '%s' \"$s\" | {{ if [ \"$(id -u)\" = 0 ]; then sh; else sudo sh; fi; }}; \
         else echo 'hypha: could not download the installer from {base} -- the hub is unreachable' >&2; false; fi"
    )
}

pub async fn create_enroll_token(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<EnrollTokenRequest>,
) -> Api<EnrollTokenResponse> {
    // Reusable keys live 30 days; one-shot keys live 24h. Explicit ttl wins,
    // but never more than 30 days -- a leaked key should have a shelf life.
    let default_ttl = if req.reusable { 30 * 86400 } else { 86400 };
    let ttl = req
        .ttl_sec
        .unwrap_or(default_ttl)
        .clamp(60, 30 * 86400);
    let token = gen_id("e", 24);
    let tags = req.tags.unwrap_or_default();
    let label = if req.label.trim().is_empty() {
        if req.reusable {
            "reusable key".to_string()
        } else {
            "one-shot".to_string()
        }
    } else {
        req.label.trim().chars().take(64).collect()
    };
    let expires_at = now() + ttl;
    st.db
        .create_enroll_token(&token, &tags, &label, req.reusable, ttl);
    let base = public_base(&st, &headers);
    st.db.audit(
        if req.reusable {
            "enroll.token.reusable"
        } else {
            "enroll.token"
        },
        "",
        &format!("{label} [{}]", tags.join(",")),
    );
    Ok(Json(EnrollTokenResponse {
        token: token.clone(),
        expires_at,
        install_cmd: install_command(&base, &token),
        reusable: req.reusable,
        label,
    }))
}

pub async fn list_enroll_tokens(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Api<Vec<EnrollTokenInfo>> {
    let base = public_base(&st, &headers);
    let mut list = st.db.list_enroll_tokens();
    for t in list.iter_mut() {
        t.install_cmd = install_command(&base, &t.token);
    }
    Ok(Json(list))
}

pub async fn revoke_enroll_token(
    State(st): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Api<serde_json::Value> {
    if !st.db.revoke_enroll_token(&token) {
        return err(StatusCode::NOT_FOUND, "unknown token");
    }
    st.db.audit("enroll.token.revoke", "", &token);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

/// Resolve targets to hosts. Shared by exec and file push so "which machines
/// does `all` mean" has exactly one answer.
pub async fn pick_hosts(st: &AppState, t: &Targets) -> Result<Vec<Host>, ApiErr> {
    let hosts = st.db.list_hosts().unwrap_or_default();
    let by_all = t.all || (t.hosts.is_empty() && t.tags.is_empty());
    let picked: Vec<Host> = hosts
        .into_iter()
        .filter(|h| {
            let by_id = t.hosts.iter().any(|x| x == &h.id);
            let by_tag = !t.tags.is_empty() && h.tags.iter().any(|tag| t.tags.iter().any(|x| x == tag));
            by_id || by_tag || by_all
        })
        .collect();
    if picked.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no hosts matched");
    }
    Ok(picked)
}

pub async fn create_exec(
    State(st): State<Arc<AppState>>,
    Json(req): Json<ExecRequest>,
) -> Api<ExecResponse> {
    if req.command.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty command");
    }

    let picked = pick_hosts(&st, &req.targets).await?;
    let online = st.agents.read().await;

    let exec_id = gen_id("x", 10);
    let timeout = req.timeout_sec.unwrap_or(st.cfg.exec_timeout_sec).min(86400);

    // Every picked host gets a job id, online or not: the panel and the CLI
    // both render one pane per job, and a vanished host still owes an answer.
    let mut jobs = Vec::new();
    let mut offline: Vec<String> = Vec::new();
    for h in &picked {
        let job_id = gen_id("j", 10);
        if !online.contains_key(&h.id) {
            offline.push(job_id.clone());
        }
        jobs.push(JobRef {
            host_id: h.id.clone(),
            hostname: h.hostname.clone(),
            job_id,
        });
    }
    drop(online);

    st.db.record_exec(
        &exec_id,
        &req.command,
        &picked.iter().map(|h| h.hostname.as_str()).collect::<Vec<_>>().join(","),
    );
    st.db.audit("exec", "", &req.command);

    let total = jobs.len() + offline.len();
    let chan = Arc::new(ExecChannel::new(exec_id.clone(), total, st.db.clone()));
    st.execs.write().await.insert(exec_id.clone(), chan.clone());

    for job_id in offline {
        if let Some(j) = jobs.iter().find(|j| j.job_id == job_id) {
            chan.push(&ExecEvent::Failed {
                job_id: job_id.clone(),
                host_id: j.host_id.clone(),
                hostname: j.hostname.clone(),
                message: "host offline".into(),
            })
            .await;
            chan.job_done().await;
        }
    }

    for j in &jobs {
        st.jobs.write().await.insert(
            j.job_id.clone(),
            crate::state::JobRoute {
                exec_id: exec_id.clone(),
                host_id: j.host_id.clone(),
                hostname: j.hostname.clone(),
            },
        );
        let sent = match st.agent(&j.host_id).await {
            Some(a) => a.send(HubMsg::Exec {
                id: j.job_id.clone(),
                command: req.command.clone(),
                cwd: req.cwd.clone(),
                timeout_sec: Some(timeout),
            }),
            None => false,
        };
        if !sent {
            st.jobs.write().await.remove(&j.job_id);
            chan.push(&ExecEvent::Failed {
                job_id: j.job_id.clone(),
                host_id: j.host_id.clone(),
                hostname: j.hostname.clone(),
                message: "host disconnected".into(),
            })
            .await;
            chan.job_done().await;
        }
    }

    // Reap the channel once nobody is watching for a while.
    let st2 = st.clone();
    let eid = exec_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1800)).await;
        st2.execs.write().await.remove(&eid);
    });

    Ok(Json(ExecResponse { exec_id, jobs }))
}

pub async fn exec_events(
    State(st): State<Arc<AppState>>,
    Path(exec_id): Path<String>,
) -> Response {
    let chan = st.execs.read().await.get(&exec_id).cloned();

    // A run that has left memory is still a run that happened. Serve it from
    // SQLite so history is browsable after a hub restart instead of 404ing.
    let Some(chan) = chan else {
        let lines = st.db.load_chunks(&exec_id);
        if lines.is_empty() {
            return (StatusCode::NOT_FOUND, Json(ApiError { error: "unknown exec".into() }))
                .into_response();
        }
        let stream = async_stream::stream! {
            for line in lines {
                yield Ok::<Event, std::convert::Infallible>(Event::default().event("e").data(line));
            }
        };
        return Sse::new(stream).keep_alive(KeepAlive::default()).into_response();
    };

    let stream = async_stream::stream! {
        let mut cursor = 0usize;
        let mut idle = 0u32;
        loop {
            let (batch, done) = {
                let log = chan.log.lock().await;
                (log.lines.iter().skip(cursor).cloned().collect::<Vec<_>>(), log.done)
            };
            if !batch.is_empty() {
                idle = 0;
                for line in batch {
                    cursor += 1;
                    yield Ok::<Event, std::convert::Infallible>(Event::default().event("e").data(line));
                }
                continue;
            }
            if done {
                break;
            }
            idle += 1;
            if idle % 300 == 0 {
                yield Ok::<Event, std::convert::Infallible>(Event::default().comment("ka"));
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

pub async fn recent_execs(State(st): State<Arc<AppState>>) -> Api<Vec<serde_json::Value>> {
    let live: Vec<String> = st.execs.read().await.keys().cloned().collect();
    let mut out = st.db.recent_execs(50);
    for row in out.iter_mut() {
        let is_live = row
            .get("id")
            .and_then(|v| v.as_str())
            .map(|id| live.iter().any(|l| l == id))
            .unwrap_or(false);
        if let Some(obj) = row.as_object_mut() {
            obj.insert("live".into(), json!(is_live));
        }
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// cancel
// ---------------------------------------------------------------------------

pub async fn cancel_exec(
    State(st): State<Arc<AppState>>,
    Path(exec_id): Path<String>,
) -> Api<serde_json::Value> {
    if !st.execs.read().await.contains_key(&exec_id) {
        return err(StatusCode::NOT_FOUND, "unknown exec");
    }
    // Only jobs still in the routing table are running; the rest already
    // reported an exit and are simply not cancelled.
    let running: Vec<(String, String)> = {
        let jobs = st.jobs.read().await;
        jobs.iter()
            .filter(|(_, r)| r.exec_id == exec_id)
            .map(|(job, r)| (job.clone(), r.host_id.clone()))
            .collect()
    };
    let mut cancelled = Vec::new();
    for (job_id, host_id) in running {
        if let Some(a) = st.agent(&host_id).await {
            if a.send(HubMsg::ExecSignal {
                id: job_id,
                signal: Signal::Term,
            }) {
                cancelled.push(host_id);
            }
        }
    }
    st.db.audit("exec.cancel", "", &exec_id);

    // Escalate to SIGKILL for anything that ignored SIGTERM. The agent reaps
    // the whole process group, so a shell script's children die too.
    let st2 = st.clone();
    let ids = cancelled.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let jobs = st2.jobs.read().await;
        for (job_id, host_id) in jobs.iter().map(|(j, r)| (j.clone(), r.host_id.clone())) {
            if !ids.contains(&host_id) {
                continue;
            }
            if let Some(a) = st2.agent(&host_id).await {
                a.send(HubMsg::ExecSignal {
                    id: job_id,
                    signal: Signal::Kill,
                });
            }
        }
    });

    Ok(Json(json!({ "cancelled": cancelled })))
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

pub async fn push_files(
    State(st): State<Arc<AppState>>,
    Json(req): Json<FilePushRequest>,
) -> Api<serde_json::Value> {
    if req.path.trim().is_empty() || !req.path.starts_with('/') {
        return err(StatusCode::BAD_REQUEST, "path must be absolute");
    }
    let data = match base64_decode(&req.content) {
        Some(d) => d,
        None => return err(StatusCode::BAD_REQUEST, "content is not valid base64"),
    };
    let picked = match crate::api::pick_hosts(&st, &req.targets).await {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let mode = req.mode.unwrap_or(0o644);
    let bytes = data.len();

    let mut results = Vec::new();
    for h in picked {
        let Some(agent) = st.agent(&h.id).await else {
            results.push(json!({
                "host_id": h.id, "hostname": h.hostname, "ok": false, "bytes": 0,
                "error": "offline",
            }));
            continue;
        };
        let id = gen_id("f", 10);
        let (tx, rx) = tokio::sync::oneshot::channel();
        st.waiters.write().await.insert(id.clone(), tx);
        if !agent.send(HubMsg::FileWrite {
            id: id.clone(),
            path: req.path.clone(),
            mode,
            data: req.content.clone(),
        }) {
            st.waiters.write().await.remove(&id);
            results.push(json!({
                "host_id": h.id, "hostname": h.hostname, "ok": false, "bytes": 0,
                "error": "agent send failed",
            }));
            continue;
        }
        let label = (h.id.clone(), h.hostname.clone());
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(AgentMsg::FileResult { ok, bytes, error, .. })) => {
                results.push(json!({
                    "host_id": label.0, "hostname": label.1, "ok": ok,
                    "bytes": bytes, "error": error,
                }));
            }
            _ => {
                st.waiters.write().await.remove(&id);
                results.push(json!({
                    "host_id": label.0, "hostname": label.1, "ok": false, "bytes": 0,
                    "error": "timeout",
                }));
            }
        }
    }
    st.db
        .audit("file.push", "", &format!("{} ({}B, {} hosts)", req.path, bytes, results.len()));
    Ok(Json(json!({ "results": results })))
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

// ---------------------------------------------------------------------------
// audit
// ---------------------------------------------------------------------------

pub async fn list_audit(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Api<Vec<serde_json::Value>> {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    let action = q.get("action").cloned().unwrap_or_default();
    let host = q.get("host").cloned().unwrap_or_default();
    Ok(Json(st.db.list_audit(limit, &action, &host)))
}

// ---------------------------------------------------------------------------
// pty
// ---------------------------------------------------------------------------

/// Allocate a session and ask the agent for a shell. Returns before the shell
/// is ready: the agent's `PtyChunk`s land in the session's scrollback, so an
/// attach that happens a moment later still sees the prompt.
pub async fn open_pty(
    State(st): State<Arc<AppState>>,
    Json(req): Json<PtyOpenRequest>,
) -> Api<PtySession> {
    let host_id = req.host_id.clone();
    let Some(agent) = st.agent(&host_id).await else {
        return err(StatusCode::BAD_REQUEST, "host is offline");
    };
    let hostname = st
        .db
        .list_hosts()
        .unwrap_or_default()
        .into_iter()
        .find(|h| h.id == host_id)
        .map(|h| h.hostname)
        .unwrap_or_else(|| host_id.clone());

    let cols = req.cols.unwrap_or(80).max(1);
    let rows = req.rows.unwrap_or(24).max(1);
    let pty_id = gen_id("p", 10);
    let session = Arc::new(crate::state::PtySession::new(
        pty_id.clone(),
        host_id.clone(),
        hostname.clone(),
        cols,
        rows,
    ));
    st.ptys.write().await.insert(pty_id.clone(), session.clone());
    if !agent.send(HubMsg::PtyOpen {
        id: pty_id.clone(),
        term: Some("xterm-256color".into()),
        cols,
        rows,
    }) {
        st.ptys.write().await.remove(&pty_id);
        return err(StatusCode::INTERNAL_SERVER_ERROR, "agent send failed");
    }
    st.db.audit("pty.open", &host_id, &pty_id);
    Ok(Json(PtySession {
        pty_id,
        host_id,
        hostname,
        cols,
        rows,
        attached: false,
    }))
}

pub async fn pty(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let cols: u16 = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(80);
    let rows: u16 = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(24);

    if let Some(pty_id) = q.get("pty_id").filter(|s| !s.is_empty()).cloned() {
        return ws.on_upgrade(move |socket| async move {
            crate::pty::attach(st, socket, pty_id).await;
        });
    }

    // Legacy one-shot: create and attach in a single upgrade.
    let host_id = q.get("host_id").cloned().unwrap_or_default();
    if host_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error: "pty_id or host_id required".into() }))
            .into_response();
    }
    ws.on_upgrade(move |socket| async move {
        crate::pty::open_and_attach(st, socket, host_id, cols, rows).await;
    })
}

// ---------------------------------------------------------------------------
// install script + binary downloads
// ---------------------------------------------------------------------------

pub async fn install_sh(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let token = q.get("token").cloned().unwrap_or_default();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing token").into_response();
    }
    let base = public_base(&st, &headers);
    let script = include_str!("../../../dist/install.sh")
        .replace("__HUB__", &base)
        .replace("__TOKEN__", &token);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/x-shellscript"))],
        script,
    )
        .into_response()
}

pub async fn download(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    // Only allow the exact binary names we publish, no path traversal.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    }
    let path = std::path::Path::new(&st.cfg.bin_dir).join(&name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

pub async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

/// Fallback when no panel bundle is configured.
///
/// The hub and the panel deploy separately now, so "no static dir" is a normal
/// production shape, not a broken install. Say so instead of returning a bare
/// 404 that looks like a routing bug.
pub async fn no_panel() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "name": "hypha",
            "api_version": API_VERSION,
            "panel": null,
            "hint": concat!(
                "this hub serves the API only; serve the panel from nginx ",
                "(root -> mycelium/dist), or point static_dir at a built panel"
            ),
            "docs": "/api/meta",
        })),
    )
}
