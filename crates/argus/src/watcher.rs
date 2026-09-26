//! A live copy of the manager's agent table, kept current by a `Watch`.
//!
//! Every long-running client (`ps -w`, `events`, `wait`, `send --wait`, the
//! dashboards) reads agents through a [`Watcher`]. The manager may go away at
//! any time (`argus manager restart`, an upgrade, a crash): the watcher then
//! connects again, starting a manager if none is back, and replaces its table
//! with the new manager's snapshot.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::msg::{AgentEvent, AgentInfo, Request, Response};

use crate::client::Conn;

/// Pause before connecting again, so a manager that is shutting down has
/// time to let go of its socket.
const RECONNECT_PAUSE: Duration = Duration::from_millis(200);
/// Connection attempts after losing the manager before giving up.
const RECONNECT_TRIES: u32 = 3;

/// Watch messages, as a reader thread received them.
type Messages = Receiver<Result<Option<Response>>>;

pub struct Watcher {
    ids: Option<Vec<u64>>,
    include_exited: bool,
    table: BTreeMap<u64, AgentInfo>,
    /// Messages read by a thread that owns the connection, so waiting with a
    /// timeout never breaks a frame apart.
    rx: Messages,
}

/// What [`Watcher::next`] saw. Consumed at once, never stored, so the size
/// of an event is not worth a box.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Update {
    /// Nothing within the wait.
    Nothing,
    /// A change, already applied to the table. `Resync` changes nothing
    /// itself: the snapshot after it arrives as a `Reset`.
    Event(AgentEvent),
    /// The table was replaced by a fresh snapshot: the manager restarted, or
    /// this watcher fell behind. What the previous state implied may no
    /// longer hold; a restarted manager, for one, knows every activity only
    /// as `unknown` until the agent's next hook.
    Reset,
}

impl Watcher {
    /// Watches the agents with these IDs, or all of them; exited ones only
    /// with `include_exited`.
    pub fn start(ids: Option<Vec<u64>>, include_exited: bool) -> Result<Watcher> {
        let (table, rx) = subscribe(ids.clone(), include_exited)?;
        Ok(Watcher { ids, include_exited, table, rx })
    }

    pub fn agents(&self) -> &BTreeMap<u64, AgentInfo> {
        &self.table
    }

    pub fn get(&self, id: u64) -> Option<&AgentInfo> {
        self.table.get(&id)
    }

    /// Waits at most `wait` for the next message and applies it.
    pub fn next(&mut self, wait: Duration) -> Result<Update> {
        match self.rx.recv_timeout(wait) {
            Ok(Ok(Some(msg))) => Ok(self.apply(msg)),
            Err(RecvTimeoutError::Timeout) => Ok(Update::Nothing),
            Ok(Ok(None) | Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                self.reconnect()?;
                Ok(Update::Reset)
            }
        }
    }

    /// Applies every message already received, without waiting. Returns
    /// whether any arrived.
    pub fn catch_up(&mut self) -> Result<bool> {
        let mut any = false;
        while !matches!(self.next(Duration::ZERO)?, Update::Nothing) {
            any = true;
        }
        Ok(any)
    }

    fn apply(&mut self, msg: Response) -> Update {
        match msg {
            Response::Snapshot { agents, .. } => {
                self.table = table_of(agents);
                Update::Reset
            }
            Response::Event { event, .. } => {
                match &event {
                    AgentEvent::Created { agent } | AgentEvent::Updated { agent } | AgentEvent::Exited { agent } => {
                        self.table.insert(agent.id, agent.clone());
                    }
                    AgentEvent::Removed { id } => {
                        self.table.remove(id);
                    }
                    AgentEvent::Resync => {}
                }
                Update::Event(event)
            }
            _ => Update::Nothing,
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        if cfg!(test) {
            bail!("a test watcher ran out of messages");
        }
        let mut tries = 0;
        loop {
            std::thread::sleep(RECONNECT_PAUSE);
            match subscribe(self.ids.clone(), self.include_exited) {
                Ok((table, rx)) => {
                    (self.table, self.rx) = (table, rx);
                    return Ok(());
                }
                Err(e) if tries + 1 >= RECONNECT_TRIES => return Err(e.context("lost connection to the manager")),
                Err(_) => tries += 1,
            }
        }
    }
}

#[cfg(test)]
impl Watcher {
    /// A watcher of `agents` fed by `rx` instead of a manager.
    pub fn fake(agents: Vec<AgentInfo>, rx: Messages) -> Watcher {
        Watcher { ids: None, include_exited: true, table: table_of(agents), rx }
    }
}

/// Starts a watch and hands its connection to a reader thread.
fn subscribe(ids: Option<Vec<u64>>, include_exited: bool) -> Result<(BTreeMap<u64, AgentInfo>, Messages)> {
    let mut conn = Conn::connect()?;
    let Response::Snapshot { agents, .. } =
        conn.request(&Request::Watch { ids, include_exited }).context("starting a watch")?
    else {
        bail!("manager did not start the watch");
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let msg = conn.next();
            let done = !matches!(msg, Ok(Some(_)));
            if tx.send(msg).is_err() || done {
                return;
            }
        }
    });
    Ok((table_of(agents), rx))
}

fn table_of(agents: Vec<AgentInfo>) -> BTreeMap<u64, AgentInfo> {
    agents.into_iter().map(|a| (a.id, a)).collect()
}
