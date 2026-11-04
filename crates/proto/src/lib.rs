//! hypha wire protocol.
//!
//! Two distinct planes, deliberately:
//!   * agent <-> hub  : one WebSocket, JSON frames, outbound-only from the agent.
//!   * cli/panel <-> hub : HTTP + SSE (exec) and WebSocket (pty).
//!
//! The agent link is stateful and resumable; output frames carry a monotonic
//! `seq` per stream so a reconnecting client can ask for a replay from the last
//! frame it saw instead of re-running anything.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Output chunk size emitted by the agent. Small enough to interleave, big
/// enough that a `make -j8` doesn't frame us to death.
pub const CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    Int,
    Term,
    Kill,
    Hup,
}

impl Signal {
    pub fn as_str(&self) -> &'static str {
        match self {
            Signal::Int => "INT",
            Signal::Term => "TERM",
            Signal::Kill => "KILL",
            Signal::Hup => "HUP",
        }
    }

    pub fn number(&self) -> i32 {
        match self {
            Signal::Hup => 1,
            Signal::Int => 2,
            Signal::Kill => 9,
            Signal::Term => 15,
        }
    }
}

/// Agent -> hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    /// Answer to `Challenge`. `sig` is hex(ed25519(nonce)) or hex(ed25519("join:"+nonce)).
    Join {
        protocol: u32,
        host_id: String,
        sig: String,
        version: String,
        hostname: String,
        os: String,
        arch: String,
    },
    ExecStarted {
        exec: String,
        pid: u32,
    },
    ExecChunk {
        exec: String,
        seq: u64,
        stream: Stream,
        /// base64
        data: String,
    },
    ExecExit {
        exec: String,
        code: Option<i32>,
        signal: Option<i32>,
        duration_ms: u64,
    },
    PtyChunk {
        pty: String,
        seq: u64,
        /// base64
        data: String,
    },
    PtyExit {
        pty: String,
        code: Option<i32>,
    },
    /// Periodic housekeeping from the agent: load, active sessions.
    Stat {
        load: f32,
        procs: u32,
    },
    Pong,
    Error {
        id: String,
        message: String,
    },
}

/// Hub -> agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HubMsg {
    Challenge {
        nonce: String,
    },
    Welcome {
        hub: String,
        version: String,
    },
    Rejected {
        reason: String,
    },
    Exec {
        id: String,
        command: String,
        cwd: Option<String>,
        timeout_sec: Option<u64>,
    },
    ExecSignal {
        id: String,
        signal: Signal,
    },
    PtyOpen {
        id: String,
        term: Option<String>,
        cols: u16,
        rows: u16,
    },
    PtyInput {
        id: String,
        /// base64
        data: String,
    },
    PtyResize {
        id: String,
        cols: u16,
        rows: u16,
    },
    PtyClose {
        id: String,
    },
    Ping,
    /// Ask the agent to drop the socket (hub shutting down / agent revoked).
    Shutdown,
}

// ---------------------------------------------------------------------------
// enrollment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    /// hex ed25519 public key
    pub pubkey: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub host_id: String,
    pub hostname: String,
    pub hub: String,
}

// ---------------------------------------------------------------------------
// cli / panel API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub last_seen: i64,
    pub online: bool,
    #[serde(default)]
    pub load: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub hub: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTokenRequest {
    pub ttl_sec: Option<i64>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTokenResponse {
    pub token: String,
    pub expires_at: i64,
    pub install_cmd: String,
}

/// Select hosts by id, by tag, or everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Targets {
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    #[serde(default)]
    pub targets: Targets,
    pub command: String,
    pub timeout_sec: Option<u64>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub exec_id: String,
    pub jobs: Vec<JobRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRef {
    pub host_id: String,
    pub hostname: String,
    pub job_id: String,
}

/// One line of an exec stream, delivered over SSE.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecEvent {
    Started {
        job_id: String,
        host_id: String,
        hostname: String,
        pid: u32,
    },
    Output {
        job_id: String,
        host_id: String,
        hostname: String,
        seq: u64,
        stream: Stream,
        data: String,
    },
    Exit {
        job_id: String,
        host_id: String,
        hostname: String,
        code: Option<i32>,
        signal: Option<i32>,
        duration_ms: u64,
    },
    Failed {
        job_id: String,
        host_id: String,
        hostname: String,
        message: String,
    },
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// ---------------------------------------------------------------------------
// pty frames (cli/panel <-> hub websocket)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TermIn {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TermOut {
    Output { data: String },
    Exit { code: Option<i32> },
    Error { message: String },
}

// ---------------------------------------------------------------------------

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn b64d(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
