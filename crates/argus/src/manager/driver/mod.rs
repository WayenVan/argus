//! Drivers: how argus treats each kind of agent.
//!
//! A driver adjusts the launch command (e.g. to register hooks) and turns the
//! agent's hook events into a [`Hint`]. Drivers hold no state: there is one
//! instance per kind, shared by every agent of that kind. What a hint does to
//! an agent's activity is decided by the common state machine in
//! `activity.rs`, which is the same for every kind.

mod claude;
pub(crate) mod codex;
mod generic;
mod omp;
mod opencode;
mod pi;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use argus_proto::msg::Activity;
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
    /// The user interrupted the turn; it ended without `Done`.
    Interrupted,
    Error,
    /// Not relevant to the main agent's activity.
    Ignore,
}

/// The command a holder will execute, as a driver may rewrite it.
pub struct Launch {
    pub command: Vec<String>,
    /// The agent's environment, starting as the client's.
    pub env: Vec<(String, String)>,
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
    fn initial_activity(&self) -> Option<Activity> {
        None
    }

    /// For agents that report nothing before the first prompt: a freshly
    /// started agent is at its prompt once it has shown a cursor on its
    /// alternate screen for this long without hiding it. Agents that draw
    /// their prompt first and a startup dialog over it after a moment need
    /// the wait; keys typed into the dialog would answer it.
    fn ready_on_cursor(&self) -> Option<Duration> {
        None
    }

    /// Rewrites the launch command or environment. Returns a warning to show the user when
    /// state tracking will be degraded; the agent is started regardless.
    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>>;

    /// Interprets one hook event.
    fn interpret(&self, event: &Value) -> Hint;

    /// What the hook prints to keep the agent going after `event`, a turn's
    /// end, with `reason` as its next input. `None` when this agent cannot be
    /// held, and the turn ends as usual.
    fn hold_stop(&self, _event: &Value, _reason: &str) -> Option<String> {
        None
    }
}

static CLAUDE: claude::Claude = claude::Claude;
static CODEX: codex::Codex = codex::Codex;
static GENERIC: generic::Generic = generic::Generic;
static OMP: omp::Omp = omp::Omp;
static OPENCODE: opencode::Opencode = opencode::Opencode;
static PI: pi::Pi = pi::Pi;

/// The driver for a kind; unknown kinds get the generic driver.
pub fn for_kind(kind: &str) -> &'static dyn Driver {
    match kind {
        "claude" => &CLAUDE,
        "codex" => &CODEX,
        "omp" => &OMP,
        "opencode" => &OPENCODE,
        "pi" => &PI,
        _ => &GENERIC,
    }
}

/// Writes the files drivers share across agents (e.g. Claude's hook
/// settings, opencode's plugin). Rewritten at every manager start and never
/// removed, since running agents may still read them.
pub fn install_shared_files(ctx: &Context) -> Result<()> {
    claude::write_shared_settings(ctx)?;
    opencode::write_shared_files(ctx)?;
    pi::write_shared_files(ctx)?;
    omp::write_shared_files(ctx)
}

/// Injected as a system-prompt/developer-instruction addition at launch, so
/// the agent can update its own state without needing a wider grant — see
/// the design doc's `argus tui` recap section for why this replaced both the
/// hook-derived-summary and Claude-session-title approaches to recap.
/// Coordination is injected whole rather than behind a command the agent must
/// remember to run: agents do not reliably follow such pointers.
pub(super) const SELF_LABEL_INSTRUCTIONS: &str = concat!(
    include_str!("../../instructions/core.md"),
    "\n",
    include_str!("../../instructions/labels.md"),
    "\n",
    include_str!("../../instructions/coordination.md"),
    "\nBefore your final message: if this turn changed where things stand, make sure your recap\n",
    "says so; if your labels are unset, set them.\n",
);

/// `'path' arg`, quoted for the `sh -c` that agents run hook commands through.
pub fn hook_command(hook_exe: &Path, source: &str) -> String {
    let path = hook_exe.to_string_lossy().replace('\'', r"'\''");
    format!("'{path}' {source}")
}

/// Interprets an event from one of argus's own in-process plugins (opencode,
/// pi, omp), which flatten their agent's events into Claude-style hook events.
/// Events in any other format `version` are ignored: an agent keeps the
/// plugin it started with while the manager may be upgraded underneath it.
fn plugin_hint(event: &Value, version: u64) -> Hint {
    if event.get("v").and_then(Value::as_u64) != Some(version) {
        return Hint::Ignore;
    }
    let tool = || field(event, "tool_name").map(str::to_string);
    match field(event, "hook_event_name").unwrap_or_default() {
        "SessionStart" => Hint::SessionStart,
        "UserPromptSubmit" | "PostToolUse" => Hint::Working,
        "PreToolUse" => Hint::Tool(tool().unwrap_or_else(|| "tool".into())),
        "PermissionRequest" => Hint::WaitingApproval,
        // The tool that asked now runs (or was refused, and the next event
        // says what happens instead).
        "PermissionReplied" => tool().map_or(Hint::Working, Hint::Tool),
        "Stop" => Hint::Done,
        "StopFailure" => Hint::Error,
        // The user interrupted the turn and is presumably about to type.
        "Interrupt" => Hint::Interrupted,
        _ => Hint::Ignore,
    }
}

/// Reads a string field of a hook event.
fn field<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::SELF_LABEL_INSTRUCTIONS;

    /// Agents copy the example: it must be single-quoted, with the escape for
    /// a quote inside spelled as the shell needs it.
    #[test]
    fn label_example_is_shell_safe() {
        assert!(SELF_LABEL_INSTRUCTIONS.contains("argus label self title='Fix auth redirect'"));
        assert!(SELF_LABEL_INSTRUCTIONS.contains(r"as '\''"));
        assert!(!SELF_LABEL_INSTRUCTIONS.contains("=\"<value>\""));
    }
}
