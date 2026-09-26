//! Turns: counting them, logging each one's prompt and reply, and holding a
//! turn's end open once with directives (see [`super::directive`]): the
//! agent keeps working on them, and only the end that follows counts as the
//! turn.

use argus_proto::msg::now_secs;
use argus_proto::text;
use serde_json::Value;

use super::registry::AgentRecord;
use super::{directive, log};
use crate::driver::{Driver, Hint};
use crate::turns::Turn;

/// The turn in progress, as far as its hook events have told.
#[derive(Default)]
pub struct TurnState {
    /// The `turns` value at which a `Stop` was last held back with
    /// directives, so a turn is held at most once.
    held: Option<u64>,
    /// The prompt of the turn in progress, for the turn log.
    prompt: Option<String>,
    /// The reply a held-back `Stop` carried: the turn's real answer, which
    /// the reply after the directives only adds to.
    held_reply: Option<String>,
}

/// Keeps a finishing turn going once, with whatever the directives ask for.
/// Returns what the hook prints to do so.
pub(super) fn hold(rec: &mut AgentRecord, driver: &dyn Driver, event: &Value) -> Option<String> {
    if rec.runtime.turn.held == Some(rec.info.turns) {
        return None;
    }
    let reason = directive::at_stop(&rec.info)?;
    let stdout = driver.hold_stop(event, &reason)?;
    rec.runtime.turn.held = Some(rec.info.turns);
    log(&format!("holding the turn of {} open: {reason}", rec.info.name));
    Some(stdout)
}

/// Counts a finished turn, interrupted ones included, even one that leaves
/// the activity unchanged (a turn ending `done` while the last result is
/// still unseen).
pub(super) fn count(rec: &mut AgentRecord, hint: &Hint) {
    if matches!(hint, Hint::Done | Hint::Error | Hint::Interrupted) {
        rec.info.turns += 1;
    }
}

/// Collects a turn's prompt and reply from the hook events that carry them
/// (`UserPromptSubmit.prompt`, `Stop.last_assistant_message`; argus's own
/// plugins send the same fields) and returns the finished turn to log. Call
/// after [`count`]; `held` when [`hold`] just held this event's turn open.
pub(super) fn track(rec: &mut AgentRecord, event: &Value, hint: &Hint, held: bool) -> Option<Turn> {
    let text = |key| event.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
    let state = &mut rec.runtime.turn;
    if held {
        state.held_reply = text("last_assistant_message");
        return None;
    }
    let ended = match hint {
        Hint::Done => "done",
        Hint::Error => "error",
        Hint::Interrupted => "interrupted",
        _ => {
            // Directives fed back into a held turn are not a new prompt.
            let is_prompt = event.get("hook_event_name").and_then(Value::as_str) == Some("UserPromptSubmit");
            if is_prompt && state.held != Some(rec.info.turns) {
                state.prompt = text("prompt");
                state.held_reply = None;
            }
            return None;
        }
    };
    let reply = match (state.held_reply.take(), text("last_assistant_message")) {
        (Some(held), Some(last)) if held != last => Some(format!("{held}\n\n{last}")),
        (held, last) => held.or(last),
    };
    Some(Turn {
        turn: rec.info.turns,
        ended: ended.into(),
        at: now_secs(),
        prompt: state.prompt.take().map(text::clip),
        reply: reply.map(text::clip),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver;
    use crate::manager::hooks::process;
    use crate::manager::registry::test_record;
    use serde_json::json;

    #[test]
    fn finished_turns_are_counted() {
        let mut rec = test_record();
        for hint in [Hint::Working, Hint::Tool("Bash".into()), Hint::WaitingInput] {
            count(&mut rec, &hint);
        }
        assert_eq!(rec.info.turns, 0);
        count(&mut rec, &Hint::Done);
        count(&mut rec, &Hint::Done); // A second unseen result is still a turn.
        count(&mut rec, &Hint::Error);
        count(&mut rec, &Hint::Interrupted);
        assert_eq!(rec.info.turns, 4);
    }

    #[test]
    fn a_turn_is_held_once_and_counted_once() {
        let claude = driver::for_kind("claude");
        let stop = json!({"hook_event_name":"Stop","stop_hook_active":false});
        let mut rec = test_record();
        let held = hold(&mut rec, claude, &stop).expect("labels are unset");
        assert!(held.contains(r#""decision":"block""#), "{held}");
        // The agent keeps going and ends again: that end is the turn.
        assert_eq!(hold(&mut rec, claude, &stop), None, "held at most once per turn");
        count(&mut rec, &Hint::Done);
        assert_eq!(rec.info.turns, 1);

        // A later turn with labels still unset is held again; one with them set is not.
        assert!(hold(&mut rec, claude, &stop).is_some());
        rec.info.turns += 1;
        rec.info.labels = [("title", "Fix auth"), ("recap", "Testing")].map(|(k, v)| (k.into(), v.into())).into();
        assert_eq!(hold(&mut rec, claude, &stop), None);

        let mut rec = test_record();
        let continuing = json!({"hook_event_name":"Stop","stop_hook_active":true});
        assert_eq!(hold(&mut rec, claude, &continuing), None, "Claude is already continuing");
        assert_eq!(hold(&mut rec, driver::for_kind("pi"), &stop), None, "not supported yet");
    }

    /// The turn a hook event finishes, through the whole of `process`.
    fn turn_after(rec: &mut AgentRecord, event: Value) -> Option<Turn> {
        process(rec, "claude", &event).and_then(|outcome| outcome.turn)
    }

    #[test]
    fn turns_record_prompt_and_reply() {
        let prompt = |p: &str| json!({"hook_event_name":"UserPromptSubmit","prompt":p});
        let stop = |r: &str, active: bool| json!({"hook_event_name":"Stop","stop_hook_active":active,"last_assistant_message":r});
        let mut rec = test_record();
        rec.info.labels = [("title", "t"), ("recap", "r")].map(|(k, v)| (k.into(), v.into())).into();
        assert_eq!(turn_after(&mut rec, prompt("hi")), None);
        let t = turn_after(&mut rec, stop("hello", false)).expect("a finished turn");
        assert_eq!(
            (t.turn, t.ended.as_str(), t.prompt.as_deref(), t.reply.as_deref()),
            (1, "done", Some("hi"), Some("hello"))
        );

        // Held for its labels: the held reply is the answer, the one after
        // the directives adds to it unless it repeats it.
        rec.info.labels.clear();
        turn_after(&mut rec, prompt("fix it"));
        assert_eq!(turn_after(&mut rec, stop("fixed", false)), None, "held");
        assert_eq!(turn_after(&mut rec, prompt("argus: set your labels")), None);
        let t = turn_after(&mut rec, stop("labels set", true)).unwrap();
        assert_eq!((t.prompt.as_deref(), t.reply.as_deref()), (Some("fix it"), Some("fixed\n\nlabels set")));

        turn_after(&mut rec, prompt("again"));
        turn_after(&mut rec, stop("same", false));
        let t = turn_after(&mut rec, stop("same", true)).unwrap();
        assert_eq!(t.reply.as_deref(), Some("same"));

        // An interrupt ends a turn with whatever it has; the next starts clean.
        rec.info.labels = [("title", "t"), ("recap", "r")].map(|(k, v)| (k.into(), v.into())).into();
        turn_after(&mut rec, prompt("long job"));
        let t = turn_after(&mut rec, json!({"hook_event_name":"StopFailure"})).unwrap();
        assert_eq!((t.ended.as_str(), t.prompt.as_deref(), t.reply), ("error", Some("long job"), None));
        let t = turn_after(&mut rec, stop("", false)).unwrap();
        assert_eq!((t.prompt, t.reply), (None, None), "nothing carried over");
    }
}
