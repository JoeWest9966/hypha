//! Shared hub state: connected agents, in-flight execs, live pty relays.

use crate::db::Db;
use anyhow::Result;
use hypha_proto::{ExecEvent, HostMetrics, HubMsg, TermOut};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    /// Public base URL used to build install commands, e.g. https://hub.example.com
    /// Empty = derive from the incoming request.
    pub public_url: String,
    pub data_dir: String,
    pub bin_dir: String,
    pub static_dir: String,
    /// Hard cap for a single exec, seconds. Agents get the same value.
    pub exec_timeout_sec: u64,
    /// How long a detached shell survives waiting for a reattach, seconds.
    pub pty_ttl_sec: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:5522".into(),
            public_url: String::new(),
            data_dir: "/var/lib/hypha".into(),
            bin_dir: "./bin".into(),
            static_dir: "./static".into(),
            exec_timeout_sec: 900,
            pty_ttl_sec: 600,
        }
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }
}

pub struct AgentHandle {
    pub tx: mpsc::UnboundedSender<HubMsg>,
    #[allow(dead_code)]
    pub hostname: String,
    /// Last reported 1-minute load average, as f32 bits.
    pub load: Arc<AtomicU32>,
    pub metrics: std::sync::Mutex<Option<HostMetrics>>,
}

impl AgentHandle {
    pub fn load(&self) -> f32 {
        f32::from_bits(self.load.load(Ordering::Relaxed))
    }
    pub fn set_load(&self, v: f32) {
        self.load.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn send(&self, msg: HubMsg) -> bool {
        self.tx.send(msg).is_ok()
    }
    pub fn set_metrics(&self, m: HostMetrics) {
        *self.metrics.lock().unwrap() = Some(m);
    }
    pub fn metrics(&self) -> Option<HostMetrics> {
        self.metrics.lock().unwrap().clone()
    }
}

/// One shell. It outlives any single WebSocket on purpose: a browser tab that
/// reloads, a laptop that sleeps, a wifi handoff — all of those detach the
/// client, and none of them should kill the shell you were in the middle of.
///
/// `scroll` is a bounded replay buffer of raw bytes, so a reattach continues
/// where the screen was instead of starting at a blank prompt.
pub struct PtySession {
    pub id: String,
    pub host_id: String,
    #[allow(dead_code)] // carried for future session listings; the API returns it at open time
    pub hostname: String,
    pub cols: AtomicU32,
    pub rows: AtomicU32,
    pub scroll: AsyncMutex<Scroll>,
    /// The currently attached client, if any.
    pub sink: AsyncMutex<Option<mpsc::UnboundedSender<TermOut>>>,
    /// Woken on output / exit so a waiting attach can flush.
    pub notify: Notify,
    /// Set once the shell exited; the session is then only a replay buffer.
    pub closed: AtomicBool,
    /// Unix seconds of the last detach, for the reaper.
    pub last_detach: AtomicI64,
}

pub struct Scroll {
    pub buf: Vec<u8>,
    /// True when old bytes were dropped off the front.
    pub truncated: bool,
}

impl PtySession {
    pub fn new(id: String, host_id: String, hostname: String, cols: u16, rows: u16) -> Self {
        Self {
            id,
            host_id,
            hostname,
            cols: AtomicU32::new(cols as u32),
            rows: AtomicU32::new(rows as u32),
            scroll: AsyncMutex::new(Scroll {
                buf: Vec::new(),
                truncated: false,
            }),
            sink: AsyncMutex::new(None),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            last_detach: AtomicI64::new(hypha_proto::now()),
        }
    }

    #[allow(dead_code)]
    pub fn attached(&self) -> bool {
        // Cheap peek used by the API listing; a racing detach just reports
        // "attached" for one extra frame, which nobody can observe.
        self.sink.try_lock().map(|g| g.is_some()).unwrap_or(true)
    }

    /// Append raw bytes, keeping at most `PTY_SCROLL_CAP` of them.
    pub async fn push_bytes(&self, data: &[u8]) {
        {
            let mut s = self.scroll.lock().await;
            s.buf.extend_from_slice(data);
            if s.buf.len() > PTY_SCROLL_CAP {
                let cut = s.buf.len() - PTY_SCROLL_CAP / 2;
                s.buf.drain(..cut);
                s.truncated = true;
            }
        }
        self.notify.notify_waiters();
    }
}

/// 256 KiB: several screens of ordinary output, and small enough that a fleet
/// of forgotten shells cannot eat the hub.
pub const PTY_SCROLL_CAP: usize = 256 * 1024;

pub struct JobRoute {
    pub exec_id: String,
    pub host_id: String,
    pub hostname: String,
}

pub struct ExecLog {
    pub lines: Vec<String>,
    pub bytes: usize,
    pub done: bool,
    /// Monotonic frame counter for the persistent copy. It must not follow
    /// `lines.len()`, which shrinks when the in-memory buffer is trimmed.
    pub next_seq: u64,
}

/// One `run` invocation, fanned out across N hosts. Subscribers (SSE, CLI)
/// read from `log` by cursor and are woken by `notify`; output is therefore
/// replayable, which is what makes a dropped connection a nothing-event.
pub struct ExecChannel {
    #[allow(dead_code)]
    pub id: String,
    /// Where finished frames go to survive a restart.
    pub db: Db,
    pub log: AsyncMutex<ExecLog>,
    pub notify: Notify,
    pub remaining: AsyncMutex<usize>,
}

impl ExecChannel {
    pub fn new(id: String, jobs: usize, db: Db) -> Self {
        Self {
            id,
            db,
            log: AsyncMutex::new(ExecLog {
                lines: Vec::new(),
                bytes: 0,
                done: false,
                next_seq: 0,
            }),
            notify: Notify::new(),
            remaining: AsyncMutex::new(jobs),
        }
    }

    pub async fn push(&self, ev: &ExecEvent) {
        let line = match serde_json::to_string(ev) {
            Ok(l) => l,
            Err(_) => return,
        };
        {
            let mut log = self.log.lock().await;
            if log.done {
                return;
            }
            log.bytes += line.len();
            // Persist before dropping: the replay buffer is bounded, history is not.
            self.db.append_chunk(&self.id, log.next_seq, &line);
            log.next_seq += 1;
            log.lines.push(line);
            // Keep a bounded replay buffer: an exec that spits a gigabyte
            // should not OOM the hub. Oldest frames are dropped first; the
            // live stream never truncates.
            while log.bytes > 8 * 1024 * 1024 && log.lines.len() > 1 {
                let dropped = log.lines.remove(0);
                log.bytes -= dropped.len();
            }
        }
        self.notify.notify_waiters();
    }

    /// Mark one job finished. When the last one lands, emit `Done` and close.
    pub async fn job_done(&self) {
        let last = {
            let mut rem = self.remaining.lock().await;
            if *rem > 0 {
                *rem -= 1;
            }
            *rem == 0
        };
        if last {
            self.push(&ExecEvent::Done).await;
            self.log.lock().await.done = true;
            self.notify.notify_waiters();
        }
    }

}

pub struct AppState {
    pub db: Db,
    pub cfg: Config,
    #[allow(dead_code)]
    pub started_at: i64,
    pub agents: RwLock<HashMap<String, Arc<AgentHandle>>>,
    pub jobs: RwLock<HashMap<String, JobRoute>>,
    pub execs: RwLock<HashMap<String, Arc<ExecChannel>>>,
    /// In-flight request/reply round trips with an agent (file writes). Keyed by
    /// the frame id the agent echoes back.
    pub waiters: RwLock<HashMap<String, tokio::sync::oneshot::Sender<hypha_proto::AgentMsg>>>,
    pub ptys: RwLock<HashMap<String, Arc<PtySession>>>,
}

impl AppState {
    pub fn new(cfg: Config, db: Db) -> Arc<Self> {
        Arc::new(Self {
            db,
            cfg,
            started_at: hypha_proto::now(),
            agents: RwLock::new(HashMap::new()),
            jobs: RwLock::new(HashMap::new()),
            execs: RwLock::new(HashMap::new()),
            waiters: RwLock::new(HashMap::new()),
            ptys: RwLock::new(HashMap::new()),
        })
    }

    pub async fn agent(&self, host_id: &str) -> Option<Arc<AgentHandle>> {
        self.agents.read().await.get(host_id).cloned()
    }

    pub async fn online_ids(&self) -> Vec<String> {
        self.agents.read().await.keys().cloned().collect()
    }

}
