//! `argus wait`: block until one agent, several, or every agent working in a
//! directory reaches a state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use argus_proto::msg::{Activity, AgentInfo, Availability};

use crate::client::Conn;
use crate::errors::{EXITED, STUCK, TIMEOUT, code_of, coded};
use crate::manager::driver;
use crate::naming::self_id;
use crate::output::{self, AgentList, OneAgent};
use crate::query::{Place, Scope};
use crate::stream::Messages;
use crate::{naming, stream};

/// How long `send --then-wait` gives a sent prompt to show up as activity.
const PICKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an agent stays `blocked` before a wait mentions it. Codex reports
/// approvals it then grants by itself, which clear within about a second.
const BLOCKED_NOTICE_DELAY: Duration = Duration::from_secs(2);

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

    /// Whether an agent in `availability` may still reach the goal by itself.
    fn accepts(&self, availability: Availability) -> bool {
        match self {
            Goal::Availability(a) => *a == availability,
            Goal::Activity(a) => a.availability() == availability,
        }
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

pub fn wait(waited: Waited, goal: Goal, after: Option<u64>, timeout: Option<u64>, json: bool) -> Result<()> {
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs(s));
    let me = self_id();
    if let Some(after) = after {
        let Waited::Targets(targets) = waited else { bail!("--after takes one agent, not --dir") };
        let [target] = &targets[..] else { bail!("--after takes one agent: turn numbers are per agent") };
        let agent = Conn::connect()?.find(target)?;
        if me == Some(agent.id) {
            bail!("an agent cannot wait for itself");
        }
        let (agents, rx) = stream::start_watch(Some(vec![agent.id]), true)?;
        let agent = agents.into_iter().next().unwrap_or(agent);
        let agent = after_turn(agent, &rx, after, &goal, deadline, None, !json)?;
        return finish(&[&agent], true, &goal, json);
    }
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
    let mut notices = BlockedNotices::new(&goal, !json);
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
        let watched = match &check {
            Check::Reached(agents) | Check::Pending(agents) => agents,
        };
        notices.observe_all(watched, &table, Instant::now());
        let pending = match check {
            Check::Reached(agents) => return finish(&agents, single, &goal, json),
            Check::Pending(pending) => pending,
        };
        let idle_for = if notices.pending() { Duration::from_secs(1) } else { Duration::from_secs(3600) };
        let wait_for = deadline.map_or(idle_for, |d| d.saturating_duration_since(Instant::now()).min(idle_for));
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

/// A sent prompt, for `after_turn` to notice one the agent never took.
pub struct Pickup {
    pub sent: AgentInfo,
    pub at: Instant,
}

/// Blocks until `current` reaches `goal` with `turns > after`: in a turn that
/// finished after turn `after`. Fails with `stuck` if the turn ends in an error
/// or the agent goes unknown on the way (e.g. interrupted with Esc, which ends
/// a turn without `Stop`). Being unknown already at the start, as every agent
/// is right after a manager restart, does not count. `blocked` keeps waiting,
/// with a notice on stderr when `notify` (see [`BlockedNotices`]).
pub fn after_turn(
    mut current: AgentInfo,
    rx: &Messages,
    after: u64,
    goal: &Goal,
    deadline: Option<Instant>,
    pickup: Option<Pickup>,
    notify: bool,
) -> Result<AgentInfo> {
    if goal.is_exit() {
        bail!("--after waits for a turn; to wait for the process to end, drop --after");
    }
    if !driver::for_kind(&current.kind).has_hooks() {
        bail!("{} has no hooks, so it reports no turns; wait with --until exited", current.name);
    }
    let mut known = false;
    let mut picked_up = pickup.is_none();
    let mut notices = BlockedNotices::new(goal, notify);
    loop {
        notices.observe(&current, Instant::now());
        if current.turns > after && goal.reached(&current) {
            return Ok(current);
        }
        let availability = current.availability();
        let stuck = match availability {
            Availability::Attention => current.activity != Activity::Blocked && !goal.accepts(availability),
            Availability::Unknown => known && !goal.accepts(availability),
            _ => false,
        };
        if stuck {
            return Err(coded(STUCK, format!("{} is {} before finishing the turn", current.name, current.activity)));
        }
        known |= availability != Availability::Unknown;
        if let Some(p) = pickup.as_ref().filter(|_| !picked_up) {
            // A quick turn can go idle → working → done between two updates.
            picked_up = current.turns > after
                || current.activity != p.sent.activity
                || current.activity_since != p.sent.activity_since;
            if !picked_up && p.at.elapsed() > PICKUP_TIMEOUT {
                bail!("{} has not picked up the prompt", current.name);
            }
        }
        if let Err(e) = stream::next_update(rx, &mut current, deadline, Duration::from_secs(1)) {
            if code_of(&e) == TIMEOUT {
                return Err(coded(TIMEOUT, format!("timed out waiting for {} ({})", current.name, current.activity)));
            }
            return Err(e);
        }
    }
}

/// Tells whoever watches a wait, on stderr, when an agent it waits for has
/// reported `blocked` for [`BLOCKED_NOTICE_DELAY`], and when that ends. Waits
/// keep going through `blocked`: argus cannot tell a prompt waiting on a person
/// from one the agent approves by itself, which can stay reported as blocked
/// until the approved tool finishes. Off under `--json`, whose stderr carries
/// only the error object, and when the goal is `blocked` itself.
struct BlockedNotices {
    on: bool,
    /// Agents reporting blocked: since when, and whether that was announced.
    blocked: HashMap<u64, (Instant, bool)>,
}

impl BlockedNotices {
    fn new(goal: &Goal, notify: bool) -> Self {
        BlockedNotices { on: notify && !goal.accepts(Availability::Attention), blocked: HashMap::new() }
    }

    fn observe(&mut self, agent: &AgentInfo, now: Instant) {
        if let Some(line) = self.notice(agent, now) {
            eprintln!("argus: {line}");
        }
    }

    /// Observes `agents`, plus any announced agent no longer among them.
    fn observe_all(&mut self, agents: &[&AgentInfo], table: &BTreeMap<u64, AgentInfo>, now: Instant) {
        let listed: BTreeSet<u64> = agents.iter().map(|a| a.id).collect();
        for agent in agents {
            self.observe(agent, now);
        }
        let gone: Vec<u64> = self.blocked.keys().copied().filter(|id| !listed.contains(id)).collect();
        for id in gone {
            match table.get(&id) {
                Some(agent) => self.observe(agent, now),
                None => {
                    self.blocked.remove(&id);
                }
            }
        }
    }

    /// The line to print for `agent` as of `now`, if any.
    fn notice(&mut self, agent: &AgentInfo, now: Instant) -> Option<String> {
        if !self.on {
            return None;
        }
        if agent.status.is_live() && agent.activity == Activity::Blocked {
            let (since, announced) = self.blocked.entry(agent.id).or_insert((now, false));
            if *announced || now.duration_since(*since) < BLOCKED_NOTICE_DELAY {
                return None;
            }
            *announced = true;
            return Some(format!("{} reports blocked (may be waiting on a person); still waiting", agent.name));
        }
        match self.blocked.remove(&agent.id) {
            Some((since, true)) => {
                Some(format!("{} is no longer blocked (after {}s)", agent.name, now.duration_since(since).as_secs()))
            }
            _ => None,
        }
    }

    /// Whether an agent is blocked but not announced yet, so the wait has to
    /// wake to announce it even without an update.
    fn pending(&self) -> bool {
        self.blocked.values().any(|(_, announced)| !announced)
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

    fn turned(activity: &str, turns: u64) -> AgentInfo {
        AgentInfo { turns, ..agent(activity, true) }
    }

    /// Runs `after_turn(after)` on `start` followed by `updates`, with a
    /// deadline so a wait that never ends times out.
    fn after(start: AgentInfo, updates: &[AgentInfo], after: u64) -> Result<AgentInfo> {
        let (tx, rx) = std::sync::mpsc::channel();
        for (seq, agent) in updates.iter().enumerate() {
            let event = argus_proto::msg::AgentEvent::Updated { agent: agent.clone() };
            tx.send(Ok(Some(argus_proto::msg::Response::Event { epoch: 1, seq: seq as u64, event }))).unwrap();
        }
        let deadline = Some(Instant::now() + Duration::from_millis(50));
        after_turn(start, &rx, after, &Goal::Availability(Availability::Free), deadline, None, false)
    }

    #[test]
    fn after_waits_for_a_later_turn() {
        assert_eq!(after(turned("done", 3), &[], 2).unwrap().turns, 3, "already past it");
        let e = after(turned("idle", 2), &[], 2).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), TIMEOUT, "free, but not after turn 2");
        let turn = [turned("working", 2), turned("tool:Bash", 2), turned("done", 3)];
        assert_eq!(after(turned("idle", 2), &turn, 2).unwrap().turns, 3);
    }

    #[test]
    fn after_fails_when_stuck() {
        let e = after(turned("idle", 2), &[turned("error", 3)], 2).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), STUCK, "a turn that ends in an error");
        let e = after(turned("working", 2), &[turned("unknown", 2)], 2).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), STUCK, "interrupted");
        // Every agent starts unknown after a manager restart.
        let restarted = [turned("working", 2), turned("done", 3)];
        assert_eq!(after(turned("unknown", 2), &restarted, 2).unwrap().turns, 3);
    }

    #[test]
    fn after_waits_through_blocked() {
        let approved = [turned("blocked", 2), turned("tool:Bash", 2), turned("done", 3)];
        assert_eq!(after(turned("working", 2), &approved, 2).unwrap().turns, 3);
        let e = after(turned("working", 2), &[turned("blocked", 2)], 2).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), TIMEOUT);
        assert_eq!(e.to_string(), "timed out waiting for a (blocked)");
    }

    #[test]
    fn blocked_notices() {
        let free = Goal::Availability(Availability::Free);
        let mut n = BlockedNotices::new(&free, true);
        let t = Instant::now();
        let at = |secs: u64| t + Duration::from_secs(secs);
        // A block that clears quickly says nothing.
        assert_eq!(n.notice(&agent("blocked", true), at(0)), None);
        assert!(n.pending());
        assert_eq!(n.notice(&agent("working", true), at(1)), None);
        assert!(!n.pending());
        // One that lasts is announced once, then its end.
        assert_eq!(n.notice(&agent("blocked", true), at(10)), None);
        let line = n.notice(&agent("blocked", true), at(12)).unwrap();
        assert!(line.starts_with("a reports blocked"), "{line}");
        assert_eq!(n.notice(&agent("blocked", true), at(20)), None);
        assert_eq!(n.notice(&agent("done", true), at(55)).unwrap(), "a is no longer blocked (after 45s)");
        assert_eq!(n.notice(&agent("done", true), at(56)), None);

        let mut quiet = BlockedNotices::new(&free, false);
        quiet.notice(&agent("blocked", true), at(0));
        assert_eq!(quiet.notice(&agent("blocked", true), at(10)), None, "off under --json");
        let mut quiet = BlockedNotices::new(&Goal::Availability(Availability::Attention), true);
        quiet.notice(&agent("blocked", true), at(0));
        assert_eq!(quiet.notice(&agent("blocked", true), at(10)), None, "blocked is what it waits for");
    }

    #[test]
    fn after_needs_turns() {
        let generic = AgentInfo { kind: "generic".into(), ..turned("quiet", 0) };
        assert!(after(generic, &[], 0).is_err());
        let exited = Goal::Availability(Availability::Exited);
        let (_tx, rx) = std::sync::mpsc::channel();
        assert!(after_turn(turned("idle", 0), &rx, 0, &exited, None, None, false).is_err());
    }

    #[test]
    fn nothing_to_wait_for_is_reached() {
        assert!(matches!(check_all(vec![], &Goal::Availability(Availability::Free)), Check::Reached(_)));
    }
}
