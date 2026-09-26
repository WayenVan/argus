//! Hook events, from a driver's report to the agent's new state.
//!
//! [`Manager::report`] handles one event forwarded by `argus-hook`. Under the
//! registry lock, [`process`] runs it through these steps, each kept by its
//! own module:
//!
//! 1. the driver translates the event into a hint and an interaction change;
//! 2. events from another session are dropped ([`Session`]);
//! 3. a finishing turn may be held open once ([`turn::hold`]);
//! 4. finished turns are counted and logged ([`turn`]);
//! 5. pending requests are updated ([`interaction::update`]);
//! 6. the activity follows the hint, and is `blocked` while a request needs
//!    a person ([`activity`]);
//! 7. the silence watchdog is armed while `working` ([`activity::arm_watchdog`]).
//!
//! What must not happen under the lock (writing the turn log, starting
//! timers) comes back as an [`Outcome`] and runs after.

use std::sync::Arc;

use argus_proto::msg::Activity;
use serde_json::Value;

use super::interaction::{self, Confirm};
use super::registry::AgentRecord;
use super::{Manager, activity, log, turn};
use crate::driver::{self, DriverReport, Hint};
use crate::turns::{self, Turn};

impl Manager {
    /// Handles a hook event forwarded by `argus-hook`. Returns what the hook
    /// prints for the agent, if anything.
    pub(super) fn report(self: &Arc<Self>, agent_id: u64, source: &str, event: &Value) -> Option<String> {
        let outcome = self.registry.lock().unwrap().edit(agent_id, |rec| process(rec, source, event)).flatten();
        let Outcome { stdout, turn, start_watchdog, confirm } = outcome?;
        if let Some(turn) = turn
            && let Err(e) = turns::append(agent_id, &turn)
        {
            log(&format!("recording turn {} of agent {agent_id}: {e}", turn.turn));
        }
        if start_watchdog {
            let manager = self.clone();
            tokio::spawn(async move { manager.watchdog(agent_id).await });
        }
        if let Some(confirm) = confirm {
            let manager = self.clone();
            tokio::spawn(async move { manager.confirm_interaction(agent_id, confirm).await });
        }
        stdout
    }
}

/// What handling a hook event leaves to do once the registry is unlocked.
pub(super) struct Outcome {
    /// What the hook prints for the agent.
    pub stdout: Option<String>,
    /// A finished turn to append to the turn log.
    pub turn: Option<Turn>,
    pub start_watchdog: bool,
    pub confirm: Option<Confirm>,
}

/// Applies one hook event to an agent's record. `None` when the event is
/// dropped: the agent has exited, the event is from another kind of agent,
/// or from another session.
pub(super) fn process(rec: &mut AgentRecord, source: &str, event: &Value) -> Option<Outcome> {
    if !rec.info.status.is_live() {
        return None;
    }
    let driver = driver::for_kind(&rec.info.kind);
    if !driver.has_hooks() || driver.kind() != source {
        log(&format!("dropping {source} hook event for {} ({})", rec.info.name, rec.info.kind));
        return None;
    }
    let DriverReport { mut hint, interaction } = driver.translate(event);
    if !rec.runtime.session.accepts(event, &hint) {
        return None;
    }
    let was_working = rec.info.activity == Activity::Working;
    let stdout = if hint == Hint::Done { turn::hold(rec, driver, event) } else { None };
    if stdout.is_some() {
        hint = Hint::Working; // Not the end of the turn yet.
    }
    // Saved, so a caller's `--after` still means the same turn after a
    // manager restart.
    turn::count(rec, &hint);
    let turn = turn::track(rec, event, &hint, stdout.is_some());
    let confirm = interaction::update(&mut rec.info.pending_interactions, interaction, &hint);
    activity::apply(rec, hint);
    activity::settle_blocked(rec);
    let start_watchdog = activity::arm_watchdog(rec, was_working);
    Some(Outcome { stdout, turn, start_watchdog, confirm })
}

/// The agent session hook reports are bound to. `ARGUS_AGENT_ID` is
/// inherited by everything the agent runs, so another agent started inside
/// it could report under the same ID; its events carry a different session.
#[derive(Default)]
pub struct Session {
    id: Option<String>,
}

impl Session {
    /// Whether `event` is from the bound session, binding the first one seen.
    fn accepts(&mut self, event: &Value, hint: &Hint) -> bool {
        let Some(session) = event.get("session_id").and_then(Value::as_str) else { return true };
        // Only explicit main-session switches may replace the binding. Codex's
        // `/btw` side conversation starts with `source: "fork"`; treating every
        // non-startup source as a restart lets it steal the main agent's state.
        let restarted = *hint == Hint::SessionStart
            && matches!(event.get("source").and_then(Value::as_str), Some("clear" | "resume" | "reload"));
        match &self.id {
            Some(bound) if bound == session => true,
            Some(_) if !restarted => false,
            _ => {
                self.id = Some(session.to_string());
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::registry::test_record;
    use argus_proto::msg::InteractionPhase;
    use serde_json::json;

    #[test]
    fn session_binding() {
        let mut session = Session::default();
        let start = |s: &str, source: &str| json!({"hook_event_name":"SessionStart","session_id":s,"source":source});
        let stop = |s: &str| json!({"hook_event_name":"Stop","session_id":s});
        assert!(session.accepts(&start("a", "startup"), &Hint::SessionStart));
        assert!(session.accepts(&stop("a"), &Hint::Done));
        assert!(!session.accepts(&stop("nested"), &Hint::Done), "other sessions are dropped");
        assert!(!session.accepts(&start("nested", "startup"), &Hint::SessionStart), "nested agents cannot take over");
        assert!(!session.accepts(&start("side", "fork"), &Hint::SessionStart), "/btw cannot take over");
        assert!(!session.accepts(&start("unknown", "other"), &Hint::SessionStart));
        assert!(session.accepts(&stop("a"), &Hint::Done), "main session still reports after /btw");
        assert!(session.accepts(&start("b", "clear"), &Hint::SessionStart), "/clear rebinds");
        assert!(!session.accepts(&stop("a"), &Hint::Done));
        assert!(session.accepts(&start("c", "resume"), &Hint::SessionStart), "resume rebinds");
    }

    /// A permission request only blocks the agent once a person must answer,
    /// and whatever the agent does next unblocks it.
    #[test]
    fn blocked_only_while_a_request_needs_the_user() {
        let mut rec = test_record();
        let mut event = |event: Value| {
            process(&mut rec, "claude", &event).expect("accepted");
            let phases: Vec<_> = rec.info.pending_interactions.iter().map(|p| p.phase).collect();
            (rec.info.activity.clone(), phases)
        };
        event(json!({"hook_event_name":"PreToolUse","tool_name":"Bash"}));
        assert_eq!(
            event(json!({"hook_event_name":"PermissionRequest","tool_name":"Bash"})),
            (Activity::Tool("Bash".into()), vec![InteractionPhase::Observed]),
            "another hook may still answer it"
        );
        assert_eq!(
            event(json!({"hook_event_name":"Notification","notification_type":"permission_prompt"})),
            (Activity::Blocked, vec![InteractionPhase::NeedsUser])
        );
        assert_eq!(
            event(json!({"hook_event_name":"Notification","notification_type":"other"})),
            (Activity::Blocked, vec![InteractionPhase::NeedsUser]),
            "an unrelated event leaves it blocked"
        );
        assert_eq!(event(json!({"hook_event_name":"PostToolUse"})), (Activity::Working, vec![]));
    }

    #[test]
    fn events_from_elsewhere_are_dropped() {
        let mut rec = test_record();
        assert!(process(&mut rec, "codex", &json!({"hook_event_name":"Stop"})).is_none(), "wrong kind");
        rec.info.status = argus_proto::msg::AgentStatus::Exited;
        assert!(process(&mut rec, "claude", &json!({"hook_event_name":"Stop"})).is_none(), "exited");
    }
}
