//! JSON control messages.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Agent model
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Starting,
    Running,
    Exited,
    Failed,
    Lost,
}

impl AgentStatus {
    pub fn is_live(self) -> bool {
        matches!(self, AgentStatus::Starting | AgentStatus::Running)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Starting => "starting",
            AgentStatus::Running => "running",
            AgentStatus::Exited => "exited",
            AgentStatus::Failed => "failed",
            AgentStatus::Lost => "lost",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentInfo {
    pub id: u64,
    /// Path-shaped name, e.g. `research/claude-1`. The group is its prefix.
    pub name: String,
    pub kind: String,
    pub command: Vec<String>,
    pub cwd: String,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default)]
    pub exited_at: Option<u64>,
    #[serde(default)]
    pub holder_pid: Option<u32>,
    #[serde(default)]
    pub agent_pid: Option<u32>,
    pub status: AgentStatus,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Adapter state. Fixed to `unknown` until adapters exist (M3).
    #[serde(default = "unknown")]
    pub activity: String,
    #[serde(default)]
    pub attached: u32,
}

fn unknown() -> String {
    "unknown".into()
}

// ---------------------------------------------------------------------------
// client ↔ manager
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Request {
    Hello {
        version: u32,
    },
    Run(RunRequest),
    List {
        #[serde(default)]
        all: bool,
        #[serde(default)]
        prefix: Option<String>,
    },
    Kill {
        target: String,
        #[serde(default)]
        signal: Option<i32>,
    },
    Remove {
        target: String,
    },
    Shutdown {
        #[serde(default)]
        kill_agents: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RunRequest {
    /// Full argv; `command[0]` also determines the kind.
    pub command: Vec<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Group to create the agent in (`--in`, else `ARGUS_GROUP`).
    #[serde(default, rename = "in")]
    pub group: Option<String>,
    pub cwd: String,
    /// The client's environment, passed to the agent. Never persisted.
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Response {
    Hello { version: u32, pid: u32 },
    Agent { agent: AgentInfo },
    Agents { agents: Vec<AgentInfo> },
    Killed { ids: Vec<u64> },
    Ok,
    Error { code: String, message: String },
}

impl Response {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Response::Error { code: code.into(), message: message.into() }
    }
}

// ---------------------------------------------------------------------------
// manager ↔ holder
// ---------------------------------------------------------------------------

/// Written by the manager to the holder's stdin as one JSON document.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HolderSpec {
    pub id: u64,
    pub command: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub manager_socket: PathBuf,
}

/// Written by the holder to its stdout as one JSON line once it is serving.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum HolderReady {
    Ready { holder_pid: u32, agent_pid: u32 },
    Failed { message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SubscribeLevel {
    /// Lifecycle events only (`Exit`).
    Events,
    /// Events plus output bytes, for adapters.
    Output,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum HolderRequest {
    Hello { version: u32 },
    Info,
    Subscribe { level: SubscribeLevel },
    Signal { signal: i32 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum HolderResponse {
    Hello { version: u32, pid: u32 },
    Info(HolderInfo),
    Ok,
    Error { message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HolderInfo {
    pub id: u64,
    pub holder_pid: u32,
    pub agent_pid: u32,
    pub running: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub output_offset: u64,
}

/// `agents/<id>/exit.json`, written by the holder when the agent exits.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExitRecord {
    pub code: i32,
    pub exited_at: u64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
