//! The manager: registry, holder spawning, recovery, and watch streams.
//!
//! It holds no PTY. Holders are the source of truth for "is this agent
//! alive"; the manager is an index that can crash or restart at any time and
//! rebuild itself by reconnecting to every holder socket.

mod activity;
mod directive;
mod holder;
mod hooks;
mod interaction;
mod registry;
mod requests;
mod screen;
mod send;
mod turn;
mod watch;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::frame::{aio, ty};
use argus_proto::msg::{Activity, AgentInfo, AgentStatus, HolderEvent, Request, Response, RunRequest, now_secs};
use argus_proto::paths;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::{driver, naming};
use activity::Fact;
use registry::{Registry, Restore, Settled};
use screen::Screens;

/// How long a stopping manager waits for each holder's output offset.
const SETTLE_QUERY: Duration = Duration::from_millis(500);

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

    manager.record_settled().await;
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
        if let Request::Report { agent_id, source, event, reply } = &req {
            let stdout = manager.report(*agent_id, source, event);
            if *reply {
                aio::write_json(&mut writer, &Response::HookReply { stdout }).await?;
            }
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
            reg.create(|id| AgentInfo {
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
                turns: 0,
                attached: 0,
                tmux_locations: Vec::new(),
                pending_interactions: Vec::new(),
                labels: req.labels.clone(),
            })
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
                let info = self.registry.lock().unwrap().edit(info.id, |rec| {
                    let agent = &mut rec.info;
                    agent.status = AgentStatus::Running;
                    if let Some(activity) = driver.initial_activity() {
                        agent.activity = activity;
                        agent.activity_since = Some(now_secs());
                    }
                    agent.holder_pid = Some(holder_pid);
                    agent.agent_pid = Some(agent_pid);
                    agent.clone()
                });
                let info = info.context("the agent was removed while it started")?;
                holder::link_name(&info.name, info.id);
                self.follow(info.id, driver.ready_on_cursor());
                log(&format!("started {} (id {}, holder {holder_pid}, agent {agent_pid})", info.name, info.id));
                Ok((info, warnings))
            }
            Err(e) => {
                self.registry.lock().unwrap().remove(info.id);
                let _ = fs::remove_dir_all(paths::agent_dir(info.id));
                Err(e)
            }
        }
    }

    /// Re-attaches to holders after a manager restart.
    fn recover(self: &Arc<Self>) {
        let (live, restore) = {
            let mut reg = self.registry.lock().unwrap();
            let live: Vec<(u64, String)> =
                reg.infos().filter(|a| a.status.is_live()).map(|a| (a.id, a.name.clone())).collect();
            (live, reg.take_restore())
        };
        log(&format!("recovering {} live agents", live.len()));
        for (id, name) in live {
            holder::link_name(&name, id);
            // Not `ready_on_cursor`: a recovered agent may be mid-turn.
            self.follow(id, None);
        }
        for (id, restore) in restore {
            tokio::spawn(self.clone().restore(id, restore));
        }
    }

    /// Gives an agent back the activity the previous manager left it in, if
    /// it has printed nothing since: the only sign of it moving on we have
    /// while no hook could reach a manager.
    async fn restore(self: Arc<Self>, id: u64, restore: Restore) {
        let offset = holder::output_offset(id).await.ok();
        self.registry.lock().unwrap().edit(id, |rec| {
            // A hook may have reported something newer meanwhile.
            if !rec.info.status.is_live() || rec.info.activity != Activity::Unknown {
                return;
            }
            if offset != Some(restore.offset) {
                let name = &rec.info.name;
                return log(&format!("{name} printed output while the manager was down; its activity stays unknown"));
            }
            rec.info.activity = restore.activity;
            rec.info.activity_since = restore.since;
        });
    }

    /// On a clean stop, notes the output offset of each agent at its prompt,
    /// so the next manager can restore it (see `restore`).
    async fn record_settled(&self) {
        let candidates: Vec<(u64, Option<u64>)> = {
            let reg = self.registry.lock().unwrap();
            reg.infos()
                .filter(|a| a.status.is_live() && a.activity.awaits_prompt())
                .map(|a| (a.id, a.activity_since))
                .collect()
        };
        let mut settled = Settled::new();
        for &(id, _) in &candidates {
            if let Ok(Ok(offset)) = tokio::time::timeout(SETTLE_QUERY, holder::output_offset(id)).await {
                settled.insert(id, offset);
            }
        }
        let reg = self.registry.lock().unwrap();
        // A hook that landed while we asked means the agent moved on.
        let unchanged = |id: &u64, since: Option<u64>| {
            reg.get(*id).is_some_and(|r| r.info.activity.awaits_prompt() && r.info.activity_since == since)
        };
        settled.retain(|id, _| candidates.iter().any(|&(c, since)| c == *id && unchanged(id, since)));
        if let Err(e) = reg.save_settled(settled) {
            log(&format!("saving the registry: {e:#}"));
        }
    }

    /// Follows a holder's events until the agent exits, then records it.
    /// `ready_on_cursor`: see [`driver::Driver::ready_on_cursor`].
    fn follow(self: &Arc<Self>, id: u64, ready_on_cursor: Option<Duration>) {
        let hookless = {
            let reg = self.registry.lock().unwrap();
            reg.get(id).is_some_and(|r| !driver::for_kind(&r.info.kind).has_hooks())
        };
        if hookless {
            self.poll_output(id);
        }
        let on_cursor = ready_on_cursor.map(|steady| {
            let manager = self.clone();
            Arc::new(move |shown| manager.on_cursor(id, shown, steady)) as screen::OnCursor
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
                HolderEvent::Resized { .. } | HolderEvent::ScreenMode { .. } => {}
            };
            let (status, code, exited_at) = match holder::follow(id, on_event).await {
                Ok(Some(code)) => (AgentStatus::Exited, Some(code), Some(now_secs())),
                Ok(None) | Err(_) => holder::exit_from_disk(id),
            };
            manager.registry.lock().unwrap().edit(id, |rec| {
                if rec.info.status.is_live() {
                    mark_exited(&mut rec.info, status, code, exited_at);
                }
            });
        });
    }
}

/// Records how a live agent ended. It keeps its last activity, for a look at
/// where it stopped; being exited already says it is doing nothing.
fn mark_exited(agent: &mut AgentInfo, status: AgentStatus, code: Option<i32>, exited_at: Option<u64>) {
    agent.status = status;
    agent.exit_code = code;
    agent.exited_at = exited_at;
    registry::clear_volatile(agent);
    holder::unlink_name(&agent.name);
    log(&format!("{} (id {}) is {}", agent.name, agent.id, status.as_str()));
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
