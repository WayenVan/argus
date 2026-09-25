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

/// Injected as a system-prompt/developer-instruction addition at launch, so
/// the agent can update its own state without needing a wider grant — see
/// the design doc's `argus tui` recap section for why this replaced both the
/// hook-derived-summary and Claude-session-title approaches to recap.
pub(super) const SELF_LABEL_INSTRUCTIONS: &str = "\
You are running as a session managed by argus, a lightweight process manager for \
coding-agent sessions. Your argus agent id is in the environment variable $ARGUS_AGENT_ID. \
MANDATORY, EVERY TURN, NO EXCEPTIONS: you maintain two conventional labels, title and recap.
Before you consider a turn complete and hand control back to the user, you MUST evaluate
both of them. This is a hard requirement, not a suggestion, not something to do \"when it
feels right\", and not something you may skip because the turn was small or unrelated. Your
very first turn is not an exception either — both labels start unset, so your first turn is
exactly when you set them; do not wait for a \"better\" moment that never comes. If you catch
yourself finishing a response without having done this check, you have made a mistake.

The evaluation itself is always required; only the update is conditional. For each label,
compare it against what you already recorded — from your own memory of the conversation, not
by querying anything — and update only if it's unset or stale:

1. title — a short name for what this session is about. Stale means what you're working on
   has genuinely moved on from that title (a rename, a pivot). If it still describes the
   session accurately, leave it alone.
2. recap — a short summary of what you're doing or just did. Stale means what you last
   recorded no longer describes where things stand (you finished that step, hit a different
   problem, moved to a new part of the task). If you're still in the middle of exactly what
   the current recap already says, leave it alone.

Update with: argus label $ARGUS_AGENT_ID <key>=\"<value>\".
Do not run any other argus subcommand on yourself or other agents unless the user explicitly
asks you to and explains why.

Before you send your final message for this turn, double check: have you actually evaluated
title and recap this turn? If not, do it now, before responding.";

/// `'path' arg`, quoted for the `sh -c` that agents run hook commands through.
pub fn hook_command(hook_exe: &Path, source: &str) -> String {
    let path = hook_exe.to_string_lossy().replace('\'', r"'\''");
    format!("'{path}' {source}")
}

/// Reads a string field of a hook event.
fn field<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(Value::as_str)
}
