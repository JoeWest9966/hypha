//! Browser/CLI pty relay. The hub is a dumb pipe here: the pty lives on the
//! agent, survives the browser tab, and the hub just moves frames.

use crate::state::{AppState, PtyHandle};
use crate::util::gen_id;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use hypha_proto::*;
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn relay(st: Arc<AppState>, socket: WebSocket, host_id: String, cols: u16, rows: u16) {
    let (mut sink, mut stream) = socket.split();

    let Some(agent) = st.agent(&host_id).await else {
        let _ = send(&mut sink, &TermOut::Error {
            message: "host is offline".into(),
        })
        .await;
        return;
    };

    let pty_id = gen_id("p", 10);
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentMsg>();
    st.ptys.write().await.insert(
        pty_id.clone(),
        PtyHandle {
            host_id: host_id.clone(),
            tx,
        },
    );

    if !agent.send(HubMsg::PtyOpen {
        id: pty_id.clone(),
        term: Some("xterm-256color".into()),
        cols,
        rows,
    }) {
        st.ptys.write().await.remove(&pty_id);
        let _ = send(&mut sink, &TermOut::Error {
            message: "agent send failed".into(),
        })
        .await;
        return;
    }

    loop {
        tokio::select! {
            from_agent = rx.recv() => {
                match from_agent {
                    Some(AgentMsg::PtyChunk { data, .. }) => {
                        // Agent sends base64 (raw bytes); forward verbatim.
                        if send(&mut sink, &TermOut::Output { data }).await.is_err() { break; }
                    }
                    Some(AgentMsg::PtyExit { code, .. }) => {
                        let _ = send(&mut sink, &TermOut::Exit { code }).await;
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            from_client = stream.next() => {
                match from_client {
                    Some(Ok(Message::Text(t))) => {
                        match serde_json::from_str::<TermIn>(&t) {
                            Ok(TermIn::Input { data }) => {
                                agent.send(HubMsg::PtyInput {
                                    id: pty_id.clone(),
                                    data: b64(data.as_bytes()),
                                });
                            }
                            Ok(TermIn::Resize { cols, rows }) => {
                                agent.send(HubMsg::PtyResize { id: pty_id.clone(), cols, rows });
                            }
                            Ok(TermIn::Close) => {
                                agent.send(HubMsg::PtyClose { id: pty_id.clone() });
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

    agent.send(HubMsg::PtyClose { id: pty_id.clone() });
    st.ptys.write().await.remove(&pty_id);
    let _ = sink.send(Message::Close(None)).await;
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
