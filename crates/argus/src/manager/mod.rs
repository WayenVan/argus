//! The manager: registry, holder spawning, recovery, and watch streams.
//!
//! It holds no PTY. Holders are the source of truth for "is this agent
//! alive"; the manager is an index that can crash or restart at any time and
//! rebuild itself by reconnecting to every holder socket.

mod holder;
mod registry;
mod watch;

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::frame::{aio, ty};
use argus_proto::msg::{AgentInfo, AgentStatus, Request, Response, RunRequest, now_secs};
use argus_proto::{PROTOCOL_VERSION, paths};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::naming;
use registry::Registry;

/// Longer than the holder's SIGTERM → SIGKILL grace period.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);

pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(serve())
}

pub struct Manager {
    registry: Mutex<Registry>,
    holder_exe: PathBuf,
    shutdown: Notify,
    /// Identifies this manager instance to watchers; changes on restart.
    epoch: u64,
}

async fn serve() -> Result<()> {
    paths::ensure_private_dir(&paths::runtime_dir())?;
    paths::ensure_private_dir(&paths::holders_dir())?;
    paths::ensure_private_dir(&paths::state_dir())?;

    let _lock = acquire_lock()?;
    fs::write(paths::manager_pid(), std::process::id().to_string())?;
    raise_fd_limit();

    let holder_exe = std::env::current_exe()?.with_file_name("argus-holder");
    if !holder_exe.is_file() {
        bail!("argus-holder not found next to argus at {}", holder_exe.display());
    }

    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64;
    let manager =
        Arc::new(Manager { registry: Mutex::new(Registry::load()?), holder_exe, shutdown: Notify::new(), epoch });
    manager.recover();

    let socket = paths::manager_socket();
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    log(&format!("manager started, pid {}", std::process::id()));

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let manager = manager.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(manager, stream).await {
                            log(&format!("connection error: {e:#}"));
                        }
                    });
                }
                Err(e) => log(&format!("accept: {e}")),
            },
            _ = manager.shutdown.notified() => break,
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(paths::manager_pid());
    log("manager stopped");
    Ok(())
}

async fn handle_conn(manager: Arc<Manager>, stream: UnixStream) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let mut greeted = false;
    while let Some((t, payload)) = aio::read_frame(&mut reader).await? {
        if t != ty::CONTROL {
            aio::write_json(&mut writer, &Response::error("bad_frame", "expected a control frame")).await?;
            break;
        }
        let req: Request = match serde_json::from_slice(&payload) {
            Ok(req) => req,
            Err(e) => {
                aio::write_json(&mut writer, &Response::error("bad_request", e.to_string())).await?;
                continue;
            }
        };
        if !greeted && !matches!(req, Request::Hello { .. }) {
            aio::write_json(&mut writer, &Response::error("no_hello", "send Hello first")).await?;
            break;
        }
        greeted = true;
        if let Request::Watch { ids, include_exited } = req {
            // The connection belongs to the watch from here on.
            return watch::serve(&manager, reader, writer, ids, include_exited).await;
        }
        let shutdown = matches!(req, Request::Shutdown { .. });
        let resp = match manager.dispatch(req).await {
            Ok(resp) => resp,
            Err(e) => Response::error("failed", format!("{e:#}")),
        };
        aio::write_json(&mut writer, &resp).await?;
        if shutdown && matches!(resp, Response::Ok) {
            manager.shutdown.notify_one();
            break;
        }
    }
    Ok(())
}

impl Manager {
    async fn dispatch(self: &Arc<Self>, req: Request) -> Result<Response> {
        match req {
            Request::Hello { version } => {
                if version != PROTOCOL_VERSION {
                    log(&format!("client speaks protocol v{version}, manager v{PROTOCOL_VERSION}"));
                }
                Ok(Response::Hello { version: PROTOCOL_VERSION, pid: std::process::id() })
            }
            Request::Run(req) => Ok(Response::Agent { agent: self.spawn_agent(req).await? }),
            Request::List { all, prefix } => {
                let reg = self.registry.lock().unwrap();
                let agents = reg
                    .agents
                    .values()
                    .filter(|a| all || a.status.is_live())
                    .filter(|a| prefix.as_deref().is_none_or(|p| naming::in_prefix(&a.name, p)))
                    .cloned()
                    .collect();
                Ok(Response::Agents { agents })
            }
            Request::Kill { target, signal } => {
                let signal = signal.unwrap_or(libc::SIGTERM);
                let ids = {
                    let reg = self.registry.lock().unwrap();
                    let ids = naming::resolve(&target, reg.agents.values())?;
                    let live: Vec<u64> = ids.into_iter().filter(|id| reg.agents[id].status.is_live()).collect();
                    if live.is_empty() {
                        bail!("{target} is not running");
                    }
                    live
                };
                for &id in &ids {
                    holder::signal(id, signal).await.with_context(|| format!("signalling agent {id}"))?;
                }
                Ok(Response::Killed { ids })
            }
            Request::Remove { target } => {
                let mut reg = self.registry.lock().unwrap();
                let id = resolve_one(&reg, &target, "rm")?;
                if reg.agents[&id].status.is_live() {
                    bail!("{} is still running; `argus kill` it first", reg.agents[&id].name);
                }
                reg.agents.remove(&id);
                reg.changed(id);
                reg.save()?;
                let _ = fs::remove_dir_all(paths::agent_dir(id));
                Ok(Response::Ok)
            }
            Request::Prune { older_than, prefix } => {
                let cutoff = now_secs().saturating_sub(older_than.unwrap_or(0));
                let mut reg = self.registry.lock().unwrap();
                let doomed: Vec<u64> = reg
                    .agents
                    .values()
                    .filter(|a| !a.status.is_live())
                    .filter(|a| a.exited_at.unwrap_or(a.created_at) <= cutoff)
                    .filter(|a| prefix.as_deref().is_none_or(|p| naming::in_prefix(&a.name, p)))
                    .map(|a| a.id)
                    .collect();
                let mut agents = Vec::with_capacity(doomed.len());
                for id in doomed {
                    agents.extend(reg.agents.remove(&id));
                    reg.changed(id);
                    let _ = fs::remove_dir_all(paths::agent_dir(id));
                }
                reg.save()?;
                Ok(Response::Pruned { agents })
            }
            Request::Send { target, text } => {
                let id = {
                    let reg = self.registry.lock().unwrap();
                    let id = resolve_one(&reg, &target, "send")?;
                    if !reg.agents[&id].status.is_live() {
                        bail!("{} is not running", reg.agents[&id].name);
                    }
                    id
                };
                holder::write(id, text).await?;
                Ok(Response::Ok)
            }
            Request::Rename { target, name } => Ok(Response::Agent { agent: self.rename(&target, &name)? }),
            Request::Label { target, set, unset } => {
                for (k, v) in &set {
                    naming::validate_label(k, v)?;
                }
                let mut reg = self.registry.lock().unwrap();
                let ids = naming::resolve(&target, reg.agents.values())?;
                for &id in &ids {
                    let labels = &mut reg.agents.get_mut(&id).expect("resolved").labels;
                    labels.extend(set.clone());
                    for key in &unset {
                        labels.remove(key);
                    }
                    reg.changed(id);
                }
                reg.save()?;
                let agents = ids.iter().map(|id| reg.agents[id].clone()).collect();
                Ok(Response::Agents { agents })
            }
            Request::Shutdown { kill_agents } => {
                if kill_agents {
                    let live: Vec<u64> = {
                        let reg = self.registry.lock().unwrap();
                        reg.agents.values().filter(|a| a.status.is_live()).map(|a| a.id).collect()
                    };
                    for &id in &live {
                        if let Err(e) = holder::signal(id, libc::SIGTERM).await {
                            log(&format!("stopping agent {id}: {e:#}"));
                        }
                    }
                    self.wait_until_stopped(&live).await;
                }
                Ok(Response::Ok)
            }
            Request::Watch { .. } => bail!("Watch is handled by the connection loop"),
        }
    }

    async fn spawn_agent(self: &Arc<Self>, req: RunRequest) -> Result<AgentInfo> {
        let Some(program) = req.command.first() else { bail!("no command given") };
        let kind = naming::kind_of(program);
        let group = match &req.group {
            Some(g) => naming::normalize_group(g)?,
            None => None,
        };
        for (k, v) in &req.labels {
            naming::validate_label(k, v)?;
        }

        let info = {
            let mut reg = self.registry.lock().unwrap();
            let name = match &req.name {
                Some(n) => {
                    let full = if n.contains('/') { n.clone() } else { naming::join(group.as_deref(), n) };
                    naming::validate_name(&full)?;
                    if reg.name_taken(&full) {
                        bail!("name {full} is already in use");
                    }
                    full
                }
                None => naming::default_name(group.as_deref(), &kind, |n| reg.name_taken(n)),
            };
            let id = reg.next_id;
            reg.next_id += 1;
            let info = AgentInfo {
                id,
                name,
                kind,
                command: req.command.clone(),
                cwd: req.cwd.clone(),
                created_at: now_secs(),
                exited_at: None,
                holder_pid: None,
                agent_pid: None,
                status: AgentStatus::Starting,
                exit_code: None,
                activity: "unknown".into(),
                attached: 0,
                labels: req.labels.clone(),
            };
            reg.agents.insert(id, info.clone());
            reg.changed(id);
            reg.save()?;
            info
        };

        match holder::start(&self.holder_exe, info.id, &req).await {
            Ok((holder_pid, agent_pid)) => {
                let info = {
                    let mut reg = self.registry.lock().unwrap();
                    let agent = reg.agents.get_mut(&info.id).expect("agent inserted above");
                    agent.status = AgentStatus::Running;
                    agent.holder_pid = Some(holder_pid);
                    agent.agent_pid = Some(agent_pid);
                    let info = agent.clone();
                    reg.changed(info.id);
                    reg.save()?;
                    info
                };
                holder::link_name(&info.name, info.id);
                self.follow(info.id);
                log(&format!("started {} (id {}, holder {holder_pid}, agent {agent_pid})", info.name, info.id));
                Ok(info)
            }
            Err(e) => {
                let mut reg = self.registry.lock().unwrap();
                reg.agents.remove(&info.id);
                reg.changed(info.id);
                reg.save()?;
                let _ = fs::remove_dir_all(paths::agent_dir(info.id));
                Err(e)
            }
        }
    }

    /// `name` may be a full path, a new last segment (group kept), or a group
    /// ending in `/` (last segment kept; `/` alone means the top level).
    fn rename(&self, target: &str, name: &str) -> Result<AgentInfo> {
        let mut reg = self.registry.lock().unwrap();
        let id = resolve_one(&reg, target, "rename")?;
        let old = reg.agents[&id].name.clone();
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
            return Ok(reg.agents[&id].clone());
        }
        if reg.name_taken(&new) {
            bail!("name {new} is already in use");
        }
        let agent = reg.agents.get_mut(&id).expect("resolved");
        agent.name = new.clone();
        let info = agent.clone();
        if info.status.is_live() {
            holder::unlink_name(&old);
            holder::link_name(&new, id);
        }
        reg.changed(id);
        reg.save()?;
        log(&format!("renamed {old} to {new} (id {id})"));
        Ok(info)
    }

    /// Waits for the follow tasks to record every agent's exit, so the
    /// registry and name links are final before the manager goes away.
    /// Holders escalate to SIGKILL after 5s, so this normally ends well before
    /// the deadline.
    async fn wait_until_stopped(&self, ids: &[u64]) {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        loop {
            let pending = {
                let reg = self.registry.lock().unwrap();
                ids.iter().filter(|id| reg.agents.get(id).is_some_and(|a| a.status.is_live())).count()
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

    /// Re-attaches to holders after a manager restart.
    fn recover(self: &Arc<Self>) {
        let live: Vec<(u64, String)> = {
            let reg = self.registry.lock().unwrap();
            reg.agents.values().filter(|a| a.status.is_live()).map(|a| (a.id, a.name.clone())).collect()
        };
        log(&format!("recovering {} live agents", live.len()));
        for (id, name) in live {
            holder::link_name(&name, id);
            self.follow(id);
        }
    }

    /// Follows a holder's events until the agent exits, then records it.
    fn follow(self: &Arc<Self>, id: u64) {
        let manager = self.clone();
        tokio::spawn(async move {
            let on_attached = |count: u32| {
                let mut reg = manager.registry.lock().unwrap();
                if let Some(agent) = reg.agents.get_mut(&id)
                    && agent.attached != count
                {
                    agent.attached = count;
                    reg.changed(id);
                }
            };
            let (status, code, exited_at) = match holder::follow(id, on_attached).await {
                Ok(Some(code)) => (AgentStatus::Exited, Some(code), Some(now_secs())),
                Ok(None) | Err(_) => holder::exit_from_disk(id),
            };
            let mut reg = manager.registry.lock().unwrap();
            if let Some(agent) = reg.agents.get_mut(&id)
                && agent.status.is_live()
            {
                agent.status = status;
                agent.exit_code = code;
                agent.exited_at = exited_at;
                agent.attached = 0;
                holder::unlink_name(&agent.name);
                log(&format!("{} (id {id}) is {}", agent.name, status.as_str()));
                reg.changed(id);
                if let Err(e) = reg.save() {
                    log(&format!("saving registry: {e:#}"));
                }
            }
        });
    }
}

fn resolve_one(reg: &Registry, target: &str, verb: &str) -> Result<u64> {
    let ids = naming::resolve(target, reg.agents.values())?;
    match ids[..] {
        [id] => Ok(id),
        _ => bail!("{target} matches {} agents; {verb} takes one", ids.len()),
    }
}

fn acquire_lock() -> Result<File> {
    let path = paths::manager_lock();
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    // SAFETY: flock on an fd owned by `file`, which the caller keeps alive.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another manager is already running ({})", path.display());
    }
    Ok(file)
}

/// macOS starts processes with a soft limit of 256 open files, which a few
/// hundred holder connections would exhaust.
fn raise_fd_limit() {
    // SAFETY: getrlimit/setrlimit on a local struct.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let target = if cfg!(target_os = "macos") { lim.rlim_max.min(10240) } else { lim.rlim_max };
        if lim.rlim_cur < target {
            lim.rlim_cur = target;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", now_secs());
}
