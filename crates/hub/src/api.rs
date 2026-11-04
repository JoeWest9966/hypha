//! HTTP API + SSE + static panel.

use crate::state::{AppState, ExecChannel};
use crate::util::{gen_id, verify_password};
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
// auth
// ---------------------------------------------------------------------------

pub fn token_of(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    if let Some(h) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = h.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.trim().to_string());
            }
        }
    }
    query.get("token").cloned().filter(|t| !t.is_empty())
}

pub fn require(state: &AppState, headers: &HeaderMap, query: &HashMap<String, String>) -> Result<(), ApiErr> {
    match token_of(headers, query) {
        Some(t) if state.db.valid_api_token(&t) => Ok(()),
        _ => err(StatusCode::UNAUTHORIZED, "unauthorized"),
    }
}

fn public_base(state: &AppState, headers: &HeaderMap) -> String {
    if !state.cfg.public_url.is_empty() {
        return state.cfg.public_url.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:5522")
        .to_string();
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
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
        "hosts_total": hosts.len(),
        "hosts_online": online,
    }))
}

pub async fn login(
    State(st): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Api<LoginResponse> {
    let hash = st.db.get_setting("password_hash").unwrap_or_default();
    if !verify_password(&req.password, &hash) {
        return err(StatusCode::UNAUTHORIZED, "wrong password");
    }
    let token = gen_id("t", 32);
    st.db.add_api_token(&token, "admin");
    st.db.audit("auth.login", "", "");
    Ok(Json(LoginResponse {
        token,
        hub: String::new(),
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
    };
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

pub async fn list_hosts(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Api<Vec<Host>> {
    require(&st, &headers, &q)?;
    let mut hosts = st.db.list_hosts().unwrap_or_default();
    let agents = st.agents.read().await;
    for h in hosts.iter_mut() {
        if let Some(a) = agents.get(&h.id) {
            h.online = true;
            h.load = a.load();
        }
    }
    Ok(Json(hosts))
}

pub async fn delete_host(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Api<serde_json::Value> {
    require(&st, &headers, &HashMap::new())?;
    if let Some(a) = st.agent(&id).await {
        a.send(HubMsg::Shutdown);
    }
    st.db.delete_host(&id).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn set_tags(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Api<serde_json::Value> {
    require(&st, &headers, &HashMap::new())?;
    let tags: Vec<String> = body
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    st.db.set_tags(&id, &tags);
    st.db.audit("host.tags", &id, &tags.join(","));
    Ok(Json(json!({ "ok": true })))
}

pub async fn create_enroll_token(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<EnrollTokenRequest>,
) -> Api<EnrollTokenResponse> {
    require(&st, &headers, &HashMap::new())?;
    let ttl = req.ttl_sec.unwrap_or(86400).clamp(60, 60 * 60 * 24 * 30);
    let token = gen_id("e", 24);
    let tags = req.tags.unwrap_or_default();
    st.db.create_enroll_token(&token, &tags, ttl);
    let base = public_base(&st, &headers);
    st.db.audit("enroll.token", "", &tags.join(","));
    Ok(Json(EnrollTokenResponse {
        token: token.clone(),
        expires_at: now() + ttl,
        install_cmd: format!("curl -fsSL {base}/install.sh?token={token} | sudo sh"),
    }))
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

pub async fn create_exec(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ExecRequest>,
) -> Api<ExecResponse> {
    require(&st, &headers, &HashMap::new())?;
    if req.command.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty command");
    }

    let hosts = st.db.list_hosts().unwrap_or_default();
    let online = st.agents.read().await;
    let mut picked: Vec<Host> = Vec::new();
    for h in hosts {
        let by_id = req.targets.hosts.iter().any(|x| x == &h.id);
        let by_tag = !req.targets.tags.is_empty()
            && h.tags.iter().any(|t| req.targets.tags.iter().any(|x| x == t));
        let by_all = req.targets.all || (req.targets.hosts.is_empty() && req.targets.tags.is_empty());
        if by_id || by_tag || by_all {
            picked.push(h);
        }
    }
    if picked.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no hosts matched");
    }

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
    let chan = Arc::new(ExecChannel::new(exec_id.clone(), total));
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
    headers: HeaderMap,
    Path(exec_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>, Response> {
    require(&st, &headers, &q).map_err(|e| e.into_response())?;
    let chan = st.execs.read().await.get(&exec_id).cloned().ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(ApiError { error: "unknown exec".into() })).into_response()
    })?;

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
                    yield Ok(Event::default().event("e").data(line));
                }
                continue;
            }
            if done {
                break;
            }
            idle += 1;
            if idle % 300 == 0 {
                yield Ok(Event::default().comment("ka"));
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn recent_execs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Api<Vec<serde_json::Value>> {
    require(&st, &headers, &q)?;
    Ok(Json(st.db.recent_execs(50)))
}

// ---------------------------------------------------------------------------
// pty
// ---------------------------------------------------------------------------

pub async fn pty(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(e) = require(&st, &headers, &q) {
        return e.into_response();
    }
    let host_id = q.get("host_id").cloned().unwrap_or_default();
    let cols: u16 = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(80);
    let rows: u16 = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(24);
    ws.on_upgrade(move |socket| async move {
        crate::pty::relay(st, socket, host_id, cols, rows).await;
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
