//! `argus events`: every change to any agent, one line each, as it happens.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;
use argus_proto::msg::{AgentEvent, AgentInfo};
use serde::Serialize;

use crate::output::{self, AgentView};
use crate::watcher::{Update, Watcher};

/// Streams events until interrupted. A closed stdout (e.g. `| head`) ends it
/// quietly.
pub fn events(json: bool) -> Result<()> {
    let mut out = io::stdout().lock();
    let mut watcher = Watcher::start(None, true)?;
    loop {
        let Update::Event(event) = watcher.next(Duration::from_secs(3600))? else { continue };
        let line = if json { event_line(&event) } else { describe(&event) };
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            return Ok(());
        }
    }
}

/// `agent` for changes to one, `id` once it is removed.
#[derive(Serialize)]
struct EventLine<'a> {
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<AgentView<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
}

/// An event as one `--json` line.
fn event_line(event: &AgentEvent) -> String {
    fn line<'a>(event: &'static str, agent: &'a AgentInfo) -> EventLine<'a> {
        EventLine { event, agent: Some(AgentView::new(agent)), id: None }
    }
    output::to_line(match event {
        AgentEvent::Created { agent } => line("created", agent),
        AgentEvent::Updated { agent } => line("updated", agent),
        AgentEvent::Exited { agent } => line("exited", agent),
        AgentEvent::Removed { id } => EventLine { event: "removed", agent: None, id: Some(*id) },
        // Events may have been missed; re-read the list.
        AgentEvent::Resync => EventLine { event: "resync", agent: None, id: None },
    })
}

fn describe(event: &AgentEvent) -> String {
    let line = match event {
        AgentEvent::Created { agent } => format!("created  {:<4} {}", agent.id, agent.name),
        AgentEvent::Updated { agent } => format!(
            "updated  {:<4} {}  {}  activity={} attached={}",
            agent.id,
            agent.name,
            agent.status.as_str(),
            agent.activity,
            agent.attached
        ),
        AgentEvent::Exited { agent } => format!(
            "exited   {:<4} {}  {}",
            agent.id,
            agent.name,
            agent.exit_code.map_or("lost".into(), |c| format!("code {c}"))
        ),
        AgentEvent::Removed { id } => format!("removed  {id}"),
        AgentEvent::Resync => "resync".into(),
    };
    format!("{} {line}", clock())
}

fn clock() -> String {
    // SAFETY: localtime_r fills a local tm from a local time_t.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}
