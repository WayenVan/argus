//! The common activity state machine, shared by every kind of agent.
//!
//! Drivers turn hook events into [`Hint`]s; this module decides what those
//! hints and the holder's facts (focus, input) do to an agent's activity:
//!
//! - `done` means "finished and nobody has looked yet". It becomes
//!   `idle` when an attached terminal has focus, someone types, or
//!   the user acks it. Being attached alone does not count.
//! - `working` is guarded by a silence watchdog: if no hook arrives and the
//!   output offset stops moving, the agent is shown as `unknown` (e.g. Claude
//!   interrupted with Esc never sends `Stop`). `tool:*` is exempt, since a
//!   long tool run can legitimately be silent.
//! - Agents without hooks are `busy` or `quiet` from their output offset.
//!
//! Everything is event-driven; the only timers are the per-agent watchdog
//! (armed only while `working`) and the poller for hook-less agents.

use std::sync::Arc;
use std::time::{Duration, Instant};

use argus_proto::msg::{Activity, TmuxLocation, now_secs};
use serde_json::Value;

use super::driver::{self, Hint};
use super::registry::AgentRecord;
use super::{Manager, holder, log};

/// How long `working` may go without hooks or output before it is `unknown`.
const SILENCE: Duration = Duration::from_secs(15);
/// How often hook-less agents are checked for output.
const GENERIC_POLL: Duration = Duration::from_secs(2);

impl Manager {
    /// Handles a hook event forwarded by `argus-hook`.
    pub(super) fn report(self: &Arc<Self>, agent_id: u64, source: &str, event: &Value) {
        let mut reg = self.registry.lock().unwrap();
        let Some(rec) = reg.agents.get_mut(&agent_id) else { return };
        if !rec.info.status.is_live() {
            return;
        }
        let driver = driver::for_kind(&rec.info.kind);
        if !driver.has_hooks() || driver.kind() != source {
            log(&format!("dropping {source} hook event for {} ({})", rec.info.name, rec.info.kind));
            return;
        }
        let hint = driver.interpret(event);
        if !bind_session(rec, event, &hint) {
            return;
        }
        // Saved, so a caller's `--after` still means the same turn after a
        // manager restart.
        let finished = count_turn(rec, &hint);
        let changed = apply(rec, hint) || finished;
        let arm = rec.info.activity == Activity::Working;
        if arm && changed {
            rec.runtime.working_gen += 1;
        }
        if arm {
            rec.runtime.deadline = Some(Instant::now() + SILENCE);
        }
        let start_watchdog = arm && !rec.runtime.watchdog;
        if start_watchdog {
            rec.runtime.watchdog = true;
        }
        if changed {
            reg.changed(agent_id);
        }
        if finished && let Err(e) = reg.save() {
            log(&format!("saving registry: {e:#}"));
        }
        drop(reg);
        if start_watchdog {
            let manager = self.clone();
            tokio::spawn(async move { manager.watchdog(agent_id).await });
        }
    }

    /// Applies a holder fact or a user action to one agent.
    pub(super) fn on_fact(&self, id: u64, fact: Fact) {
        let mut reg = self.registry.lock().unwrap();
        let Some(rec) = reg.agents.get_mut(&id) else { return };
        let changed = match fact {
            Fact::Attached { count, focused, tmux_locations } => {
                let gained_focus = focused > rec.runtime.focused;
                rec.runtime.focused = focused;
                let attach_changed = rec.info.attached != count;
                let locations_changed = rec.info.tmux_locations != tmux_locations;
                rec.info.attached = count;
                rec.info.tmux_locations = tmux_locations;
                let seen = gained_focus && rec.info.activity == Activity::Done && set(rec, Activity::Idle);
                attach_changed || locations_changed || seen
            }
            Fact::Input => {
                rec.runtime.last_input = Some(Instant::now());
                match rec.info.activity {
                    Activity::Done => set(rec, Activity::Idle),
                    // Answering a permission prompt takes a keypress; the
                    // tool that asked now runs.
                    Activity::Blocked => {
                        let next = rec.runtime.last_tool.clone().map_or(Activity::Working, Activity::Tool);
                        set(rec, next)
                    }
                    _ => false,
                }
            }
            Fact::Ack => rec.info.activity == Activity::Done && set(rec, Activity::Idle),
            // Nothing has reported otherwise since it started.
            Fact::Ready => rec.info.activity == Activity::Unknown && set(rec, Activity::Idle),
        };
        if changed {
            reg.changed(id);
        }
    }

    /// Downgrades `working` to `unknown` once hooks and output both go quiet.
    async fn watchdog(self: Arc<Self>, id: u64) {
        let mut baseline = None;
        let mut period = None;
        loop {
            let (deadline, current_gen) = {
                let mut reg = self.registry.lock().unwrap();
                let Some(rec) = reg.agents.get_mut(&id) else { return };
                match rec.runtime.deadline {
                    Some(d) if rec.info.status.is_live() && rec.info.activity == Activity::Working => {
                        (d, rec.runtime.working_gen)
                    }
                    _ => {
                        rec.runtime.watchdog = false;
                        return;
                    }
                }
            };
            if period != Some(current_gen) {
                // A new working period: measure silence from now on.
                period = Some(current_gen);
                baseline = holder::output_offset(id).await.ok();
                continue;
            }
            if Instant::now() < deadline {
                tokio::time::sleep_until(deadline.into()).await;
                continue;
            }
            let offset = holder::output_offset(id).await.ok();
            let mut reg = self.registry.lock().unwrap();
            let Some(rec) = reg.agents.get_mut(&id) else { return };
            if rec.info.activity != Activity::Working {
                rec.runtime.watchdog = false;
                return;
            }
            if rec.runtime.working_gen != current_gen {
                continue; // Re-entered working while we checked; start over.
            }
            if rec.runtime.deadline.is_some_and(|d| d > deadline) {
                baseline = offset; // A hook arrived while we were checking.
                continue;
            }
            match (baseline, offset) {
                (Some(before), Some(now)) if now > before => {
                    baseline = offset;
                    rec.runtime.deadline = Some(Instant::now() + SILENCE);
                }
                _ => {
                    set(rec, Activity::Unknown);
                    rec.runtime.watchdog = false;
                    reg.changed(id);
                    return;
                }
            }
        }
    }

    /// `busy` / `quiet` for agents whose driver has no hooks.
    pub(super) fn poll_output(self: &Arc<Self>, id: u64) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut last = None;
            loop {
                let offset = holder::output_offset(id).await.ok();
                {
                    let mut reg = manager.registry.lock().unwrap();
                    let Some(rec) = reg.agents.get_mut(&id) else { return };
                    if !rec.info.status.is_live() || offset.is_none() {
                        return;
                    }
                    let busy = last.is_some() && offset != last;
                    last = offset;
                    if set(rec, if busy { Activity::Busy } else { Activity::Quiet }) {
                        reg.changed(id);
                    }
                }
                tokio::time::sleep(GENERIC_POLL).await;
            }
        });
    }
}

pub enum Fact {
    Attached {
        count: u32,
        focused: u32,
        tmux_locations: Vec<TmuxLocation>,
    },
    Input,
    Ack,
    /// A freshly started agent can take input (see `Driver::ready_on_cursor`).
    Ready,
}

/// Ties hook reports to one agent session. `ARGUS_AGENT_ID` is inherited by
/// everything the agent runs, so another agent started inside it could
/// report under the same ID; its events carry a different session.
fn bind_session(rec: &mut AgentRecord, event: &Value, hint: &Hint) -> bool {
    let Some(session) = event.get("session_id").and_then(Value::as_str) else { return true };
    // `/clear` and resume start a new session inside the same agent; a nested
    // agent that just launched says `startup`, so it cannot take over.
    let restarted = *hint == Hint::SessionStart && event.get("source").and_then(Value::as_str) != Some("startup");
    match &rec.runtime.session_id {
        Some(bound) if bound == session => true,
        Some(_) if !restarted => false,
        _ => {
            rec.runtime.session_id = Some(session.to_string());
            true
        }
    }
}

/// Counts a finished turn, interrupted ones included, even one that leaves
/// the activity unchanged (a turn ending `done` while the last result is
/// still unseen).
fn count_turn(rec: &mut AgentRecord, hint: &Hint) -> bool {
    let finished = matches!(hint, Hint::Done | Hint::Error | Hint::Interrupted);
    if finished {
        rec.info.turns += 1;
    }
    finished
}

fn apply(rec: &mut AgentRecord, hint: Hint) -> bool {
    match hint {
        // Unseen results stay marked until someone looks.
        Hint::SessionStart | Hint::WaitingInput | Hint::Interrupted if rec.info.activity == Activity::Done => false,
        Hint::SessionStart | Hint::WaitingInput | Hint::Interrupted => set(rec, Activity::Idle),
        Hint::Working => set(rec, Activity::Working),
        Hint::Tool(name) => {
            let changed = set(rec, Activity::Tool(name.clone()));
            rec.runtime.last_tool = Some(name);
            changed
        }
        Hint::WaitingApproval => set(rec, Activity::Blocked),
        // Someone watching it finish has already seen it.
        Hint::Done if rec.runtime.focused > 0 => set(rec, Activity::Idle),
        Hint::Done => set(rec, Activity::Done),
        Hint::Error => set(rec, Activity::Error),
        Hint::Ignore => false,
    }
}

fn set(rec: &mut AgentRecord, activity: Activity) -> bool {
    if rec.info.activity == activity {
        return false;
    }
    if !activity.awaits_prompt() {
        rec.runtime.submitting = None; // The sent prompt went through.
    }
    rec.info.activity = activity;
    rec.info.activity_since = Some(now_secs());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::{AgentInfo, AgentStatus};
    use serde_json::json;

    fn record() -> AgentRecord {
        AgentRecord::new(AgentInfo {
            id: 1,
            name: "claude-1".into(),
            kind: "claude".into(),
            command: vec![],
            cwd: "/".into(),
            created_at: 0,
            exited_at: None,
            holder_pid: None,
            agent_pid: None,
            status: AgentStatus::Running,
            exit_code: None,
            activity: "unknown".into(),
            activity_since: None,
            turns: 0,
            attached: 0,
            tmux_locations: Vec::new(),
            labels: Default::default(),
        })
    }

    #[test]
    fn done_needs_focus_input_or_ack() {
        let mut rec = record();
        apply(&mut rec, Hint::Working);
        apply(&mut rec, Hint::Done);
        assert_eq!(rec.info.activity, Activity::Done);
        // Idle notifications do not clear an unseen result.
        apply(&mut rec, Hint::WaitingInput);
        assert_eq!(rec.info.activity, Activity::Done);

        let mut watched = record();
        watched.runtime.focused = 1;
        apply(&mut watched, Hint::Done);
        assert_eq!(watched.info.activity, Activity::Idle);
    }

    #[test]
    fn finished_turns_are_counted() {
        let mut rec = record();
        for hint in [Hint::Working, Hint::Tool("Bash".into()), Hint::WaitingApproval, Hint::WaitingInput] {
            assert!(!count_turn(&mut rec, &hint), "{hint:?}");
        }
        assert!(count_turn(&mut rec, &Hint::Done));
        apply(&mut rec, Hint::Done);
        assert!(count_turn(&mut rec, &Hint::Done), "a second unseen result is still a turn");
        assert!(count_turn(&mut rec, &Hint::Error));
        assert!(count_turn(&mut rec, &Hint::Interrupted));
        assert_eq!(rec.info.turns, 4);
    }

    #[test]
    fn session_binding() {
        let mut rec = record();
        let start = |s: &str, source: &str| json!({"hook_event_name":"SessionStart","session_id":s,"source":source});
        let stop = |s: &str| json!({"hook_event_name":"Stop","session_id":s});
        assert!(bind_session(&mut rec, &start("a", "startup"), &Hint::SessionStart));
        assert!(bind_session(&mut rec, &stop("a"), &Hint::Done));
        assert!(!bind_session(&mut rec, &stop("nested"), &Hint::Done), "other sessions are dropped");
        assert!(
            !bind_session(&mut rec, &start("nested", "startup"), &Hint::SessionStart),
            "nested agents cannot take over"
        );
        assert!(bind_session(&mut rec, &start("b", "clear"), &Hint::SessionStart), "/clear rebinds");
        assert!(!bind_session(&mut rec, &stop("a"), &Hint::Done));
    }
}
