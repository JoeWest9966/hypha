//! Shared hub state: connected agents, in-flight execs, live pty relays.

use crate::db::Db;
use anyhow::Result;
use hypha_proto::{ExecEvent, HubMsg};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
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
}

pub struct PtyHandle {
    pub host_id: String,
    pub tx: mpsc::UnboundedSender<hypha_proto::AgentMsg>,
}

pub struct JobRoute {
    pub exec_id: String,
    pub host_id: String,
    pub hostname: String,
}

pub struct ExecLog {
    pub lines: Vec<String>,
    pub bytes: usize,
    pub done: bool,
}

/// One `run` invocation, fanned out across N hosts. Subscribers (SSE, CLI)
/// read from `log` by cursor and are woken by `notify`; output is therefore
/// replayable, which is what makes a dropped connection a nothing-event.
pub struct ExecChannel {
    #[allow(dead_code)]
    pub id: String,
    pub log: AsyncMutex<ExecLog>,
    pub notify: Notify,
    pub remaining: AsyncMutex<usize>,
}

impl ExecChannel {
    pub fn new(id: String, jobs: usize) -> Self {
        Self {
            id,
            log: AsyncMutex::new(ExecLog {
                lines: Vec::new(),
                bytes: 0,
                done: false,
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
    pub ptys: RwLock<HashMap<String, PtyHandle>>,
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
