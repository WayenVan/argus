//! Serving `Watch`: a snapshot, then coalesced change events.
//!
//! Change notifications only carry an agent ID. When flushing, the watcher
//! reads each changed agent's current record and compares it with what this
//! client already knows, which decides between Created / Updated / Exited /
//! Removed. Several changes to one agent inside the flush window become one
//! event.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::Result;
use argus_proto::frame::aio;
use argus_proto::msg::{AgentEvent, AgentInfo, Response};
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::Manager;

/// At most one flush per watcher per this interval.
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

struct Filter {
    ids: Option<Vec<u64>>,
    include_exited: bool,
}

impl Filter {
    fn wants_id(&self, id: u64) -> bool {
        self.ids.as_ref().is_none_or(|ids| ids.contains(&id))
    }

    fn wants(&self, agent: &AgentInfo) -> bool {
        self.wants_id(agent.id) && (self.include_exited || agent.status.is_live())
    }
}

pub async fn serve(
    manager: &Manager,
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    ids: Option<Vec<u64>>,
    include_exited: bool,
) -> Result<()> {
    let filter = Filter { ids, include_exited };
    let epoch = manager.epoch;
    let mut client_gone = [0u8; 64];

    loop {
        // Snapshot and subscription under one lock: nothing slips between them.
        let (seq, agents, mut changes) = {
            let reg = manager.registry.lock().unwrap();
            let agents: Vec<AgentInfo> = reg.infos().filter(|a| filter.wants(a)).cloned().collect();
            (reg.seq, agents, reg.subscribe())
        };
        // id → whether the client last saw it live.
        let mut known: HashMap<u64, bool> = agents.iter().map(|a| (a.id, a.status.is_live())).collect();
        aio::write_json(&mut writer, &Response::Snapshot { epoch, seq, agents }).await?;

        let mut dirty = BTreeSet::new();
        let mut flush_at: Option<Instant> = None;
        let mut last_flush = Instant::now() - FLUSH_INTERVAL;

        let lagged = loop {
            tokio::select! {
                change = changes.recv() => match change {
                    Ok(id) => {
                        if filter.wants_id(id) {
                            dirty.insert(id);
                            flush_at.get_or_insert_with(|| (last_flush + FLUSH_INTERVAL).max(Instant::now()));
                        }
                    }
                    Err(RecvError::Lagged(_)) => break true,
                    Err(RecvError::Closed) => return Ok(()),
                },
                _ = sleep_until(flush_at), if flush_at.is_some() => {
                    let (seq, events) = collect(manager, &filter, &mut known, std::mem::take(&mut dirty));
                    for event in events {
                        aio::write_json(&mut writer, &Response::Event { epoch, seq, event }).await?;
                    }
                    flush_at = None;
                    last_flush = Instant::now();
                },
                // The client never sends anything after Watch; EOF means it left.
                read = reader.read(&mut client_gone) => if matches!(read, Ok(0) | Err(_)) {
                    return Ok(());
                },
            }
        };
        if lagged {
            let seq = manager.registry.lock().unwrap().seq;
            aio::write_json(&mut writer, &Response::Event { epoch, seq, event: AgentEvent::Resync }).await?;
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
}

fn collect(
    manager: &Manager,
    filter: &Filter,
    known: &mut HashMap<u64, bool>,
    dirty: BTreeSet<u64>,
) -> (u64, Vec<AgentEvent>) {
    let reg = manager.registry.lock().unwrap();
    let mut events = Vec::new();
    for id in dirty {
        let was = known.get(&id).copied();
        let Some(agent) = reg.agents.get(&id).map(|r| &r.info) else {
            if known.remove(&id).is_some() {
                events.push(AgentEvent::Removed { id });
            }
            continue;
        };
        let live = agent.status.is_live();
        let event = match (was, live) {
            (None, _) if !filter.wants(agent) => continue,
            (None, _) => AgentEvent::Created { agent: agent.clone() },
            (Some(true), false) => AgentEvent::Exited { agent: agent.clone() },
            (Some(false), _) if !filter.include_exited => continue,
            _ => AgentEvent::Updated { agent: agent.clone() },
        };
        if live || filter.include_exited {
            known.insert(id, live);
        } else {
            known.remove(&id);
        }
        events.push(event);
    }
    (reg.seq, events)
}
