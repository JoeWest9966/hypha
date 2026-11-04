//! Shell sessions. The pty lives on the agent; the hub owns the session record,
//! which is what lets a dropped WebSocket be a detach rather than a death.
//!
//! Three moving parts:
//!   * `open_and_attach` — legacy one-shot: create + attach, dies with the socket.
//!   * `attach` — reattach to a session that outlived its last client, replaying
//!     the scrollback first so the screen continues instead of going blank.
//!   * `reap` — close sessions nobody has come back for.

use crate::state::{AppState, PtySession};
use crate::util::gen_id;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use hypha_proto::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// One-shot shell: create a session and attach to it, then bin the session when
/// the socket closes. This is the shape the CLI's `hypha sh` uses — it has no
/// reconnect story and does not need one.
pub async fn open_and_attach(st: Arc<AppState>, socket: WebSocket, host_id: String, cols: u16, rows: u16) {
    let mut sink = socket;
    let Some(agent) = st.agent(&host_id).await else {
        let _ = send(
            &mut sink,
            &TermOut::Error {
                message: "host is offline".into(),
            },
        )
        .await;
        return;
    };

    let pty_id = gen_id("p", 10);
    let hostname = host_id.clone();
    let session = Arc::new(PtySession::new(
        pty_id.clone(),
        host_id.clone(),
        hostname,
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
        let _ = send(
            &mut sink,
            &TermOut::Error {
                message: "agent send failed".into(),
            },
        )
        .await;
        return;
    }

    run_attached(st.clone(), session.clone(), sink).await;
    // Transient sessions do not linger: whoever asked for this one is gone.
    agent.send(HubMsg::PtyClose { id: pty_id.clone() });
    st.ptys.write().await.remove(&pty_id);
}

/// Attach to an existing session.
pub async fn attach(st: Arc<AppState>, socket: WebSocket, pty_id: String) {
    let Some(session) = st.ptys.read().await.get(&pty_id).cloned() else {
        let mut sink = socket;
        let _ = send(
            &mut sink,
            &TermOut::Error {
                message: "unknown session".into(),
            },
        )
        .await;
        return;
    };
    run_attached(st, session, socket).await;
}

async fn run_attached(st: Arc<AppState>, session: Arc<PtySession>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<TermOut>();

    // Replay first: the screen the human left behind, then live bytes.
    {
        let s = session.scroll.lock().await;
        if s.truncated {
            let _ = send(
                &mut sink,
                &TermOut::Output {
                    data: b64(b"\r\n[earlier output truncated]\r\n"),
                },
            )
            .await;
        }
        if !s.buf.is_empty() {
            let data = b64(&s.buf);
            drop(s);
            if send(&mut sink, &TermOut::Output { data }).await.is_err() {
                return;
            }
        }
    }

    *session.sink.lock().await = Some(tx);

    let agent = st.agent(&session.host_id).await;
    let mut closed_by_client = false;

    loop {
        tokio::select! {
            from_session = rx.recv() => {
                match from_session {
                    Some(msg) => {
                        let done = matches!(msg, TermOut::Exit { .. });
                        if send(&mut sink, &msg).await.is_err() { break; }
                        if done { break; }
                    }
                    None => break,
                }
            }
            from_client = stream.next() => {
                match from_client {
                    Some(Ok(Message::Text(t))) => {
                        match serde_json::from_str::<TermIn>(&t) {
                            Ok(TermIn::Input { data }) => {
                                if let Some(a) = &agent {
                                    a.send(HubMsg::PtyInput {
                                        id: session.id.clone(),
                                        data: b64(data.as_bytes()),
                                    });
                                }
                            }
                            Ok(TermIn::Resize { cols, rows }) => {
                                session.cols.store(cols as u32, Ordering::Relaxed);
                                session.rows.store(rows as u32, Ordering::Relaxed);
                                if let Some(a) = &agent {
                                    a.send(HubMsg::PtyResize { id: session.id.clone(), cols, rows });
                                }
                            }
                            Ok(TermIn::Close) => {
                                closed_by_client = true;
                                break;
                            }
                            Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    *session.sink.lock().await = None;
    session.last_detach.store(hypha_proto::now(), Ordering::Relaxed);

    if closed_by_client || session.closed.load(Ordering::Relaxed) {
        if let Some(a) = st.agent(&session.host_id).await {
            a.send(HubMsg::PtyClose { id: session.id.clone() });
        }
        st.ptys.write().await.remove(&session.id);
    } else {
        // A dropped connection is a detach: keep the shell for a while.
        spawn_reaper(st.clone(), session.id.clone());
    }
    let _ = sink.send(Message::Close(None)).await;
}

/// Close a detached session once its TTL has expired.
fn spawn_reaper(st: Arc<AppState>, pty_id: String) {
    let ttl = st.cfg.pty_ttl_sec.max(30);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let Some(session) = st.ptys.read().await.get(&pty_id).cloned() else {
                return;
            };
            let age = hypha_proto::now() - session.last_detach.load(Ordering::Relaxed);
            let attached = !session.sink.lock().await.is_none();
            if attached {
                // Someone came back; the new attach owns the lifecycle.
                return;
            }
            if age < ttl as i64 && !session.closed.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(a) = st.agent(&session.host_id).await {
                a.send(HubMsg::PtyClose { id: pty_id.clone() });
            }
            st.ptys.write().await.remove(&pty_id);
            return;
        }
    });
}

async fn send<S>(sink: &mut S, msg: &TermOut) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
{
    let Ok(text) = serde_json::to_string(msg) else {
        return Err(());
    };
    sink.send(Message::Text(text.into())).await.map_err(|_| ())
}
