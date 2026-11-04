//! Job execution. One process per job, own session, streamed out in chunks.

use hypha_proto::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

pub struct Ctx {
    pub out: mpsc::UnboundedSender<AgentMsg>,
    /// job id -> process group id (== pid, because we setsid)
    pub jobs: Mutex<HashMap<String, i32>>,
    pub ptys: Mutex<HashMap<String, crate::term::Pty>>,
}

fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
}

pub async fn run(
    ctx: Arc<Ctx>,
    id: String,
    command: String,
    cwd: Option<String>,
    timeout_sec: Option<u64>,
) {
    let started = Instant::now();
    let mut cmd = Command::new(shell());
    cmd.arg("-lc").arg(&command);
    cmd.env("HYPHA_JOB", &id);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if let Some(d) = &cwd {
        cmd.current_dir(d);
    }
    #[cfg(unix)]
    unsafe {
        // Own session => kill(-pid) reaps the whole tree, not just the leader.
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            fail(&ctx, &id, format!("spawn failed: {e}"));
            return;
        }
    };

    let pid = child.id().unwrap_or(0) as i32;
    ctx.jobs.lock().unwrap().insert(id.clone(), pid);

    let _ = ctx.out.send(AgentMsg::ExecStarted {
        exec: id.clone(),
        pid: pid as u32,
    });

    let seq = Arc::new(AtomicU64::new(0));
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(spawn_pump(out, id.clone(), Stream::Stdout, seq.clone(), ctx.out.clone()));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(spawn_pump(err, id.clone(), Stream::Stderr, seq.clone(), ctx.out.clone()));
    }

    let mut timed_out = false;
    let status = match timeout_sec.filter(|t| *t > 0) {
        Some(t) => {
            tokio::select! {
                s = child.wait() => s.ok(),
                _ = tokio::time::sleep(Duration::from_secs(t)) => {
                    timed_out = true;
                    kill_group(pid, libc::SIGKILL);
                    child.wait().await.ok()
                }
            }
        }
        None => child.wait().await.ok(),
    };

    for p in pumps {
        let _ = tokio::time::timeout(Duration::from_secs(2), p).await;
    }
    ctx.jobs.lock().unwrap().remove(&id);

    if timed_out {
        let _ = ctx.out.send(AgentMsg::Error {
            id: id.clone(),
            message: format!("timed out after {}s", timeout_sec.unwrap_or(0)),
        });
    }

    let code = match &status {
        Some(s) => s.code(),
        None => None,
    };
    let _ = ctx.out.send(AgentMsg::ExecExit {
        exec: id,
        code: if timed_out { Some(124) } else { code },
        signal: status.as_ref().and_then(|s| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                s.signal()
            }
            #[cfg(not(unix))]
            {
                None
            }
        }),
        duration_ms: started.elapsed().as_millis() as u64,
    });
}

fn spawn_pump<R>(
    mut reader: R,
    job: String,
    stream: Stream,
    seq: Arc<AtomicU64>,
    out: mpsc::UnboundedSender<AgentMsg>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK_BYTES];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let frame = AgentMsg::ExecChunk {
                        exec: job.clone(),
                        seq: seq.fetch_add(1, Ordering::Relaxed),
                        stream,
                        data: b64(&buf[..n]),
                    };
                    if out.send(frame).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn fail(ctx: &Ctx, id: &str, message: String) {
    let _ = ctx.out.send(AgentMsg::Error {
        id: id.to_string(),
        message,
    });
    let _ = ctx.out.send(AgentMsg::ExecExit {
        exec: id.to_string(),
        code: Some(126),
        signal: None,
        duration_ms: 0,
    });
}

pub fn signal(ctx: &Ctx, id: &str, sig: Signal) {
    if let Some(pid) = ctx.jobs.lock().unwrap().get(id).copied() {
        kill_group(pid, sig.number());
    }
}

pub fn kill_all(ctx: &Ctx) {
    let pids: Vec<i32> = ctx.jobs.lock().unwrap().values().copied().collect();
    for pid in pids {
        kill_group(pid, libc::SIGKILL);
    }
}

/// Negative pid = the whole process group.
fn kill_group(pid: i32, sig: i32) {
    if pid > 0 {
        unsafe {
            libc::kill(-pid, sig);
        }
    }
}
