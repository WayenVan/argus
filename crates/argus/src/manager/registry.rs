//! The agent registry: persisted index plus change notifications for watchers.
//!
//! Each agent is an [`AgentRecord`]: the public [`AgentInfo`] (persisted and
//! sent to clients) plus [`AgentRuntime`], state that only means something to
//! this manager process and is rebuilt from scratch after a restart.
//!
//! Records change only through [`Registry::create`], [`Registry::edit`] and
//! [`Registry::remove`]. Each compares the record before and after: watchers
//! hear of any change to its `AgentInfo`, and the file is rewritten when a
//! durable field changed (see [`durable`]).

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use argus_proto::msg::{Activity, AgentInfo};
use argus_proto::paths;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::activity::Watchdog;
use super::hooks::Session;
use super::log;
use super::send::SendGate;
use super::turn::TurnState;

/// How many unread change notifications a watcher may fall behind by before
/// it is told to resync.
const CHANGE_BACKLOG: usize = 4096;

pub struct AgentRecord {
    pub info: AgentInfo,
    pub runtime: AgentRuntime,
}

/// Each part belongs to the module that keeps it up to date.
#[derive(Default)]
pub struct AgentRuntime {
    /// Attached terminals that currently have focus (from the holder).
    pub focused: u32,
    /// Bumped each time the agent shows or hides its cursor, so a pending
    /// `ready_on_cursor` check can tell whether it stayed up.
    pub cursor_gen: u64,
    pub session: Session,
    pub turn: TurnState,
    pub watchdog: Watchdog,
    pub send: SendGate,
}

impl AgentRecord {
    pub fn new(info: AgentInfo) -> AgentRecord {
        AgentRecord { info, runtime: AgentRuntime::default() }
    }
}

pub struct Registry {
    next_id: u64,
    agents: BTreeMap<u64, AgentRecord>,
    /// Activities the previous manager left agents in, for `recover` to
    /// restore if the agent printed nothing since (see [`Settled`]).
    restore: BTreeMap<u64, Restore>,
    /// Bumped on every change; watch events carry it.
    seq: u64,
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

/// Clears what only a running agent watched by this manager has: attached
/// terminals and pending requests. Neither survives the agent's exit, and a
/// restarted manager learns them afresh (holders report attachments on
/// subscribe; hooks report new requests).
pub fn clear_volatile(info: &mut AgentInfo) {
    info.attached = 0;
    info.tmux_locations.clear();
    info.pending_interactions.clear();
}

/// `info` without the fields a restarted manager does not trust (see
/// [`reset`]): a change to anything else is worth writing to disk. Activity
/// is only worth keeping at a clean stop, which saves it with
/// [`Registry::save_settled`].
fn durable(info: &AgentInfo) -> AgentInfo {
    let mut durable = AgentInfo { activity: Activity::Unknown, activity_since: None, ..info.clone() };
    clear_volatile(&mut durable);
    durable
}

/// Clears what a new manager cannot trust in a loaded agent: holders report
/// the real attach count on subscribe, and hook state from before the restart
/// may be stale. Returns the activity to restore if the agent was left at its
/// prompt by a manager that stopped cleanly.
fn reset(info: &mut AgentInfo, settled: &Settled) -> Option<Restore> {
    clear_volatile(info);
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

    /// Adds an agent under a fresh ID; `build` makes its record from the ID.
    pub fn create(&mut self, build: impl FnOnce(u64) -> AgentInfo) -> AgentInfo {
        let id = self.next_id;
        self.next_id += 1;
        let info = build(id);
        self.agents.insert(id, AgentRecord::new(info.clone()));
        self.changed(id);
        self.persist();
        info
    }

    /// Runs `f` on agent `id`'s record, then tells watchers if its info
    /// changed and saves if a durable part of it did. `None` if there is no
    /// such agent.
    pub fn edit<R>(&mut self, id: u64, f: impl FnOnce(&mut AgentRecord) -> R) -> Option<R> {
        let rec = self.agents.get_mut(&id)?;
        let before = rec.info.clone();
        let out = f(rec);
        if rec.info != before {
            let durable_changed = durable(&rec.info) != durable(&before);
            self.changed(id);
            if durable_changed {
                self.persist();
            }
        }
        Some(out)
    }

    pub fn remove(&mut self, id: u64) -> Option<AgentInfo> {
        let rec = self.agents.remove(&id)?;
        self.changed(id);
        self.persist();
        Some(rec.info)
    }

    /// Activities to restore after a restart; see [`Restore`]. Empty after
    /// the first call.
    pub fn take_restore(&mut self) -> BTreeMap<u64, Restore> {
        std::mem::take(&mut self.restore)
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// A failed save is logged, not returned: the change already took effect
    /// in memory, and the next save writes it again.
    fn persist(&self) {
        if let Err(e) = self.save_with(Settled::new()) {
            log(&format!("saving the registry: {e:#}"));
        }
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

    pub fn get(&self, id: u64) -> Option<&AgentRecord> {
        self.agents.get(&id)
    }

    /// Records that agent `id` changed (or was removed) and wakes watchers.
    fn changed(&mut self, id: u64) {
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

/// A running Claude agent that has reported nothing yet, for tests.
#[cfg(test)]
pub fn test_record() -> AgentRecord {
    AgentRecord::new(tests::agent(1, "claude-1", argus_proto::msg::AgentStatus::Running))
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::AgentStatus;

    pub fn agent(id: u64, name: &str, status: AgentStatus) -> AgentInfo {
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
            pending_interactions: Vec::new(),
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
    fn pending_interactions_are_discarded_on_restart() {
        let mut info = agent(1, "a", AgentStatus::Running);
        info.pending_interactions.push(argus_proto::msg::PendingInteraction {
            id: "native".into(),
            kind: "permission".into(),
            phase: argus_proto::msg::InteractionPhase::NeedsUser,
            session_id: "s".into(),
            summary: None,
            choices: vec![],
            created_at: 1,
        });
        reset(&mut info, &Settled::new());
        assert!(info.pending_interactions.is_empty());
        assert_eq!(info.activity, Activity::Unknown);
    }

    #[test]
    fn edits_notify_only_when_the_info_changes() {
        let mut reg = registry(vec![agent(1, "a", AgentStatus::Running)]);
        let mut changes = reg.subscribe();
        reg.edit(1, |rec| rec.runtime.focused = 1);
        assert!(changes.try_recv().is_err(), "runtime-only state is not news");
        reg.edit(1, |rec| rec.info.activity = Activity::Working);
        assert_eq!(changes.try_recv().unwrap(), 1);
        assert_eq!(reg.seq(), 1);
        assert_eq!(reg.edit(2, |_| ()), None, "no such agent");
    }

    #[test]
    fn only_what_a_restart_keeps_is_durable() {
        let before = agent(1, "a", AgentStatus::Running);
        let volatile = AgentInfo {
            activity: Activity::Working,
            activity_since: Some(5),
            attached: 2,
            tmux_locations: vec![argus_proto::msg::TmuxLocation { socket: "s".into(), pane: "%1".into() }],
            ..before.clone()
        };
        assert_eq!(durable(&volatile), durable(&before));
        for changed in [
            AgentInfo { turns: 1, ..before.clone() },
            AgentInfo { name: "b".into(), ..before.clone() },
            AgentInfo { status: AgentStatus::Exited, ..before.clone() },
            AgentInfo { labels: [("title".into(), "t".into())].into(), ..before.clone() },
        ] {
            assert_ne!(durable(&changed), durable(&before), "{changed:?}");
        }
    }

    #[test]
    fn a_live_agents_name_stays_taken() {
        let reg = registry(vec![agent(1, "codex-1", AgentStatus::Running)]);
        assert!(reg.name_taken("codex-1"));
    }
}
