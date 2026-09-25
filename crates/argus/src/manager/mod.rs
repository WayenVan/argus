//! The manager: registry, holder spawning, recovery, and watch streams.
//!
//! It holds no PTY. Holders are the source of truth for "is this agent
//! alive"; the manager is an index that can crash or restart at any time and
//! rebuild itself by reconnecting to every holder socket.

mod activity;
pub(crate) mod driver;
mod holder;
mod registry;
mod screen;
mod send;
mod watch;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::frame::{aio, ty};
use argus_proto::msg::{
    AgentInfo, AgentStatus, HolderEvent, MANAGER_CAPABILITIES, Request, Response, RunRequest, now_secs,
};
use argus_proto::{MANAGER_PROTOCOL_VERSION, paths};
use nix::fcntl::{Flock, FlockArg};
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::naming;
use activity::Fact;
use registry::{AgentRecord, Registry};
use screen::Screens;

/// Longer than the holder's SIGTERM → SIGKILL grace period.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);

pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(serve())
}

pub struct Manager {
    registry: Mutex<Registry>,
    holder_exe: PathBuf,
    drivers: driver::Context,
    shutdown: Notify,
    /// Identifies this manager instance to watchers; changes on restart.
    epoch: u64,
    screens: Arc<Screens>,
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

    let hook_exe = std::env::current_exe()?.with_file_name("argus-hook");
    let drivers =
        driver::Context { hook_exe: hook_exe.is_file().then_some(hook_exe), dir: paths::state_dir().join("drivers") };
    paths::ensure_private_dir(&drivers.dir)?;
    if let Err(e) = driver::install_shared_files(&drivers) {
        log(&format!("writing driver files: {e:#}"));
    }

    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64;
    let manager = Arc::new(Manager {
        registry: Mutex::new(Registry::load()?),
        holder_exe,
        drivers,
        shutdown: Notify::new(),
        epoch,
        screens: Screens::new(),
    });
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
        if let Request::Report { agent_id, source, event } = &req {
            manager.report(*agent_id, source, event); // Never answered.
            continue;
        }
        if let Request::Watch { ids, include_exited } = req {
            // The connection belongs to the watch from here on.
            return watch::serve(&manager, reader, writer, ids, include_exited).await;
        }
        let shutdown = matches!(req, Request::Shutdown { .. });
        let resp = match manager.dispatch(req).await {
            Ok(resp) => resp,
            Err(e) => Response::error(crate::errors::code_of(&e), format!("{e:#}")),
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
            Request::Hello { version, .. } => {
                if version != MANAGER_PROTOCOL_VERSION {
                    log(&format!("client speaks protocol v{version}, manager v{MANAGER_PROTOCOL_VERSION}"));
                }
                Ok(Response::Hello {
                    version: MANAGER_PROTOCOL_VERSION,
                    pid: std::process::id(),
                    capabilities: MANAGER_CAPABILITIES.to_vec(),
                    build: Some(argus_proto::BUILD.to_string()),
                })
            }
            Request::Run(req) => {
                let (agent, warnings) = self.spawn_agent(req).await?;
                Ok(Response::Agent { agent, warnings })
            }
            Request::List { all, prefix } => {
                let reg = self.registry.lock().unwrap();
                let agents = reg
                    .infos()
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
                    let ids = naming::resolve(&target, reg.infos())?;
                    let live: Vec<u64> = ids.into_iter().filter(|&id| reg.info(id).status.is_live()).collect();
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
                if reg.info(id).status.is_live() {
                    bail!("{} is still running; `argus kill` it first", reg.info(id).name);
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
                    .infos()
                    .filter(|a| !a.status.is_live())
                    .filter(|a| a.exited_at.unwrap_or(a.created_at) <= cutoff)
                    .filter(|a| prefix.as_deref().is_none_or(|p| naming::in_prefix(&a.name, p)))
                    .map(|a| a.id)
                    .collect();
                let mut agents = Vec::with_capacity(doomed.len());
                for id in doomed {
                    agents.extend(reg.agents.remove(&id).map(|r| r.info));
                    reg.changed(id);
                    let _ = fs::remove_dir_all(paths::agent_dir(id));
                }
                reg.save()?;
                Ok(Response::Pruned { agents })
            }
            Request::Send { target, text, enter, force } => self.send(&target, text, enter, force).await,
            Request::Rename { target, name } => {
                Ok(Response::Agent { agent: self.rename(&target, &name)?, warnings: vec![] })
            }
            Request::Ack { target } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "ack")?;
                self.on_fact(id, Fact::Ack);
                Ok(Response::Ok)
            }
            Request::Label { target, set, unset } => {
                for (k, v) in &set {
                    naming::validate_label(k, v)?;
                }
                let mut reg = self.registry.lock().unwrap();
                let ids = naming::resolve(&target, reg.infos())?;
                for &id in &ids {
                    let labels = &mut reg.agents.get_mut(&id).expect("resolved").info.labels;
                    labels.extend(set.clone());
                    for key in &unset {
                        labels.remove(key);
                    }
                    reg.changed(id);
                }
                reg.save()?;
                let agents = ids.iter().map(|&id| reg.info(id).clone()).collect();
                Ok(Response::Agents { agents })
            }
            Request::Shutdown { kill_agents } => {
                if kill_agents {
                    let live: Vec<u64> = {
                        let reg = self.registry.lock().unwrap();
                        reg.infos().filter(|a| a.status.is_live()).map(|a| a.id).collect()
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
            Request::Screen { target, since_offset } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "screen")?;
                let screen::ScreenReply { mode, rows, cols, offset, bytes } = self.screens.get(id, since_offset);
                Ok(Response::Screen { mode, rows, cols, offset, bytes })
            }
            Request::ScreenPreview { target, rows, cols } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "screen")?;
                Ok(Response::ScreenPreview { lines: self.screens.preview(id, rows, cols) })
            }
            Request::ScreenDump { target } => {
                let id = resolve_one(&self.registry.lock().unwrap(), &target, "screen")?;
                let Some((rows, cols, bytes)) = self.screens.dump(id) else {
                    bail!("no screen recorded yet for {target}");
                };
                Ok(Response::ScreenDump { rows, cols, bytes })
            }
            Request::Watch { .. } | Request::Report { .. } => bail!("handled by the connection loop"),
        }
    }

    async fn spawn_agent(self: &Arc<Self>, req: RunRequest) -> Result<(AgentInfo, Vec<String>)> {
        let Some(program) = req.command.first() else { bail!("no command given") };
        let kind = match &req.kind {
            Some(k) => {
                naming::validate_name(k).context("invalid --kind")?;
                k.clone()
            }
            None => naming::kind_of(program),
        };
        let driver = driver::for_kind(&kind);
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
                activity_since: None,
                attached: 0,
                tmux_locations: Vec::new(),
                labels: req.labels.clone(),
            };
            reg.agents.insert(id, AgentRecord::new(info.clone()));
            reg.changed(id);
            reg.save()?;
            info
        };

        let dir = paths::agent_dir(info.id);
        paths::ensure_private_dir(&dir)?;
        let mut launch = driver::Launch { command: req.command.clone(), env: req.env.clone(), agent_dir: dir };
        let warnings: Vec<String> = match driver.prepare(&mut launch, &self.drivers) {
            Ok(w) => w.into_iter().collect(),
            Err(e) => vec![format!("{} setup failed, activity will not be tracked: {e:#}", driver.kind())],
        };

        match holder::start(&self.holder_exe, info.id, &req, launch.command, launch.env).await {
            Ok((holder_pid, agent_pid)) => {
                let info = {
                    let mut reg = self.registry.lock().unwrap();
                    let agent = &mut reg.agents.get_mut(&info.id).expect("agent inserted above").info;
                    agent.status = AgentStatus::Running;
                    if let Some(activity) = driver.initial_activity() {
                        agent.activity = activity;
                        agent.activity_since = Some(now_secs());
                    }
                    agent.holder_pid = Some(holder_pid);
                    agent.agent_pid = Some(agent_pid);
                    let info = agent.clone();
                    reg.changed(info.id);
                    reg.save()?;
                    info
                };
                holder::link_name(&info.name, info.id);
                self.follow(info.id, driver.ready_on_cursor());
                log(&format!("started {} (id {}, holder {holder_pid}, agent {agent_pid})", info.name, info.id));
                Ok((info, warnings))
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
        let agent = &mut reg.agents.get_mut(&id).expect("resolved").info;
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
                ids.iter().filter(|id| reg.agents.get(id).is_some_and(|r| r.info.status.is_live())).count()
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
            reg.infos().filter(|a| a.status.is_live()).map(|a| (a.id, a.name.clone())).collect()
        };
        log(&format!("recovering {} live agents", live.len()));
        for (id, name) in live {
            holder::link_name(&name, id);
            // Not `ready_on_cursor`: a recovered agent may be mid-turn.
            self.follow(id, false);
        }
    }

    /// Follows a holder's events until the agent exits, then records it.
    /// `ready_on_cursor`: see [`driver::Driver::ready_on_cursor`].
    fn follow(self: &Arc<Self>, id: u64, ready_on_cursor: bool) {
        let hookless = {
            let reg = self.registry.lock().unwrap();
            reg.agents.get(&id).is_some_and(|r| !driver::for_kind(&r.info.kind).has_hooks())
        };
        if hookless {
            self.poll_output(id);
        }
        let on_cursor = ready_on_cursor.then(|| {
            let manager = self.clone();
            Box::new(move || manager.on_fact(id, Fact::Ready)) as Box<dyn FnOnce() + Send>
        });
        self.screens.track(id, on_cursor);
        let manager = self.clone();
        tokio::spawn(async move {
            let on_event = |event: HolderEvent| match event {
                HolderEvent::Attached { count, focused, tmux_locations } => {
                    manager.on_fact(id, Fact::Attached { count, focused, tmux_locations })
                }
                HolderEvent::Input => manager.on_fact(id, Fact::Input),
                // Only the screen-tracking subscription cares about this.
                HolderEvent::Resized { .. } => {}
            };
            let (status, code, exited_at) = match holder::follow(id, on_event).await {
                Ok(Some(code)) => (AgentStatus::Exited, Some(code), Some(now_secs())),
                Ok(None) | Err(_) => holder::exit_from_disk(id),
            };
            let mut reg = manager.registry.lock().unwrap();
            if let Some(agent) = reg.agents.get_mut(&id).map(|r| &mut r.info)
                && agent.status.is_live()
            {
                agent.status = status;
                agent.exit_code = code;
                agent.exited_at = exited_at;
                agent.attached = 0;
                agent.tmux_locations.clear();
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
    let ids = naming::resolve(target, reg.infos())?;
    match ids[..] {
        [id] => Ok(id),
        _ => bail!("{target} matches {} agents; {verb} takes one", ids.len()),
    }
}

fn acquire_lock() -> Result<Flock<File>> {
    let path = paths::manager_lock();
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock)
        .map_err(|(_, _)| anyhow::anyhow!("another manager is already running ({})", path.display()))
}

/// macOS starts processes with a soft limit of 256 open files, which a few
/// hundred holder connections would exhaust.
fn raise_fd_limit() {
    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else { return };
    let target = if cfg!(target_os = "macos") { hard.min(10240) } else { hard };
    if soft < target {
        let _ = setrlimit(Resource::RLIMIT_NOFILE, target, hard);
    }
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", now_secs());
}
