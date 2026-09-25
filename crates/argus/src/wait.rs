//! `argus wait`: block until one agent, several, or every agent working in a
//! directory reaches a state.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use argus_proto::msg::{Activity, AgentInfo, Availability};

use crate::client::Conn;
use crate::errors::{EXITED, TIMEOUT, coded};
use crate::naming::self_id;
use crate::output::{self, AgentList, OneAgent};
use crate::query::{Place, Scope};
use crate::{naming, stream};

/// What `--until` or `--until-activity` asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Goal {
    /// Any activity mapping to it, or `exited`.
    Availability(Availability),
    /// One exact activity, e.g. `done` or `tool:Bash`.
    Activity(Activity),
}

/// An availability; an activity's name gets pointed to `--until-activity`.
pub fn parse_availability(s: &str) -> Result<Availability, String> {
    s.parse().map_err(|e: String| match parse_activity(s) {
        Ok(_) => format!("{s} is an activity, not an availability; use free, or --until-activity {s}"),
        Err(_) => e,
    })
}

/// An activity `argus` knows by name; `unknown` only as itself.
pub fn parse_activity(s: &str) -> Result<Activity, String> {
    let activity = Activity::from(s);
    if activity.to_string() != s {
        return Err(format!("expected an activity such as idle, done or tool:Bash; got {s:?}"));
    }
    Ok(activity)
}

impl Goal {
    fn is_exit(&self) -> bool {
        *self == Goal::Availability(Availability::Exited)
    }

    fn reached(&self, agent: &AgentInfo) -> bool {
        match self {
            Goal::Availability(a) => agent.availability() == *a,
            Goal::Activity(a) => agent.status.is_live() && agent.activity == *a,
        }
    }
}

/// Which agents to wait for.
pub enum Waited {
    /// These targets; each may name several (`group/**`).
    Targets(Vec<String>),
    /// Every running agent in a directory other than the caller, as
    /// `argus status` lists them, re-evaluated as agents come and go.
    Dir { path: Option<PathBuf>, scope: Scope, labels: Vec<(String, String)> },
}

enum Check<'a> {
    Reached(Vec<&'a AgentInfo>),
    Pending(Vec<&'a AgentInfo>),
}

pub fn wait(waited: Waited, goal: Goal, timeout: Option<u64>, json: bool) -> Result<()> {
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs(s));
    let me = self_id();
    let (ids, place, labels) = match waited {
        Waited::Targets(targets) => {
            let agents = Conn::connect()?.list(true)?;
            let mut ids = Vec::new();
            for target in &targets {
                ids.extend(naming::resolve(target, agents.iter())?);
            }
            ids.sort_unstable();
            ids.dedup();
            if me.is_some_and(|me| ids.contains(&me)) {
                bail!("an agent cannot wait for itself");
            }
            (Some(ids), None, Vec::new())
        }
        Waited::Dir { path, scope, labels } => (None, Some(Place::new(path, scope)?), labels),
    };
    let single = ids.as_ref().is_some_and(|ids| ids.len() == 1);
    let (agents, rx) = stream::start_watch(ids.clone(), true)?;
    let mut table: BTreeMap<u64, AgentInfo> = agents.into_iter().map(|a| (a.id, a)).collect();
    // Whether an agent is in the directory; its cwd never changes.
    let mut inside: HashMap<u64, bool> = HashMap::new();
    loop {
        let check = match &ids {
            Some(ids) => check_targets(ids, &table, &goal)?,
            None => {
                let place = place.as_ref().expect("set with no targets");
                let members: Vec<&AgentInfo> = table
                    .values()
                    .filter(|a| a.status.is_live() && Some(a.id) != me)
                    .filter(|a| labels.iter().all(|(k, v)| a.labels.get(k) == Some(v)))
                    .filter(|a| *inside.entry(a.id).or_insert_with(|| place.contains(a)))
                    .collect();
                check_all(members, &goal)
            }
        };
        let pending = match check {
            Check::Reached(agents) => return finish(&agents, single, &goal, json),
            Check::Pending(pending) => pending,
        };
        let wait_for = deadline.map_or(Duration::from_secs(3600), |d| d.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(wait_for) {
            Ok(Ok(Some(msg))) => stream::apply(&mut table, &msg),
            Err(RecvTimeoutError::Timeout) if deadline.is_some_and(|d| Instant::now() >= d) => {
                let names: Vec<String> = pending.iter().map(|a| format!("{} ({})", a.name, a.activity)).collect();
                return Err(coded(TIMEOUT, format!("timed out waiting for {}", names.join(", "))));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Err(e)) => return Err(e),
            Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => bail!("lost connection to the manager"),
        }
    }
}

/// Named targets must each still exist and, unless waiting for that, run.
fn check_targets<'a>(ids: &[u64], table: &'a BTreeMap<u64, AgentInfo>, goal: &Goal) -> Result<Check<'a>> {
    let mut agents = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(agent) = table.get(id) else {
            return Err(coded(EXITED, format!("agent {id} was removed")));
        };
        if !agent.status.is_live() && !goal.is_exit() {
            return Err(coded(
                EXITED,
                format!("{} {} before becoming {}", agent.name, agent.status.as_str(), label(goal)),
            ));
        }
        agents.push(agent);
    }
    Ok(check_all(agents, goal))
}

fn check_all<'a>(agents: Vec<&'a AgentInfo>, goal: &Goal) -> Check<'a> {
    let pending: Vec<&AgentInfo> = agents.iter().copied().filter(|a| !goal.reached(a)).collect();
    if pending.is_empty() { Check::Reached(agents) } else { Check::Pending(pending) }
}

fn label(goal: &Goal) -> String {
    match goal {
        Goal::Availability(a) => a.to_string(),
        Goal::Activity(a) => a.to_string(),
    }
}

/// One named agent prints what it reached (or its exit code, which becomes
/// ours); several print a line each.
fn finish(agents: &[&AgentInfo], single: bool, goal: &Goal, json: bool) -> Result<()> {
    if let ([agent], true) = (agents, single) {
        let text = match agent.exit_code {
            Some(code) if goal.is_exit() => code.to_string(),
            None if goal.is_exit() => agent.status.as_str().to_string(),
            _ => label(goal),
        };
        if json {
            output::print(OneAgent::new(agent));
        } else {
            println!("{text}");
        }
        if goal.is_exit() {
            std::process::exit(agent.exit_code.map_or(255, |c| c.clamp(0, 255)));
        }
        return Ok(());
    }
    if json {
        let owned: Vec<AgentInfo> = agents.iter().map(|a| (*a).clone()).collect();
        output::print(AgentList::new(&owned));
    } else {
        for a in agents {
            let state = if a.status.is_live() { a.activity.to_string() } else { a.status.as_str().to_string() };
            println!("{}\t{}\t{state}", a.id, a.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(activity: &str, live: bool) -> AgentInfo {
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": "a", "kind": "claude", "command": [], "cwd": "/",
            "created_at": 0, "status": if live { "running" } else { "exited" }, "activity": activity
        }))
        .unwrap()
    }

    #[test]
    fn goals() {
        assert_eq!(parse_activity("done"), Ok(Activity::Done));
        assert_eq!(parse_activity("tool:Bash"), Ok(Activity::Tool("Bash".into())));
        assert!(parse_activity("waiting").is_err());
        assert!("idle".parse::<Availability>().is_err());

        let free = Goal::Availability(Availability::Free);
        assert!(free.reached(&agent("done", true)) && free.reached(&agent("quiet", true)));
        assert!(!free.reached(&agent("blocked", true)) && !free.reached(&agent("idle", false)));
        assert!(!Goal::Activity(Activity::Done).reached(&agent("idle", true)));
        let exited = Goal::Availability(Availability::Exited);
        assert!(exited.reached(&agent("idle", false)) && !exited.reached(&agent("idle", true)));
    }

    #[test]
    fn a_named_agent_that_exits_ends_the_wait() {
        let table = BTreeMap::from([(1, agent("idle", false))]);
        let e = check_targets(&[1], &table, &Goal::Availability(Availability::Free)).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), EXITED);
        assert!(matches!(
            check_targets(&[1], &table, &Goal::Availability(Availability::Exited)),
            Ok(Check::Reached(_))
        ));
        let e = check_targets(&[2], &table, &Goal::Availability(Availability::Exited)).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), EXITED);
    }

    #[test]
    fn nothing_to_wait_for_is_reached() {
        assert!(matches!(check_all(vec![], &Goal::Availability(Availability::Free)), Check::Reached(_)));
    }
}
