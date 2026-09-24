//! The agent registry: persisted index plus change notifications for watchers.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use argus_proto::msg::AgentInfo;
use argus_proto::paths;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// How many unread change notifications a watcher may fall behind by before
/// it is told to resync.
const CHANGE_BACKLOG: usize = 4096;

pub struct Registry {
    pub next_id: u64,
    pub agents: BTreeMap<u64, AgentInfo>,
    /// Bumped on every change; watch events carry it.
    pub seq: u64,
    changes: broadcast::Sender<u64>,
}

#[derive(Serialize, Deserialize)]
struct RegistryFile {
    next_id: u64,
    agents: Vec<AgentInfo>,
}

impl Registry {
    pub fn load() -> Result<Registry> {
        let path = paths::registry_file();
        let file: RegistryFile = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RegistryFile { next_id: 1, agents: vec![] },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut agents: BTreeMap<u64, AgentInfo> = file.agents.into_iter().map(|a| (a.id, a)).collect();
        for agent in agents.values_mut() {
            agent.attached = 0; // Holders report the real count on subscribe.
        }
        // Never hand out an ID twice, even if the counter was lost.
        let next_id = file.next_id.max(agents.keys().max().map_or(1, |m| m + 1));
        let (changes, _) = broadcast::channel(CHANGE_BACKLOG);
        Ok(Registry { next_id, agents, seq: 0, changes })
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::registry_file();
        let tmp = path.with_extension("json.tmp");
        let file = RegistryFile { next_id: self.next_id, agents: self.agents.values().cloned().collect() };
        fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
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

    pub fn name_taken(&self, name: &str) -> bool {
        self.agents.values().any(|a| a.name == name)
    }
}
