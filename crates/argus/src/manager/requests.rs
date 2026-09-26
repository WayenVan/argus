//! Client requests: one handler per `Request`, each answering with its own
//! `Response`. `Watch` and `Report` never get here: they belong to the
//! connection loop (`handle_conn`), since one takes the connection over and
//! the other may not be answered at all.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::msg::{AgentInfo, MANAGER_CAPABILITIES, Request, Response, now_secs};
use argus_proto::{MANAGER_PROTOCOL_VERSION, paths};

use super::activity::Fact;
use super::screen::ScreenReply;
use super::{Manager, holder, log, resolve_one};
use crate::naming;

/// Longer than the holder's SIGTERM → SIGKILL grace period.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);

impl Manager {
    pub(super) async fn dispatch(self: &Arc<Self>, req: Request) -> Result<Response> {
        Ok(match req {
            Request::Hello { version, .. } => hello(version),
            Request::Run(req) => {
                let (agent, warnings) = self.spawn_agent(req).await?;
                Response::Agent { agent, warnings }
            }
            Request::List { all, prefix } => Response::Agents { agents: self.list(all, prefix.as_deref()) },
            Request::Kill { target, signal } => Response::Killed { ids: self.kill(&target, signal).await? },
            Request::Remove { target } => {
                self.remove(&target)?;
                Response::Ok
            }
            Request::Prune { older_than, prefix } => {
                Response::Pruned { agents: self.prune(older_than, prefix.as_deref()) }
            }
            Request::Send { target, text, enter, force } => self.send(&target, text, enter, force).await?,
            Request::Rename { target, name } => {
                Response::Agent { agent: self.rename(&target, &name)?, warnings: vec![] }
            }
            Request::Ack { target } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "ack")?;
                self.on_fact(id, Fact::Ack);
                Response::Ok
            }
            Request::Label { target, set, unset } => Response::Agents { agents: self.label(&target, set, &unset)? },
            Request::Shutdown { kill_agents } => {
                if kill_agents {
                    self.stop_all().await;
                }
                Response::Ok
            }
            Request::Screen { target, since_offset } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "screen")?;
                let ScreenReply { mode, rows, cols, offset, bytes } = self.screens.get(id, since_offset);
                Response::Screen { mode, rows, cols, offset, bytes }
            }
            Request::ScreenPreview { target, rows, cols } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "screen")?;
                Response::ScreenPreview { lines: self.screens.preview(id, rows, cols) }
            }
            Request::ScreenDump { target } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "screen")?;
                let Some((rows, cols, bytes)) = self.screens.dump(id) else {
                    bail!("no screen recorded yet for {target}");
                };
                Response::ScreenDump { rows, cols, bytes }
            }
            Request::Watch { .. } | Request::Report { .. } => bail!("handled by the connection loop"),
        })
    }

    fn list(&self, all: bool, prefix: Option<&str>) -> Vec<AgentInfo> {
        let reg = self.registry.lock().unwrap();
        reg.infos()
            .filter(|a| all || a.status.is_live())
            .filter(|a| prefix.is_none_or(|p| naming::in_prefix(&a.name, p)))
            .cloned()
            .collect()
    }

    /// Signals every live agent `target` names. Returns their IDs.
    async fn kill(&self, target: &str, signal: Option<i32>) -> Result<Vec<u64>> {
        let ids = {
            let reg = self.registry.lock().unwrap();
            let ids = naming::resolve(target, reg.infos())?;
            let live: Vec<u64> = ids.into_iter().filter(|&id| reg.info(id).status.is_live()).collect();
            if live.is_empty() {
                bail!("{target} is not running");
            }
            live
        };
        let signal = signal.unwrap_or(libc::SIGTERM);
        for &id in &ids {
            holder::signal(id, signal).await.with_context(|| format!("signalling agent {id}"))?;
        }
        Ok(ids)
    }

    fn remove(&self, target: &str) -> Result<()> {
        let mut reg = self.registry.lock().unwrap();
        let id = resolve_one(&reg, target, "rm")?;
        if reg.info(id).status.is_live() {
            bail!("{} is still running; `argus kill` it first", reg.info(id).name);
        }
        reg.remove(id);
        let _ = fs::remove_dir_all(paths::agent_dir(id));
        Ok(())
    }

    /// Removes every exited agent that ended more than `older_than` seconds
    /// ago, under `prefix` if given. Returns them.
    fn prune(&self, older_than: Option<u64>, prefix: Option<&str>) -> Vec<AgentInfo> {
        let cutoff = now_secs().saturating_sub(older_than.unwrap_or(0));
        let mut reg = self.registry.lock().unwrap();
        let doomed: Vec<u64> = reg
            .infos()
            .filter(|a| !a.status.is_live())
            .filter(|a| a.exited_at.unwrap_or(a.created_at) <= cutoff)
            .filter(|a| prefix.is_none_or(|p| naming::in_prefix(&a.name, p)))
            .map(|a| a.id)
            .collect();
        let mut agents = Vec::with_capacity(doomed.len());
        for id in doomed {
            agents.extend(reg.remove(id));
            let _ = fs::remove_dir_all(paths::agent_dir(id));
        }
        agents
    }

    /// `name` may be a full path, a new last segment (group kept), or a group
    /// ending in `/` (last segment kept; `/` alone means the top level).
    fn rename(&self, target: &str, name: &str) -> Result<AgentInfo> {
        let mut reg = self.registry.lock().unwrap();
        let id = resolve_one(&reg, target, "rename")?;
        let old = reg.info(id).name.clone();
        let (group, leaf) = match old.rsplit_once('/') {
            Some((g, l)) => (Some(g), l),
            None => (None, old.as_str()),
        };
        let new = if let Some(dest) = name.strip_suffix('/') {
            let dest = dest.trim_start_matches('/');
            naming::join((!dest.is_empty()).then_some(dest), leaf)
        } else if name.contains('/') {
            name.trim_start_matches('/').to_string()
        } else {
            naming::join(group, name)
        };
        naming::validate_name(&new)?;
        if new == old {
            return Ok(reg.info(id).clone());
        }
        if reg.name_taken(&new) {
            bail!("name {new} is already in use");
        }
        let info = reg
            .edit(id, |rec| {
                rec.info.name = new.clone();
                rec.info.clone()
            })
            .expect("resolved");
        if info.status.is_live() {
            holder::unlink_name(&old);
            holder::link_name(&new, id);
        }
        log(&format!("renamed {old} to {new} (id {id})"));
        Ok(info)
    }

    /// Sets and removes labels on every agent `target` names. Returns them.
    fn label(&self, target: &str, set: BTreeMap<String, String>, unset: &[String]) -> Result<Vec<AgentInfo>> {
        for (k, v) in &set {
            naming::validate_label(k, v)?;
        }
        let mut reg = self.registry.lock().unwrap();
        let ids = naming::resolve(target, reg.infos())?;
        for &id in &ids {
            reg.edit(id, |rec| {
                let labels = &mut rec.info.labels;
                labels.extend(set.clone());
                for key in unset {
                    labels.remove(key);
                }
            });
        }
        Ok(ids.iter().map(|&id| reg.info(id).clone()).collect())
    }

    /// Stops every live agent and waits for the follow tasks to record each
    /// exit, so the registry and name links are final before the manager
    /// goes away. Holders escalate to SIGKILL after 5s, so this normally ends
    /// well before the deadline.
    async fn stop_all(&self) {
        let live: Vec<u64> = {
            let reg = self.registry.lock().unwrap();
            reg.infos().filter(|a| a.status.is_live()).map(|a| a.id).collect()
        };
        for &id in &live {
            if let Err(e) = holder::signal(id, libc::SIGTERM).await {
                log(&format!("stopping agent {id}: {e:#}"));
            }
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        loop {
            let pending = {
                let reg = self.registry.lock().unwrap();
                live.iter().filter(|&&id| reg.get(id).is_some_and(|r| r.info.status.is_live())).count()
            };
            if pending == 0 {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                return log(&format!("{pending} agents still running at shutdown"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn hello(version: u32) -> Response {
    if version != MANAGER_PROTOCOL_VERSION {
        log(&format!("client speaks protocol v{version}, manager v{MANAGER_PROTOCOL_VERSION}"));
    }
    Response::Hello {
        version: MANAGER_PROTOCOL_VERSION,
        pid: std::process::id(),
        capabilities: MANAGER_CAPABILITIES.to_vec(),
        build: Some(argus_proto::BUILD.to_string()),
    }
}
