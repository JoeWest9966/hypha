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
    /// Periodic housekeeping from the agent.
    ///
    /// Everything past `procs` is `#[serde(default)]` so a hub and an agent of
    /// different vintages still talk: an old agent sends `{load, procs}` and the
    /// new fields read as zero, which is exactly what "no data" looks like.
    Stat {
        load: f32,
        procs: u32,
        #[serde(default)]
        cpu_pct: f32,
        #[serde(default)]
        mem_used: u64,
        #[serde(default)]
        mem_total: u64,
        #[serde(default)]
        disk_used: u64,
        #[serde(default)]
        disk_total: u64,
        #[serde(default)]
        uptime_sec: u64,
    },
    /// Answer to `FileWrite`. `error` is present only when `ok` is false.
    FileResult {
        id: String,
        ok: bool,
        bytes: u64,
        #[serde(default)]
        error: Option<String>,
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
    /// Write `data` (base64) to `path`, atomically: the agent writes a temp
    /// file next to the target and renames it, so a full disk or a killed agent
    /// never leaves a half-written file where the old one used to be.
    FileWrite {
        id: String,
        path: String,
        /// Unix mode, decimal. `0` means "use 0644".
        mode: u32,
        /// base64
        data: String,
    },
    Ping,
    /// Ask the agent to drop the socket (hub shutting down / agent revoked).
    Shutdown,
}

/// Latest resource report from an agent. Serialized straight into the host
/// list, so this shape is the API's shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostMetrics {
    /// Busy fraction over the last reporting interval, 0-100.
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub procs: u32,
    pub uptime_sec: u64,
    pub collected_at: i64,
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
    /// Absent until the agent's first report, and absent again for any host
    /// that is offline: metrics are live state, never history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<HostMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTokenRequest {
    pub ttl_sec: Option<i64>,
    pub tags: Option<Vec<String>>,
    /// Reusable tokens are not burned on use. Keep them somewhere safe: they
    /// are a permanent-ish key to enrolling hosts in this fleet.
    #[serde(default)]
    pub reusable: bool,
    /// Human-readable label so you can tell your keys apart later.
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTokenResponse {
    pub token: String,
    pub expires_at: i64,
    pub install_cmd: String,
    pub reusable: bool,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTokenInfo {
    pub token: String,
    pub label: String,
    pub tags: Vec<String>,
    pub reusable: bool,
    pub revoked: bool,
    pub created_at: i64,
    pub expires_at: i64,
    pub uses: i64,
    pub last_used_at: Option<i64>,
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

/// Version of the HTTP contract in `api/openapi.yaml`.
///
/// Bump on any breaking change (field removed/renamed, status code semantics,
/// stream framing). Additive fields do not bump it: consumers ignore unknown
/// fields, and a panel that refuses to run against a version it does not know
/// is strictly better than one that half-works.
pub const API_VERSION: u32 = 1;

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// files / pty sessions (API surface)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePushRequest {
    pub targets: Targets,
    pub path: String,
    /// Decimal unix mode; 0 or absent means 0644.
    #[serde(default)]
    pub mode: Option<u32>,
    /// base64 of the file bytes.
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePushResponse {
    pub results: Vec<FilePushResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePushResult {
    pub host_id: String,
    pub hostname: String,
    pub ok: bool,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyOpenRequest {
    pub host_id: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtySession {
    pub pty_id: String,
    pub host_id: String,
    pub hostname: String,
    pub cols: u16,
    pub rows: u16,
    pub attached: bool,
}
