//! Drivers: how argus treats each kind of agent.
//!
//! A driver adjusts the launch command (e.g. to register hooks) and turns the
//! agent's hook events into a [`DriverReport`]. Drivers hold no state: there is one
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
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use argus_proto::msg::{Activity, InteractionPhase, PendingInteraction};
use argus_proto::text;
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

/// One translated hook event. Only drivers inspect provider-specific JSON;
/// the manager consumes this value and updates its shared state machine.
pub struct DriverReport {
    pub hint: Hint,
    pub interaction: Option<InteractionChange>,
}

/// Lifecycle of a request that may need an answer from a person.
pub enum InteractionChange {
    /// `confirm_after` and `on_screen` promote the request to `needs_user`
    /// once it is still `observed` after the delay, or once the agent's
    /// screen shows it to a person.
    Opened {
        request: PendingInteraction,
        confirm_after: Option<Duration>,
        on_screen: Option<ScreenCheck>,
    },
    NeedsUser {
        fallback: PendingInteraction,
    },
    Closed {
        id: Option<String>,
        session_id: Option<String>,
    },
}

/// Whether the agent's screen shows a request to a person. Only consulted
/// while the request a hook reported is still `observed`, so it confirms that
/// request rather than discovering one.
pub type ScreenCheck = Arc<dyn Fn(&vt100::Screen) -> bool + Send + Sync>;

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

    /// Provider-specific interaction information from the same hook event.
    fn interaction(&self, _event: &Value) -> Option<InteractionChange> {
        None
    }

    fn translate(&self, event: &Value) -> DriverReport {
        DriverReport { hint: self.interpret(event), interaction: self.interaction(event) }
    }

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
/// remember to run: agents do not reliably follow such pointers. `argus
/// guide` prints the same text, for sessions it never reached.
pub(crate) const SELF_LABEL_INSTRUCTIONS: &str = concat!(
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

/// Common interaction translation for argus's in-process plugins. Pi and
/// OMP report a visible prompt immediately; OpenCode gets a short grace
/// period for requests that resolve without a person.
fn plugin_interaction(event: &Value, version: u64, confirm_after: Option<Duration>) -> Option<InteractionChange> {
    if event.get("v").and_then(Value::as_u64) != Some(version) {
        return None;
    }
    match field(event, "hook_event_name") {
        Some("PermissionRequest") => {
            let mut request = interaction_request(event);
            if confirm_after.is_none() {
                request.phase = InteractionPhase::NeedsUser;
            }
            Some(InteractionChange::Opened { request, confirm_after, on_screen: None })
        }
        Some("PermissionReplied") => Some(InteractionChange::Closed {
            id: field(event, "request_id").map(str::to_string),
            session_id: field(event, "request_session_id").map(str::to_string),
        }),
        _ => None,
    }
}

/// Reads a string field of a hook event.
fn field<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(Value::as_str)
}

/// The common fields forwarded by native hooks or argus's OpenCode plugin.
fn interaction_request(event: &Value) -> PendingInteraction {
    let session = field(event, "request_session_id").or_else(|| field(event, "session_id")).unwrap_or_default();
    let kind = field(event, "interaction_kind").unwrap_or("permission");
    let tool = field(event, "tool_name").unwrap_or_default();
    let id = field(event, "request_id")
        .or_else(|| field(event, "tool_use_id"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{session}:{kind}:{tool}"));
    let summary = field(event, "question_text")
        .or_else(|| field(event, "permission"))
        .or_else(|| field(event, "tool_name"))
        .map(|s| text::clip(s.chars().take(120).collect::<String>()));
    let choices = event
        .get("choices")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().filter_map(Value::as_str).take(12).map(|s| s.chars().take(80).collect::<String>()).collect()
        })
        .unwrap_or_default();
    let created_at =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    PendingInteraction {
        id,
        kind: kind.into(),
        phase: InteractionPhase::Observed,
        session_id: session.into(),
        summary,
        choices,
        created_at,
    }
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
