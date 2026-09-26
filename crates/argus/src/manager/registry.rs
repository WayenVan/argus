//! The agent registry: persisted index plus change notifications for watchers.
//!
//! Each agent is an [`AgentRecord`]: the public [`AgentInfo`] (persisted and
//! sent to clients) plus [`AgentRuntime`], state that only means something to
//! this manager process and is rebuilt from scratch after a restart.

use std::collections::BTreeMap;
use std::fs;
use std::time::Instant;

use anyhow::{Context, Result};
use argus_proto::msg::{Activity, AgentInfo};
use argus_proto::paths;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// How many unread change notifications a watcher may fall behind by before
/// it is told to resync.
const CHANGE_BACKLOG: usize = 4096;

pub struct AgentRecord {
    pub info: AgentInfo,
    pub runtime: AgentRuntime,
}

#[derive(Default)]
pub struct AgentRuntime {
    /// Attached terminals that currently have focus (from the holder).
    pub focused: u32,
    /// The agent session hook reports are bound to; others are dropped.
    pub session_id: Option<String>,
    /// Last tool seen, restored when a permission prompt is answered.
    pub last_tool: Option<String>,
    /// Silence watchdog: when to look at the output offset next.
    pub deadline: Option<Instant>,
    /// Whether a watchdog task is running for this agent.
    pub watchdog: bool,
    /// Bumped each time the agent enters `working`, so the watchdog takes a
    /// fresh output baseline for every working period.
    pub working_gen: u64,
    /// Bumped each time the agent shows or hides its cursor, so a pending
    /// `ready_on_cursor` check can tell whether it stayed up.
    pub cursor_gen: u64,
    /// When an attached terminal last typed into the agent.
    pub last_input: Option<Instant>,
    /// Set by `argus send` until the agent leaves `idle`/`done` (the prompt
    /// was submitted) or this deadline passes, so a second send cannot land
    /// on top of the first.
    pub submitting: Option<Instant>,
    /// The `turns` value at which a `Stop` was last held back with
    /// directives, so a turn is held at most once.
    pub held_turn: Option<u64>,
    /// The prompt of the turn in progress, for the turn log.
    pub prompt: Option<String>,
    /// The reply a held-back `Stop` carried: the turn's real answer, which
    /// the reply after the directives only adds to.
    pub held_reply: Option<String>,
}

impl AgentRecord {
    pub fn new(info: AgentInfo) -> AgentRecord {
        AgentRecord { info, runtime: AgentRuntime::default() }
    }
}

pub struct Registry {
    pub next_id: u64,
    pub agents: BTreeMap<u64, AgentRecord>,
    /// Activities the previous manager left agents in, for `recover` to
    /// restore if the agent printed nothing since (see [`Settled`]).
    pub restore: BTreeMap<u64, Restore>,
    /// Bumped on every change; watch events carry it.
    pub seq: u64,
    changes: broadcast::Sender<u64>,
}

#[derive(Serialize, Deserialize)]
struct RegistryFile {
    next_id: u64,
    agents: Vec<AgentInfo>,
    /// Written only by a manager stopping cleanly: the output offset of each
    /// agent it left at its prompt. Any later save drops it, so a manager
    /// that crashes afterwards leaves nothing stale behind.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    settled: Settled,
}

/// Agent ID → how many bytes of output it had produced when the manager
/// stopped with the agent at its prompt.
pub type Settled = BTreeMap<u64, u64>;

/// An activity to restore once the holder confirms `offset` is still where
/// output ends: an agent that printed nothing cannot have moved on.
pub struct Restore {
    pub activity: Activity,
    pub since: Option<u64>,
    pub offset: u64,
}

/// Clears what a new manager cannot trust in a loaded agent: holders report
/// the real attach count on subscribe, and hook state from before the restart
/// may be stale. Returns the activity to restore if the agent was left at its
/// prompt by a manager that stopped cleanly.
fn reset(info: &mut AgentInfo, settled: &Settled) -> Option<Restore> {
    info.attached = 0;
    info.tmux_locations.clear();
    if !info.status.is_live() {
        return None;
    }
    let activity = std::mem::replace(&mut info.activity, Activity::Unknown);
    let since = info.activity_since.take();
    let &offset = settled.get(&info.id)?;
    activity.awaits_prompt().then_some(Restore { activity, since, offset })
}

impl Registry {
    pub fn load() -> Result<Registry> {
        let path = paths::registry_file();
        let file: RegistryFile = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                RegistryFile { next_id: 1, agents: vec![], settled: Settled::new() }
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut restore = BTreeMap::new();
        let agents: BTreeMap<u64, AgentRecord> = file
            .agents
            .into_iter()
            .map(|mut info| {
                if let Some(r) = reset(&mut info, &file.settled) {
                    restore.insert(info.id, r);
                }
                (info.id, AgentRecord::new(info))
            })
            .collect();
        // Never hand out an ID twice, even if the counter was lost.
        let next_id = file.next_id.max(agents.keys().max().map_or(1, |m| m + 1));
        let (changes, _) = broadcast::channel(CHANGE_BACKLOG);
        Ok(Registry { next_id, agents, restore, seq: 0, changes })
    }

    pub fn save(&self) -> Result<()> {
        self.save_with(Settled::new())
    }

    /// The last save of a manager stopping cleanly; see [`RegistryFile::settled`].
    pub fn save_settled(&self, settled: Settled) -> Result<()> {
        self.save_with(settled)
    }

    fn save_with(&self, settled: Settled) -> Result<()> {
        let path = paths::registry_file();
        let tmp = path.with_extension("json.tmp");
        let agents = self
            .infos()
            .cloned()
            .map(|mut info| {
                info.tmux_locations.clear();
                info
            })
            .collect();
        let file = RegistryFile { next_id: self.next_id, agents, settled };
        fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn infos(&self) -> impl Iterator<Item = &AgentInfo> + Clone {
        self.agents.values().map(|r| &r.info)
    }

    pub fn info(&self, id: u64) -> &AgentInfo {
        &self.agents[&id].info
    }

    /// Records that agent `id` changed (or was removed) and wakes watchers.
    pub fn changed(&mut self, id: u64) {
        self.seq += 1;
        // No receivers just means nobody is watching.
        let _ = self.changes.send(id);
    }

    /// Subscribes to changes. Taken under the same lock as a snapshot, so no
    /// change can fall between the two.
    pub fn subscribe(&self) -> broadcast::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Only a *live* agent reserves its name: once it exits, the name is free
    /// again for reuse (an exited record hangs around, addressable by ID,
    /// until `argus rm`, but it no longer squats on the name).
    pub fn name_taken(&self, name: &str) -> bool {
        self.infos().any(|a| a.name == name && a.status.is_live())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::AgentStatus;

    fn agent(id: u64, name: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            id,
            name: name.into(),
            kind: "claude".into(),
            command: vec!["claude".into()],
            cwd: "/".into(),
            created_at: 0,
            exited_at: None,
            holder_pid: None,
            agent_pid: None,
            status,
            exit_code: None,
            activity: "unknown".into(),
            activity_since: None,
            turns: 0,
            attached: 0,
            tmux_locations: Vec::new(),
            labels: Default::default(),
        }
    }

    fn registry(agents: Vec<AgentInfo>) -> Registry {
        let (changes, _) = broadcast::channel(CHANGE_BACKLOG);
        Registry {
            next_id: agents.iter().map(|a| a.id + 1).max().unwrap_or(1),
            agents: agents.into_iter().map(|a| (a.id, AgentRecord::new(a))).collect(),
            restore: BTreeMap::new(),
            seq: 0,
            changes,
        }
    }

    #[test]
    fn an_exited_agents_name_is_free_again() {
        let reg = registry(vec![agent(1, "codex-1", AgentStatus::Exited)]);
        assert!(!reg.name_taken("codex-1"));
    }

    #[test]
    fn a_clean_stop_restores_agents_left_at_their_prompt() {
        let settled = Settled::from([(1, 500)]);
        let mut done =
            AgentInfo { activity: "done".into(), activity_since: Some(7), ..agent(1, "a", AgentStatus::Running) };
        let r = reset(&mut done, &settled).unwrap();
        assert_eq!((r.activity, r.since, r.offset), (Activity::Done, Some(7), 500));
        assert_eq!((done.activity, done.activity_since), (Activity::Unknown, None), "unknown until confirmed");

        let mut working = AgentInfo { activity: "working".into(), ..agent(1, "a", AgentStatus::Running) };
        assert!(reset(&mut working, &settled).is_none(), "mid-turn");
        let mut unsettled = AgentInfo { activity: "idle".into(), ..agent(2, "b", AgentStatus::Running) };
        assert!(reset(&mut unsettled, &settled).is_none(), "crashed manager: no offset");
        assert_eq!(unsettled.activity, Activity::Unknown);
    }

    #[test]
    fn a_live_agents_name_stays_taken() {
        let reg = registry(vec![agent(1, "codex-1", AgentStatus::Running)]);
        assert!(reg.name_taken("codex-1"));
    }
}
