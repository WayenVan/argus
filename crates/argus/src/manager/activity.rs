//! The activity state machine, shared by every kind of agent.
//!
//! Hook events move an agent's activity through [`apply`] (see `hooks.rs`
//! for the whole path of an event); holder facts (focus, input), timers and
//! user acks move it here:
//!
//! - `done` means "finished and nobody has looked yet". It becomes
//!   `idle` when an attached terminal has focus, someone types, or
//!   the user acks it. Being attached alone does not count.
//! - `blocked` means a request needs the agent's user; see [`settle_blocked`].
//! - `working` is guarded by a silence watchdog: if no hook arrives and the
//!   output offset stops moving, the agent is shown as `unknown` (e.g. Claude
//!   interrupted with Esc never sends `Stop`). `tool:*` is exempt, since a
//!   long tool run can legitimately be silent.
//! - Agents that report nothing at first are `idle` once their cursor has
//!   stayed up for a while; see `Driver::ready_on_cursor`.
//! - Agents without hooks are `busy` or `quiet` from their output offset.

use std::sync::Arc;
use std::time::{Duration, Instant};

use argus_proto::msg::{Activity, TmuxLocation, now_secs};

use super::interaction::needs_user;
use super::registry::AgentRecord;
use super::{Manager, holder};
use crate::driver::Hint;

/// How long `working` may go without hooks or output before it is `unknown`.
const SILENCE: Duration = Duration::from_secs(15);
/// How often hook-less agents are checked for output.
const GENERIC_POLL: Duration = Duration::from_secs(2);

/// The silence watchdog's state for one agent.
#[derive(Default)]
pub struct Watchdog {
    /// When to look at the output offset next.
    deadline: Option<Instant>,
    /// Whether a watchdog task is running for this agent.
    running: bool,
    /// Bumped each time the agent enters `working`, so the watchdog takes a
    /// fresh output baseline for every working period.
    generation: u64,
}

impl Manager {
    /// Applies a holder fact or a user action to one agent.
    pub(super) fn on_fact(&self, id: u64, fact: Fact) {
        self.registry.lock().unwrap().edit(id, |rec| match fact {
            Fact::Attached { count, focused, tmux_locations } => {
                let gained_focus = focused > rec.runtime.focused;
                rec.runtime.focused = focused;
                rec.info.attached = count;
                rec.info.tmux_locations = tmux_locations;
                if gained_focus && rec.info.activity == Activity::Done {
                    set(rec, Activity::Idle);
                }
            }
            Fact::Input => {
                rec.runtime.send.typed(Instant::now());
                // Not `blocked`: arrow keys and typing may only navigate a
                // dialog. Wait for the agent's reply or next tool event.
                if rec.info.activity == Activity::Done {
                    set(rec, Activity::Idle);
                }
            }
            Fact::Ack => {
                if rec.info.activity == Activity::Done {
                    set(rec, Activity::Idle);
                }
            }
        });
    }

    /// A freshly started agent showed or hid its cursor. Once it has stayed
    /// up for `steady` (see `Driver::ready_on_cursor`), the agent is at its
    /// prompt, unless a hook has reported something since it started.
    pub(super) fn on_cursor(self: &Arc<Self>, id: u64, shown: bool, steady: Duration) {
        let generation = self.registry.lock().unwrap().edit(id, |rec| {
            rec.runtime.cursor_gen += 1;
            (shown && rec.info.activity == Activity::Unknown).then_some(rec.runtime.cursor_gen)
        });
        let Some(generation) = generation.flatten() else { return };
        let manager = self.clone();
        let ready = move || {
            manager.registry.lock().unwrap().edit(id, |rec| {
                if rec.runtime.cursor_gen == generation && rec.info.activity == Activity::Unknown {
                    set(rec, Activity::Idle);
                }
            });
        };
        if steady.is_zero() {
            return ready();
        }
        tokio::spawn(async move {
            tokio::time::sleep(steady).await;
            ready();
        });
    }

    /// Downgrades `working` to `unknown` once hooks and output both go quiet.
    pub(super) async fn watchdog(self: Arc<Self>, id: u64) {
        /// What the watchdog does after a check at its deadline.
        enum Step {
            Stop,
            /// Start over with a new working period.
            Again,
            /// Keep watching, measuring output from the offset just read.
            Rebase,
        }

        let mut baseline = None;
        let mut period = None;
        loop {
            let armed = self.registry.lock().unwrap().edit(id, |rec| match rec.runtime.watchdog.deadline {
                Some(d) if rec.info.status.is_live() && rec.info.activity == Activity::Working => {
                    Some((d, rec.runtime.watchdog.generation))
                }
                _ => {
                    rec.runtime.watchdog.running = false;
                    None
                }
            });
            let Some((deadline, current_gen)) = armed.flatten() else { return };
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
            let step = self.registry.lock().unwrap().edit(id, |rec| {
                if rec.info.activity != Activity::Working {
                    rec.runtime.watchdog.running = false;
                    return Step::Stop;
                }
                if rec.runtime.watchdog.generation != current_gen {
                    return Step::Again; // Re-entered working while we checked; start over.
                }
                if rec.runtime.watchdog.deadline.is_some_and(|d| d > deadline) {
                    return Step::Rebase; // A hook arrived while we were checking.
                }
                match (baseline, offset) {
                    (Some(before), Some(now)) if now > before => {
                        rec.runtime.watchdog.deadline = Some(Instant::now() + SILENCE);
                        Step::Rebase
                    }
                    _ => {
                        set(rec, Activity::Unknown);
                        rec.runtime.watchdog.running = false;
                        Step::Stop
                    }
                }
            });
            match step {
                None | Some(Step::Stop) => return,
                Some(Step::Again) => {}
                Some(Step::Rebase) => baseline = offset,
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
                let polled = manager.registry.lock().unwrap().edit(id, |rec| {
                    if !rec.info.status.is_live() || offset.is_none() {
                        return false;
                    }
                    let busy = last.is_some() && offset != last;
                    set(rec, if busy { Activity::Busy } else { Activity::Quiet });
                    true
                });
                if polled != Some(true) {
                    return;
                }
                last = offset;
                tokio::time::sleep(GENERIC_POLL).await;
            }
        });
    }
}

pub enum Fact {
    Attached { count: u32, focused: u32, tmux_locations: Vec<TmuxLocation> },
    Input,
    Ack,
}

/// Moves the activity as a hook's hint says.
pub(super) fn apply(rec: &mut AgentRecord, hint: Hint) {
    let activity = match hint {
        // Unseen results stay marked until someone looks.
        Hint::SessionStart | Hint::WaitingInput | Hint::Interrupted if rec.info.activity == Activity::Done => return,
        Hint::SessionStart | Hint::WaitingInput | Hint::Interrupted => Activity::Idle,
        Hint::Working => Activity::Working,
        Hint::Tool(name) => Activity::Tool(name),
        // Someone watching it finish has already seen it.
        Hint::Done if rec.runtime.focused > 0 => Activity::Idle,
        Hint::Done => Activity::Done,
        Hint::Error => Activity::Error,
        Hint::Ignore => return,
    };
    set(rec, activity);
}

/// The one way into `blocked`: while a request needs the agent's user. The
/// next hint showing the agent moving on clears the requests and sets its
/// activity (see `interaction::update`).
pub(super) fn settle_blocked(rec: &mut AgentRecord) {
    if needs_user(&rec.info) {
        set(rec, Activity::Blocked);
    }
}

/// After a hook event: while `working`, pushes the silence deadline out, and
/// starts a new working period if the agent was not `working` before the
/// event. Returns whether a watchdog task needs starting.
pub(super) fn arm_watchdog(rec: &mut AgentRecord, was_working: bool) -> bool {
    if rec.info.activity != Activity::Working {
        return false;
    }
    let watchdog = &mut rec.runtime.watchdog;
    if !was_working {
        watchdog.generation += 1;
    }
    watchdog.deadline = Some(Instant::now() + SILENCE);
    !std::mem::replace(&mut watchdog.running, true)
}

/// Changes the activity, noting when it did. Setting the current one again
/// changes nothing, not even its `activity_since`.
fn set(rec: &mut AgentRecord, activity: Activity) {
    if rec.info.activity == activity {
        return;
    }
    rec.runtime.send.activity_changed(&activity);
    rec.info.activity = activity;
    rec.info.activity_since = Some(now_secs());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::registry::test_record;

    #[test]
    fn done_needs_focus_input_or_ack() {
        let mut rec = test_record();
        apply(&mut rec, Hint::Working);
        apply(&mut rec, Hint::Done);
        assert_eq!(rec.info.activity, Activity::Done);
        // Idle notifications do not clear an unseen result.
        apply(&mut rec, Hint::WaitingInput);
        assert_eq!(rec.info.activity, Activity::Done);

        let mut watched = test_record();
        watched.runtime.focused = 1;
        apply(&mut watched, Hint::Done);
        assert_eq!(watched.info.activity, Activity::Idle);
    }

    #[test]
    fn a_watchdog_starts_once_per_agent_and_counts_working_periods() {
        let mut rec = test_record();
        assert!(!arm_watchdog(&mut rec, false), "not working");
        apply(&mut rec, Hint::Working);
        assert!(arm_watchdog(&mut rec, false));
        assert!(!arm_watchdog(&mut rec, true), "already running");
        assert_eq!(rec.runtime.watchdog.generation, 1, "one working period");
        assert!(rec.runtime.watchdog.deadline.is_some());
    }
}
