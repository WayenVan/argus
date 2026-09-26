//! Pending interactions: requests an agent reported that may need a person.
//!
//! A request starts `observed` when the agent may still resolve it without a
//! person (a permission hook, an automatic reviewer), or `needs_user` when it
//! is known to be waiting. An observed request is promoted by later evidence:
//! a driver's own event, a grace period ([`ConfirmBy::Delay`]), or the
//! request showing on the agent's screen ([`ConfirmBy::Screen`]). An agent is
//! `blocked` exactly while a request needs its user (see
//! [`super::activity::settle_blocked`]).

use std::time::Duration;

use argus_proto::msg::{AgentInfo, InteractionPhase, PendingInteraction};

use super::Manager;
use super::activity::settle_blocked;
use crate::driver::{Hint, InteractionChange, ScreenCheck};

/// How often the screen of an agent with an observed request is checked
/// for it, while the request stays observed.
const SCREEN_POLL: Duration = Duration::from_millis(250);

/// How an observed request may later be promoted to `needs_user`.
pub(super) struct Confirm {
    request_id: String,
    created_at: u64,
    how: ConfirmBy,
}

enum ConfirmBy {
    Delay(Duration),
    Screen(ScreenCheck),
}

/// Whether a request is waiting on the agent's user.
pub(super) fn needs_user(info: &AgentInfo) -> bool {
    info.pending_interactions.iter().any(|p| p.phase == InteractionPhase::NeedsUser)
}

/// Applies a driver's request change to the pending list. Without one, a
/// hint that shows the agent moving on clears the list: whatever it asked
/// was answered. Returns how to confirm a new observed request, if one needs
/// confirming.
pub(super) fn update(
    pending: &mut Vec<PendingInteraction>,
    change: Option<InteractionChange>,
    hint: &Hint,
) -> Option<Confirm> {
    match change {
        Some(InteractionChange::Opened { request, confirm_after, on_screen }) => {
            if pending.iter().any(|p| p.id == request.id && p.session_id == request.session_id) {
                return None;
            }
            let how = confirm_after.map(ConfirmBy::Delay).or(on_screen.map(ConfirmBy::Screen));
            let confirm = how.filter(|_| request.phase == InteractionPhase::Observed).map(|how| Confirm {
                request_id: request.id.clone(),
                created_at: request.created_at,
                how,
            });
            pending.push(request);
            confirm
        }
        Some(InteractionChange::NeedsUser { fallback }) => {
            if pending.iter().any(|p| p.phase == InteractionPhase::NeedsUser) {
                return None;
            }
            if let Some(p) = pending.iter_mut().rev().find(|p| p.phase == InteractionPhase::Observed) {
                p.phase = InteractionPhase::NeedsUser;
            } else {
                pending.push(fallback);
            }
            None
        }
        Some(InteractionChange::Closed { id, session_id }) => {
            if let Some(id) = id {
                pending.retain(|p| p.id != id || session_id.as_ref().is_some_and(|s| p.session_id != *s));
            } else {
                pending.clear();
            }
            None
        }
        None if matches!(
            hint,
            Hint::Working
                | Hint::Tool(_)
                | Hint::SessionStart
                | Hint::Done
                | Hint::Error
                | Hint::Interrupted
                | Hint::WaitingInput
        ) =>
        {
            pending.clear();
            None
        }
        None => None,
    }
}

impl Manager {
    /// Promotes an observed request to `needs_user` after the driver's grace
    /// period for automatic resolution, or once its screen check sees the
    /// request in front of a person. Ends as soon as the request is gone or
    /// no longer observed; other hook events clear it.
    pub(super) async fn confirm_interaction(&self, agent_id: u64, confirm: Confirm) {
        match confirm.how {
            ConfirmBy::Delay(delay) => {
                tokio::time::sleep(delay).await;
                self.promote_interaction(agent_id, &confirm.request_id, confirm.created_at, true);
            }
            ConfirmBy::Screen(check) => loop {
                tokio::time::sleep(SCREEN_POLL).await;
                let shown = self.screens.inspect(agent_id, |screen| check(screen)).unwrap_or(false);
                if !self.promote_interaction(agent_id, &confirm.request_id, confirm.created_at, shown) || shown {
                    return;
                }
            },
        }
    }

    /// Promotes the request when `promote`. Returns whether it was still
    /// observed on a live agent.
    fn promote_interaction(&self, agent_id: u64, request_id: &str, created_at: u64, promote: bool) -> bool {
        let promoted = self.registry.lock().unwrap().edit(agent_id, |rec| {
            let live = rec.info.status.is_live();
            let Some(pending) =
                rec.info.pending_interactions.iter_mut().find(|p| p.id == request_id && p.created_at == created_at)
            else {
                return false;
            };
            if pending.phase != InteractionPhase::Observed || !live {
                return false;
            }
            if promote {
                pending.phase = InteractionPhase::NeedsUser;
                settle_blocked(rec);
            }
            true
        });
        promoted.unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver;
    use crate::manager::registry::{AgentRecord, test_record};
    use serde_json::{Value, json};

    /// Whether `event` changed the pending list, and how to confirm what it opened.
    fn translated_interaction(rec: &mut AgentRecord, event: &Value) -> (bool, Option<Confirm>) {
        let report = driver::for_kind(&rec.info.kind).translate(event);
        let before = rec.info.pending_interactions.clone();
        let confirm = update(&mut rec.info.pending_interactions, report.interaction, &report.hint);
        (rec.info.pending_interactions != before, confirm)
    }

    #[test]
    fn claude_request_is_observed_until_notification() {
        let mut rec = test_record();
        let request = json!({"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash"});
        let (changed, confirm) = translated_interaction(&mut rec, &request);
        assert!(changed);
        assert!(confirm.is_none());
        assert_eq!(rec.info.pending_interactions[0].phase, InteractionPhase::Observed);
        assert_eq!(rec.info.pending_interactions[0].summary.as_deref(), Some("Bash"));

        let notification =
            json!({"hook_event_name":"Notification","session_id":"s","notification_type":"permission_prompt"});
        assert!(translated_interaction(&mut rec, &notification).0);
        assert_eq!(rec.info.pending_interactions[0].phase, InteractionPhase::NeedsUser);
        assert!(!translated_interaction(&mut rec, &notification).0);

        let progress = json!({"hook_event_name":"PostToolUse","session_id":"s"});
        assert!(translated_interaction(&mut rec, &progress).0);
        assert!(rec.info.pending_interactions.is_empty());
    }

    #[test]
    fn claude_notification_without_request_still_records_a_wait() {
        let mut rec = test_record();
        let notification =
            json!({"hook_event_name":"Notification","session_id":"s","notification_type":"permission_prompt"});
        assert!(translated_interaction(&mut rec, &notification).0);
        let pending = &rec.info.pending_interactions[0];
        assert_eq!(pending.id, "s:permission_prompt");
        assert_eq!(pending.phase, InteractionPhase::NeedsUser);
    }

    #[test]
    fn open_code_requests_keep_native_ids_and_options() {
        let mut rec = test_record();
        rec.info.kind = "opencode".into();
        for id in ["p1", "p2"] {
            let request = json!({"v":1,"hook_event_name":"PermissionRequest","session_id":"root",
                "request_session_id":"child","request_id":id,"interaction_kind":"question",
                "question_text":"Continue?","choices":["Yes","No"]});
            let (changed, confirm) = translated_interaction(&mut rec, &request);
            assert!(changed);
            assert_eq!(confirm.unwrap().request_id, id);
        }
        assert_eq!(rec.info.pending_interactions.len(), 2);
        assert_eq!(rec.info.pending_interactions[0].choices, ["Yes", "No"]);
        assert_eq!(rec.info.pending_interactions[0].summary.as_deref(), Some("Continue?"));
        assert_eq!(rec.info.pending_interactions[0].session_id, "child");
        let reply = json!({"v":1,"hook_event_name":"PermissionReplied","request_id":"p1"});
        assert!(translated_interaction(&mut rec, &reply).0);
        assert_eq!(rec.info.pending_interactions[0].id, "p2");
    }

    #[test]
    fn old_or_unknown_opencode_plugin_events_do_not_open_requests() {
        let mut rec = test_record();
        rec.info.kind = "opencode".into();
        let request = json!({"v":2,"hook_event_name":"PermissionRequest","session_id":"s"});
        assert!(!translated_interaction(&mut rec, &request).0);
        assert!(rec.info.pending_interactions.is_empty());
    }

    #[test]
    fn pi_dialog_reports_title_and_closes() {
        let mut rec = test_record();
        rec.info.kind = "pi".into();
        let start = json!({"v":1,"hook_event_name":"PermissionRequest","session_id":"s",
            "interaction_kind":"confirm","question_text":"Allow deployment?"});
        let (changed, confirm) = translated_interaction(&mut rec, &start);
        assert!(changed);
        assert!(confirm.is_none());
        let pending = &rec.info.pending_interactions[0];
        assert_eq!(pending.phase, InteractionPhase::NeedsUser);
        assert_eq!(pending.kind, "confirm");
        assert_eq!(pending.summary.as_deref(), Some("Allow deployment?"));
        let end = json!({"v":1,"hook_event_name":"PermissionReplied","session_id":"s"});
        assert!(translated_interaction(&mut rec, &end).0);
        assert!(rec.info.pending_interactions.is_empty());
    }

    #[test]
    fn omp_parallel_requests_close_by_tool_call_id() {
        let mut rec = test_record();
        rec.info.kind = "omp".into();
        for (id, kind) in [("call-1", "permission"), ("call-2", "question")] {
            let start = json!({"v":1,"hook_event_name":"PermissionRequest","session_id":"s",
                "request_id":id,"interaction_kind":kind,"tool_name":"ask"});
            assert!(translated_interaction(&mut rec, &start).0);
        }
        assert_eq!(rec.info.pending_interactions.len(), 2);
        assert!(rec.info.pending_interactions.iter().all(|p| p.phase == InteractionPhase::NeedsUser));
        let end = json!({"v":1,"hook_event_name":"PermissionReplied","session_id":"s","request_id":"call-1"});
        assert!(translated_interaction(&mut rec, &end).0);
        assert_eq!(rec.info.pending_interactions[0].id, "call-2");
    }

    #[test]
    fn codex_request_stays_observed_until_its_screen_or_progress_settles_it() {
        let mut rec = test_record();
        rec.info.kind = "codex".into();
        let request = json!({"hook_event_name":"PermissionRequest","session_id":"s"});
        let confirm = translated_interaction(&mut rec, &request).1.unwrap();
        assert!(matches!(confirm.how, ConfirmBy::Screen(_)));
        assert_eq!(rec.info.pending_interactions[0].phase, InteractionPhase::Observed);
        let progress = json!({"hook_event_name":"PostToolUse"});
        translated_interaction(&mut rec, &progress);
        assert!(rec.info.pending_interactions.is_empty());
    }
}
