//! JSON control messages.

use std::collections::BTreeMap;
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
    /// What the agent is doing, as understood by its driver: `working`,
    /// `tool:<name>`, `blocked`, `done`, `idle`, `error`,
    /// `unknown`; `busy` / `quiet` for agents without hooks.
    #[serde(default = "unknown")]
    pub activity: String,
    /// Unix seconds when `activity` last changed.
    #[serde(default)]
    pub activity_since: Option<u64>,
    #[serde(default)]
    pub attached: u32,
    /// Live tmux attachments, rebuilt from the holder after a manager restart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmux_locations: Vec<TmuxLocation>,
    /// Free-form `key=value` tags, orthogonal to the group path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TmuxLocation {
    /// The tmux server socket; pane IDs are only unique within one server.
    pub socket: String,
    pub pane: String,
}

fn unknown() -> String {
    "unknown".into()
}

// ---------------------------------------------------------------------------
// client ↔ manager
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ScreenRestore,
    StyledPreview,
    OffsetReplay,
}

pub const MANAGER_CAPABILITIES: &[Capability] = &[Capability::ScreenRestore, Capability::StyledPreview];
pub const HOLDER_CAPABILITIES: &[Capability] = &[Capability::OffsetReplay];

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Request {
    Hello {
        version: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<Capability>,
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
    /// Removes every exited agent, optionally only those that ended more than
    /// `older_than` seconds ago and/or under a group prefix.
    Prune {
        #[serde(default)]
        older_than: Option<u64>,
        #[serde(default)]
        prefix: Option<String>,
    },
    Shutdown {
        #[serde(default)]
        kill_agents: bool,
    },
    /// Types `text` into the agent without attaching, then presses Enter if
    /// `enter` is set. Refused with code `not_ready` unless the agent is
    /// waiting for a prompt (`idle` or `done`), nobody attached has typed
    /// recently, and no earlier send is still being submitted. `force` skips
    /// those checks, except that a `blocked` agent is always refused: a
    /// keypress there answers its permission prompt.
    Send {
        target: String,
        text: String,
        #[serde(default)]
        enter: bool,
        #[serde(default)]
        force: bool,
    },
    Rename {
        target: String,
        name: String,
    },
    Label {
        target: String,
        #[serde(default)]
        set: BTreeMap<String, String>,
        #[serde(default)]
        unset: Vec<String>,
    },
    /// Long-lived: `Snapshot`, then an `Event` whenever something changes.
    Watch {
        #[serde(default)]
        ids: Option<Vec<u64>>,
        #[serde(default)]
        include_exited: bool,
    },
    /// Sent by `argus-hook` on behalf of an agent. Never answered.
    Report {
        agent_id: u64,
        /// The hook's origin (`claude`, `codex`); must match the agent's kind.
        source: String,
        /// The hook payload exactly as the agent wrote it.
        event: serde_json::Value,
    },
    /// Marks a finished agent as seen: `done` → `idle`.
    Ack {
        target: String,
    },
    /// The manager's own idea of the agent's current screen, for `attach` to
    /// restore without depending on the agent redrawing itself. `since_offset`
    /// lets a caller that already has the screen at that offset skip the
    /// bytes.
    Screen {
        target: String,
        #[serde(default)]
        since_offset: Option<u64>,
    },
    /// A styled crop of the agent's current screen to `rows`x`cols`, for
    /// dashboard thumbnails. Unlike `Screen`, it contains structured spans,
    /// not escape codes or absolute cursor positions, so it can be safely
    /// placed inside a caller-drawn layout.
    ScreenPreview {
        target: String,
        rows: u16,
        cols: u16,
    },
    /// A one-shot escape-code rendering of the agent's current screen,
    /// alternate or primary, for `argus logs --screen`. Unlike `Screen`,
    /// this is a plain readback, not an attach-restore hint: it never
    /// signals entering the alternate screen.
    ScreenDump {
        target: String,
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
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Overrides kind detection from the program name (`--kind`).
    #[serde(default)]
    pub kind: Option<String>,
    /// The client terminal's colours, for the holder to answer the agent's
    /// colour queries with until a terminal attaches.
    #[serde(default)]
    pub colors: TerminalColors,
}

/// Default foreground and background of a terminal, as the bodies of its
/// OSC 10 / OSC 11 replies (e.g. `rgb:1e1e/1e1e/2e2e`). `None` when the
/// terminal did not answer.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
}

impl TerminalColors {
    /// Whether `spec` is a plain X11 colour spec (`rgb:…`, `#…`), safe to
    /// write into an agent's input inside an OSC reply.
    pub fn is_color_spec(spec: &str) -> bool {
        (1..=64).contains(&spec.len())
            && (spec.starts_with("rgb:") || spec.starts_with('#'))
            && spec.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'/' | b'#'))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Response {
    Hello {
        version: u32,
        pid: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<Capability>,
        /// [`crate::BUILD`] of the replying process; absent from builds
        /// that predate it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
    },
    Agent {
        agent: AgentInfo,
        /// Problems worth telling the user about, e.g. activity tracking
        /// being unavailable for this agent.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    Agents {
        agents: Vec<AgentInfo>,
    },
    Killed {
        ids: Vec<u64>,
    },
    Pruned {
        agents: Vec<AgentInfo>,
    },
    Ok,
    Error {
        code: String,
        message: String,
    },
    /// First message of a watch. `epoch` changes when the manager restarts.
    Snapshot {
        epoch: u64,
        seq: u64,
        agents: Vec<AgentInfo>,
    },
    Event {
        epoch: u64,
        seq: u64,
        event: AgentEvent,
    },
    /// Reply to `Screen`. `offset` is the output offset the screen
    /// corresponds to; a caller restoring a live attach passes it to the
    /// holder as `AttachRequest.from_offset` to pick up from exactly there.
    Screen {
        mode: ScreenMode,
        rows: u16,
        cols: u16,
        offset: u64,
        /// Empty when the caller's `since_offset` already matches, or when
        /// `mode` is `Replay` (the bytes live in the holder's ring buffer,
        /// fetched separately).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bytes: Vec<u8>,
    },
    /// Reply to `ScreenPreview`. Empty `lines` means nothing is tracked for
    /// this agent yet (just started, or the manager lost its holder
    /// connection); the caller shows the box empty rather than erroring.
    ScreenPreview {
        lines: Vec<PreviewLine>,
    },
    /// Reply to `ScreenDump`.
    ScreenDump {
        rows: u16,
        cols: u16,
        bytes: Vec<u8>,
    },
}

/// One styled terminal row used by `argus grid` thumbnails.
pub type PreviewLine = Vec<PreviewSpan>;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreviewSpan {
    pub text: String,
    pub fg: PreviewColor,
    pub bg: PreviewColor,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub inverse: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum PreviewColor {
    Default,
    Indexed(u8),
    Rgb([u8; 3]),
}

/// How a client should restore an agent's screen.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScreenMode {
    /// The agent is in the alternate screen: `bytes` is a manager-rendered
    /// redraw of the current screen and terminal modes. Apply it only if the
    /// caller's terminal size matches `rows`/`cols`.
    Snapshot,
    /// The agent has never entered the alternate screen: recovering earlier
    /// output means replaying the holder's ring buffer from `offset`
    /// (usually its oldest retained byte) instead of a synthetic redraw.
    Replay,
    /// The manager has no screen state for this agent yet (just started) or
    /// isn't reachable; the caller should fall back to the first-stage
    /// clear-and-resize dance.
    Unavailable,
}

/// Watch events carry the agent's whole current record, so a client simply
/// replaces its row. Changes to one agent within ~100 ms are coalesced.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum AgentEvent {
    Created {
        agent: AgentInfo,
    },
    /// Any change to a live agent: status, activity, attach count, name, labels.
    Updated {
        agent: AgentInfo,
    },
    Exited {
        agent: AgentInfo,
    },
    Removed {
        id: u64,
    },
    /// The watcher fell behind; a fresh `Snapshot` follows.
    Resync,
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
    /// From [`RunRequest::colors`].
    #[serde(default)]
    pub colors: TerminalColors,
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
    /// Events plus output bytes, for screen snapshots and logs.
    Output,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum HolderRequest {
    Hello {
        version: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<Capability>,
    },
    Info,
    /// With `from_offset`, output subscribers first get the ring buffer from
    /// that offset (or its oldest byte), then live output.
    Subscribe {
        level: SubscribeLevel,
        #[serde(default)]
        from_offset: Option<u64>,
    },
    Signal {
        signal: i32,
    },
    /// Input written to the agent as if typed.
    Write {
        text: String,
    },
    /// Switches the connection to stream mode after `Ok`.
    Attach(AttachRequest),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AttachRequest {
    pub rows: u16,
    pub cols: u16,
    /// Never forwards input and never decides the PTY size.
    #[serde(default)]
    pub readonly: bool,
    /// Disconnects every other attached client first.
    #[serde(default)]
    pub steal: bool,
    /// Sends the ring buffer before live output.
    #[serde(default)]
    pub replay: bool,
    /// Allows historical OSC 52 sequences to modify the attaching terminal's
    /// clipboard. Live output is always passed through unchanged.
    #[serde(default)]
    pub allow_clipboard_replay: bool,
    /// Sends the ring buffer from this offset (or its oldest byte) before
    /// live output, instead of the whole thing. Takes priority over `replay`
    /// when both are set. Used to pick up right after a manager-provided
    /// screen snapshot or replay.
    #[serde(default)]
    pub from_offset: Option<u64>,
    /// The attaching terminal's colours; the holder answers the agent's
    /// colour queries with them once this terminal is gone again.
    #[serde(default)]
    pub colors: TerminalColors,
    /// Supplied by the attaching terminal, if it is running inside tmux.
    #[serde(default)]
    pub tmux: Option<TmuxLocation>,
}

/// Pushed by the holder to subscribers as control frames.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum HolderEvent {
    /// Attached terminals, how many have focus, and where tmux ones live.
    Attached {
        count: u32,
        #[serde(default)]
        focused: u32,
        #[serde(default)]
        tmux_locations: Vec<TmuxLocation>,
    },
    /// Someone typed into the agent. At most one per second.
    Input,
    /// The PTY's size actually changed. Not sent for a jiggle (resize and
    /// back) that leaves the size unchanged, since nothing to track moved.
    Resized { rows: u16, cols: u16 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum HolderResponse {
    Hello {
        version: u32,
        pid: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<Capability>,
        /// [`crate::BUILD`] of the replying process; absent from builds
        /// that predate it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
    },
    Info(HolderInfo),
    Ok,
    Error {
        message: String,
    },
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
    #[serde(default)]
    pub attached: u32,
}

/// `agents/<id>/exit.json`, written by the holder when the agent exits.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExitRecord {
    pub code: i32,
    pub exited_at: u64,
}

/// Whether an agent with this activity is waiting for a prompt, so typing
/// one (`argus send`) is safe. `done` counts: it is `idle` that nobody has
/// looked at yet.
pub fn awaits_prompt(activity: &str) -> bool {
    matches!(activity, "idle" | "done")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn hello_capabilities_are_optional_for_older_peers() {
        let request: Request = serde_json::from_str(r#"{"type":"Hello","version":1}"#).unwrap();
        assert!(matches!(request, Request::Hello { capabilities, .. } if capabilities.is_empty()));

        let holder: HolderResponse = serde_json::from_str(r#"{"type":"Hello","version":1,"pid":42}"#).unwrap();
        assert!(matches!(holder, HolderResponse::Hello { capabilities, build: None, .. } if capabilities.is_empty()));

        let manager: Response = serde_json::from_str(r#"{"type":"Hello","version":2,"pid":42}"#).unwrap();
        assert!(matches!(manager, Response::Hello { build: None, .. }));
    }

    #[test]
    fn older_attachment_events_have_no_tmux_locations() {
        let event: HolderEvent = serde_json::from_str(r#"{"type":"Attached","count":2,"focused":1}"#).unwrap();
        assert!(
            matches!(event, HolderEvent::Attached { count: 2, focused: 1, tmux_locations } if tmux_locations.is_empty())
        );
    }
}
