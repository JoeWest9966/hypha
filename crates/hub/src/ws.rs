//! The agent link: one outbound WebSocket per host, authenticated with an
//! ed25519 challenge. Everything the hub can do to a machine goes through here.

use crate::state::{AgentHandle, AppState};
use crate::util::gen_id;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use hypha_proto::*;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn upgrade(State(st): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_FRAME_BYTES * 4)
        .on_upgrade(move |socket| handle(st, socket))
}

fn to_text(msg: &HubMsg) -> Option<Message> {
    serde_json::to_string(msg).ok().map(|s| Message::Text(s.into()))
}

async fn handle(st: Arc<AppState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // --- challenge / response ------------------------------------------------
    let nonce = gen_id("n", 24);
    if sink
        .send(Message::Text(
            serde_json::to_string(&HubMsg::Challenge { nonce: nonce.clone() })
                .unwrap()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    let first = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<AgentMsg>(&t).ok(),
        _ => return,
    };

    let Some(AgentMsg::Join {
        protocol,
        host_id,
        sig,
        version,
        hostname,
        ..
    }) = first
    else {
        let _ = sink
            .send(Message::Text(
                serde_json::to_string(&HubMsg::Rejected {
                    reason: "expected join".into(),
                })
                .unwrap()
                .into(),
            ))
            .await;
        return;
    };

    if protocol != PROTOCOL_VERSION {
        let _ = sink
            .send(Message::Text(
                serde_json::to_string(&HubMsg::Rejected {
                    reason: format!("protocol {protocol} != {PROTOCOL_VERSION}"),
                })
                .unwrap()
                .into(),
            ))
            .await;
        return;
    }

    let ok = verify_join(&st, &host_id, &nonce, &sig);
    if !ok {
        tracing::warn!(host_id = %host_id, "agent auth failed");
        let _ = sink
            .send(Message::Text(
                serde_json::to_string(&HubMsg::Rejected {
                    reason: "bad signature".into(),
                })
                .unwrap()
                .into(),
            ))
            .await;
        return;
    }

    st.db.touch_host(&host_id, &hostname, &version);
    let _ = sink
        .send(Message::Text(
            serde_json::to_string(&HubMsg::Welcome {
                hub: st.cfg.public_url.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
            .unwrap()
            .into(),
        ))
        .await;

    // --- live loop -----------------------------------------------------------
    let (tx, mut rx) = mpsc::unbounded_channel::<HubMsg>();
    let handle = Arc::new(AgentHandle {
        metrics: Default::default(),
        tx,
        hostname: hostname.clone(),
        load: Arc::new(AtomicU32::new(0f32.to_bits())),
    });
    st.agents.write().await.insert(host_id.clone(), handle.clone());
    tracing::info!(host = %hostname, id = %host_id, "agent online");

    let mut ping = tokio::time::interval(Duration::from_secs(20));
    ping.tick().await;

    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                match outgoing {
                    Some(msg) => {
                        let is_shutdown = matches!(msg, HubMsg::Shutdown);
                        match to_text(&msg) {
                            Some(m) => if sink.send(m).await.is_err() { break; },
                            None => {}
                        }
                        if is_shutdown { break; }
                    }
                    None => break,
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(msg) = serde_json::from_str::<AgentMsg>(&t) {
                            if !on_agent_msg(&st, &host_id, &handle, msg).await { break; }
                        }
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if sink.send(to_text(&HubMsg::Ping).unwrap()).await.is_err() { break; }
            }
        }
    }

    st.agents.write().await.remove(&host_id);
    tracing::info!(host = %hostname, "agent offline");
    fail_orphans(&st, &host_id).await;
}

fn verify_join(st: &AppState, host_id: &str, nonce: &str, sig_hex: &str) -> bool {
    let Some(pk) = st.db.pubkey_of(host_id) else {
        return false;
    };
    let (Ok(pk), Ok(sig)) = (hex::decode(&pk), hex::decode(sig_hex)) else {
        return false;
    };
    let Ok(pk) = <[u8; 32]>::try_from(pk.as_slice()) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else {
        return false;
    };
    match VerifyingKey::from_bytes(&pk) {
        Ok(vk) => vk.verify_strict(nonce.as_bytes(), &Signature::from_bytes(&sig)).is_ok(),
        Err(_) => false,
    }
}

/// Returns false if the connection should be dropped.
async fn on_agent_msg(
    st: &Arc<AppState>,
    host_id: &str,
    agent: &Arc<AgentHandle>,
    msg: AgentMsg,
) -> bool {
    match msg {
        AgentMsg::Pong => true,
        AgentMsg::Stat {
            load,
            procs,
            cpu_pct,
            mem_used,
            mem_total,
            disk_used,
            disk_total,
            uptime_sec,
        } => {
            agent.set_load(load);
            agent.set_metrics(HostMetrics {
                cpu_pct,
                mem_used,
                mem_total,
                disk_used,
                disk_total,
                procs,
                uptime_sec,
                collected_at: hypha_proto::now(),
            });
            true
        }
        AgentMsg::ExecStarted { exec, pid } => {
            if let Some(ev) = routed(st, &exec, |r| ExecEvent::Started {
                job_id: exec.clone(),
                host_id: r.host_id.clone(),
                hostname: r.hostname.clone(),
                pid,
            })
            .await
            {
                push_event(st, &exec, ev).await;
            }
            true
        }
        AgentMsg::ExecChunk {
            exec,
            seq,
            stream,
            data,
        } => {
            if let Some(ev) = routed(st, &exec, |r| ExecEvent::Output {
                job_id: exec.clone(),
                host_id: r.host_id.clone(),
                hostname: r.hostname.clone(),
                seq,
                stream,
                data,
            })
            .await
            {
                push_event(st, &exec, ev).await;
            }
            true
        }
        AgentMsg::ExecExit {
            exec,
            code,
            signal,
            duration_ms,
        } => {
            if let Some(ev) = routed(st, &exec, |r| ExecEvent::Exit {
                job_id: exec.clone(),
                host_id: r.host_id.clone(),
                hostname: r.hostname.clone(),
                code,
                signal,
                duration_ms,
            })
            .await
            {
                let exec_id = st.jobs.write().await.remove(&exec).map(|r| r.exec_id);
                if let Some(eid) = exec_id {
                    if let Some(chan) = st.execs.read().await.get(&eid).cloned() {
                        chan.push(&ev).await;
                        chan.job_done().await;
                    }
                }
            }
            true
        }
        AgentMsg::PtyChunk { pty, data, .. } => {
            // Always into the scrollback, whether or not a client is attached:
            // that buffer is what makes reattach continuous.
            if let Some(session) = st.ptys.read().await.get(&pty).cloned() {
                if let Some(bytes) = b64_decode(&data) {
                    session.push_bytes(&bytes).await;
                }
                let sink = session.sink.lock().await.clone();
                if let Some(tx) = sink {
                    let _ = tx.send(TermOut::Output { data });
                }
            }
            true
        }
        AgentMsg::PtyExit { pty, code } => {
            if let Some(session) = st.ptys.read().await.get(&pty).cloned() {
                session.closed.store(true, std::sync::atomic::Ordering::Relaxed);
                let sink = session.sink.lock().await.clone();
                if let Some(tx) = sink {
                    let _ = tx.send(TermOut::Exit { code });
                }
            }
            true
        }
        AgentMsg::FileResult {
            id,
            ok,
            bytes,
            error,
        } => {
            if let Some(tx) = st.waiters.write().await.remove(&id) {
                let _ = tx.send(AgentMsg::FileResult {
                    id,
                    ok,
                    bytes,
                    error,
                });
            }
            true
        }
        AgentMsg::Error { id, message } => {
            tracing::warn!(host = %host_id, id = %id, "agent error: {message}");
            true
        }
        AgentMsg::Join { .. } => true,
    }
}

/// Build an event from the job route, if the job is still known.
async fn routed<F>(st: &Arc<AppState>, job_id: &str, f: F) -> Option<ExecEvent>
where
    F: FnOnce(&crate::state::JobRoute) -> ExecEvent,
{
    let jobs = st.jobs.read().await;
    jobs.get(job_id).map(f)
}

async fn push_event(st: &Arc<AppState>, job_id: &str, ev: ExecEvent) {
    let exec_id = {
        let jobs = st.jobs.read().await;
        jobs.get(job_id).map(|r| r.exec_id.clone())
    };
    if let Some(eid) = exec_id {
        if let Some(chan) = st.execs.read().await.get(&eid).cloned() {
            chan.push(&ev).await;
        }
    }
}

/// A host vanished mid-flight: fail every job it was running and unblock the
/// exec watchers, otherwise a `hypha run` would hang forever.
async fn fail_orphans(st: &Arc<AppState>, host_id: &str) {
    let orphans: Vec<(String, crate::state::JobRoute)> = {
        let mut jobs = st.jobs.write().await;
        let ids: Vec<String> = jobs
            .iter()
            .filter(|(_, r)| r.host_id == host_id)
            .map(|(k, _)| k.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| jobs.remove(&id).map(|r| (id, r)))
            .collect()
    };

    for (job_id, route) in orphans {
        if let Some(chan) = st.execs.read().await.get(&route.exec_id).cloned() {
            chan.push(&ExecEvent::Failed {
                job_id,
                host_id: route.host_id,
                hostname: route.hostname,
                message: "host disconnected".into(),
            })
            .await;
            chan.job_done().await;
        }
    }

    // Shells on a dead host cannot be reattached, so drop them outright.
    let orphans: Vec<Arc<crate::state::PtySession>> = st
        .ptys
        .read()
        .await
        .values()
        .filter(|p| p.host_id == host_id)
        .cloned()
        .collect();
    for p in orphans {
        st.ptys.write().await.remove(&p.id);
    }
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}
