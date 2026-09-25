//! `argus send`: typing a prompt into an agent as a person at its keyboard
//! would, but only when the agent is waiting for one.
//!
//! The check and the typing both happen here, not in the client, so nothing
//! can change between them: the registry lock is held while deciding, and
//! `submitting` keeps a second send out until the agent has picked up the
//! first (its activity leaves `idle`/`done`) or `SUBMIT_TIMEOUT` passes.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use argus_proto::msg::{Activity, Response};

use super::registry::AgentRecord;
use super::{Manager, holder, log, resolve_one};

/// Input from an attached terminal this recent means someone is typing.
const TYPING_GRACE: Duration = Duration::from_secs(10);
/// How long a submitted prompt may take to show up as activity.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Enter goes in as a separate write after this pause, so TUIs see a
/// keypress rather than a line ending inside typed or pasted text.
const ENTER_DELAY: Duration = Duration::from_millis(30);
const PASTE_ENTER_DELAY: Duration = Duration::from_millis(150);

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

impl Manager {
    pub(super) async fn send(&self, target: &str, text: String, enter: bool, force: bool) -> Result<Response> {
        let (id, name, turn) = {
            let mut reg = self.registry.lock().unwrap();
            let id = resolve_one(&reg, target, "send")?;
            let rec = reg.agents.get_mut(&id).expect("resolved");
            if !rec.info.status.is_live() {
                bail!("{} is not running", rec.info.name);
            }
            let now = Instant::now();
            if let Some(reason) = refusal(rec, force, now) {
                return Ok(Response::error(crate::errors::NOT_READY, format!("{} {reason}", rec.info.name)));
            }
            rec.runtime.submitting = Some(now + SUBMIT_TIMEOUT);
            (id, rec.info.name.clone(), rec.info.turns)
        };
        let typed = self.type_text(id, &text, enter).await;
        if typed.is_err() || !enter {
            // Nothing was submitted, so there is nothing to wait for.
            if let Some(rec) = self.registry.lock().unwrap().agents.get_mut(&id) {
                rec.runtime.submitting = None;
            }
        }
        typed?;
        if enter {
            log(&format!("sent a prompt to {name} (id {id})"));
        }
        Ok(Response::Sent { turn })
    }

    async fn type_text(&self, id: u64, text: &str, enter: bool) -> Result<()> {
        let multiline = text.contains(['\n', '\r']);
        let paste = self.screens.bracketed_paste(id) == Some(true);
        if multiline && !paste {
            bail!("multi-line text needs an agent that accepts pastes (bracketed paste is off)");
        }
        let pasted = paste && !text.is_empty();
        if pasted {
            // A line break inside the text would press Enter; pasting it
            // keeps it as text. Single lines are pasted too: typed fast, they
            // look like a paste the agent must guess the end of (Codex takes
            // an Enter right after one as a new line). The end marker must
            // not end the paste early.
            let body = text.replace(PASTE_END, "");
            holder::write(id, format!("{PASTE_START}{body}{PASTE_END}")).await?;
        } else if !text.is_empty() {
            holder::write(id, text.to_string()).await?;
        }
        if enter {
            if !text.is_empty() {
                tokio::time::sleep(if pasted { PASTE_ENTER_DELAY } else { ENTER_DELAY }).await;
            }
            holder::write(id, "\r".into()).await?;
        }
        Ok(())
    }
}

/// Why typing into `rec` now would be unsafe, if it would.
fn refusal(rec: &AgentRecord, force: bool, now: Instant) -> Option<String> {
    let refusal = rec.info.activity.send_refusal();
    // Even forced: the keypress would answer the permission prompt.
    if rec.info.activity == Activity::Blocked || (refusal.is_some() && !force) {
        return refusal;
    }
    if force {
        return None;
    }
    if rec.info.attached > 0 && rec.runtime.last_input.is_some_and(|t| now < t + TYPING_GRACE) {
        return Some("has someone typing in an attached terminal".into());
    }
    if rec.runtime.submitting.is_some_and(|deadline| now < deadline) {
        return Some("is still taking an earlier prompt".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::{AgentInfo, AgentStatus};

    fn record(activity: &str) -> AgentRecord {
        AgentRecord::new(AgentInfo {
            id: 1,
            name: "a".into(),
            kind: "claude".into(),
            command: vec![],
            cwd: "/".into(),
            created_at: 0,
            exited_at: None,
            holder_pid: None,
            agent_pid: None,
            status: AgentStatus::Running,
            exit_code: None,
            activity: activity.into(),
            activity_since: None,
            turns: 0,
            attached: 0,
            tmux_locations: Vec::new(),
            labels: Default::default(),
        })
    }

    #[test]
    fn only_at_a_prompt() {
        let now = Instant::now();
        for ok in ["idle", "done", "error"] {
            assert_eq!(refusal(&record(ok), false, now), None, "{ok}");
        }
        for busy in ["working", "tool:Bash", "unknown", "starting", "busy", "quiet"] {
            assert!(refusal(&record(busy), false, now).is_some(), "{busy}");
            assert_eq!(refusal(&record(busy), true, now), None, "--force {busy}");
        }
        assert!(refusal(&record("blocked"), true, now).is_some(), "blocked even with --force");
    }

    #[test]
    fn typing_and_submitting() {
        let now = Instant::now();
        let mut rec = record("idle");
        rec.runtime.last_input = Some(now);
        assert_eq!(refusal(&rec, false, now), None, "detached input does not count");
        rec.info.attached = 1;
        assert!(refusal(&rec, false, now).is_some());
        assert_eq!(refusal(&rec, false, now + TYPING_GRACE), None);

        let mut rec = record("idle");
        rec.runtime.submitting = Some(now + SUBMIT_TIMEOUT);
        assert!(refusal(&rec, false, now).is_some());
        assert_eq!(refusal(&rec, false, now + SUBMIT_TIMEOUT), None);
    }
}
