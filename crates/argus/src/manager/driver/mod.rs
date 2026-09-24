//! Drivers: how argus treats each kind of agent.
//!
//! A driver adjusts the launch command (e.g. to register hooks) and turns the
//! agent's hook events into a [`Hint`]. Drivers hold no state: there is one
//! instance per kind, shared by every agent of that kind. What a hint does to
//! an agent's activity is decided by the common state machine in
//! `activity.rs`, which is the same for every kind.

mod claude;
mod codex;
mod generic;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

/// What a hook event means, independent of which agent sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    /// A fresh or cleared session waiting for its first prompt.
    SessionStart,
    Working,
    Tool(String),
    WaitingApproval,
    /// The turn finished.
    Done,
    /// Idle, waiting for input.
    WaitingInput,
    Error,
    /// Not relevant to the main agent's activity.
    Ignore,
}

/// The command a holder will execute, as a driver may rewrite it.
pub struct Launch {
    pub command: Vec<String>,
    /// The agent's own state directory, for per-agent files.
    pub agent_dir: PathBuf,
}

/// What drivers need to know about this argus installation.
pub struct Context {
    /// `argus-hook`, if installed next to `argus`.
    pub hook_exe: Option<PathBuf>,
    /// Directory for files the manager generates for drivers.
    pub dir: PathBuf,
}

pub trait Driver: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Whether the agent reports its state through hooks. Agents without
    /// hooks get `busy` / `quiet` from output activity instead.
    fn has_hooks(&self) -> bool;

    /// Activity to show once the agent is running, before any hook arrives.
    fn initial_activity(&self) -> Option<&'static str> {
        None
    }

    /// Rewrites the launch command. Returns a warning to show the user when
    /// state tracking will be degraded; the agent is started regardless.
    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>>;

    /// Interprets one hook event.
    fn interpret(&self, event: &Value) -> Hint;
}

static CLAUDE: claude::Claude = claude::Claude;
static CODEX: codex::Codex = codex::Codex;
static GENERIC: generic::Generic = generic::Generic;

/// The driver for a kind; unknown kinds get the generic driver.
pub fn for_kind(kind: &str) -> &'static dyn Driver {
    match kind {
        "claude" => &CLAUDE,
        "codex" => &CODEX,
        _ => &GENERIC,
    }
}

/// Writes the files drivers share across agents (e.g. Claude's hook settings).
pub fn install_shared_files(ctx: &Context) -> Result<()> {
    claude::write_shared_settings(ctx)
}

/// `'path' arg`, quoted for the `sh -c` that agents run hook commands through.
pub fn hook_command(hook_exe: &Path, source: &str) -> String {
    let path = hook_exe.to_string_lossy().replace('\'', r"'\''");
    format!("'{path}' {source}")
}

/// Reads a string field of a hook event.
fn field<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(Value::as_str)
}
