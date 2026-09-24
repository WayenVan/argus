//! The manager: registry, holder spawning, and recovery.
//!
//! It holds no PTY. Holders are the source of truth for "is this agent
//! alive"; the manager is an index that can crash or restart at any time and
//! rebuild itself by reconnecting to every holder socket.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::frame::{aio, ty};
use argus_proto::msg::{
    AgentInfo, AgentStatus, ExitRecord, HolderReady, HolderRequest, HolderResponse, HolderSpec, Request,
    Response, RunRequest, SubscribeLevel, now_secs,
};
use argus_proto::{PROTOCOL_VERSION, paths};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::naming;

const HOLDER_READY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(serve())
}

type Shared = Arc<Manager>;

struct Manager {
    registry: Mutex<Registry>,
    holder_exe: PathBuf,
    shutdown: Notify,
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

    let manager = Arc::new(Manager { registry: Mutex::new(Registry::load()?), holder_exe, shutdown: Notify::new() });
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

async fn handle_conn(manager: Shared, stream: UnixStream) -> Result<()> {
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
                    signal_holder(id, signal).await.with_context(|| format!("signalling agent {id}"))?;
                }
                Ok(Response::Killed { ids })
            }
            Request::Remove { target } => {
                let mut reg = self.registry.lock().unwrap();
                let ids = naming::resolve(&target, reg.agents.values())?;
                let [id] = ids[..] else { bail!("{target} matches {} agents; rm takes one", ids.len()) };
                if reg.agents[&id].status.is_live() {
                    bail!("{} is still running; `argus kill` it first", reg.agents[&id].name);
                }
                reg.agents.remove(&id);
                reg.save()?;
                let _ = fs::remove_dir_all(paths::agent_dir(id));
                Ok(Response::Ok)
            }
            Request::Shutdown { kill_agents } => {
                if kill_agents {
                    let live: Vec<u64> = {
                        let reg = self.registry.lock().unwrap();
                        reg.agents.values().filter(|a| a.status.is_live()).map(|a| a.id).collect()
                    };
                    for id in live {
                        if let Err(e) = signal_holder(id, libc::SIGTERM).await {
                            log(&format!("stopping agent {id}: {e:#}"));
                        }
                    }
                }
                Ok(Response::Ok)
            }
        }
    }

    async fn spawn_agent(self: &Arc<Self>, req: RunRequest) -> Result<AgentInfo> {
        let Some(program) = req.command.first() else { bail!("no command given") };
        let kind = naming::kind_of(program);
        let group = match &req.group {
            Some(g) => naming::normalize_group(g)?,
            None => None,
        };

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
            };
            reg.agents.insert(id, info.clone());
            reg.save()?;
            info
        };

        match self.start_holder(info.id, &req).await {
            Ok((holder_pid, agent_pid)) => {
                let info = {
                    let mut reg = self.registry.lock().unwrap();
                    let agent = reg.agents.get_mut(&info.id).expect("agent inserted above");
                    agent.status = AgentStatus::Running;
                    agent.holder_pid = Some(holder_pid);
                    agent.agent_pid = Some(agent_pid);
                    let info = agent.clone();
                    reg.save()?;
                    info
                };
                self.watch(info.id);
                log(&format!("started {} (id {}, holder {holder_pid}, agent {agent_pid})", info.name, info.id));
                Ok(info)
            }
            Err(e) => {
                let mut reg = self.registry.lock().unwrap();
                reg.agents.remove(&info.id);
                reg.save()?;
                let _ = fs::remove_dir_all(paths::agent_dir(info.id));
                Err(e)
            }
        }
    }

    /// Launches `argus-holder` and waits for it to report the agent is running.
    async fn start_holder(&self, id: u64, req: &RunRequest) -> Result<(u32, u32)> {
        let dir = paths::agent_dir(id);
        paths::ensure_private_dir(&dir)?;
        let log_file = OpenOptions::new().create(true).append(true).open(dir.join("holder.log"))?;
        let spec = HolderSpec {
            id,
            command: req.command.clone(),
            cwd: req.cwd.clone(),
            env: req.env.clone(),
            rows: req.rows,
            cols: req.cols,
            socket: paths::holder_socket(id),
            state_dir: dir,
            manager_socket: paths::manager_socket(),
        };

        let mut child = tokio::process::Command::new(&self.holder_exe)
            .args(["--id", &id.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log_file)
            .spawn()
            .context("launching argus-holder")?;
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(&serde_json::to_vec(&spec)?).await?;
        drop(stdin);
        let stdout = child.stdout.take().expect("piped");
        // The holder forks and its parent exits at once; reap that parent.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(HOLDER_READY_TIMEOUT, lines.next_line())
            .await
            .context("argus-holder did not report ready in time")??
            .context("argus-holder exited without reporting")?;
        match serde_json::from_str(&line).context("bad ready message from argus-holder")? {
            HolderReady::Ready { holder_pid, agent_pid } => Ok((holder_pid, agent_pid)),
            HolderReady::Failed { message } => bail!(message),
        }
    }

    /// Re-attaches to holders after a manager restart.
    fn recover(self: &Arc<Self>) {
        let live: Vec<u64> = {
            let reg = self.registry.lock().unwrap();
            reg.agents.values().filter(|a| a.status.is_live()).map(|a| a.id).collect()
        };
        log(&format!("recovering {} live agents", live.len()));
        for id in live {
            self.watch(id);
        }
    }

    /// Follows a holder's events until the agent exits, then records it.
    fn watch(self: &Arc<Self>, id: u64) {
        let manager = self.clone();
        tokio::spawn(async move {
            let (status, code, exited_at) = match follow_holder(id).await {
                Ok(Some(code)) => (AgentStatus::Exited, Some(code), Some(now_secs())),
                Ok(None) | Err(_) => exit_from_disk(id),
            };
            let mut reg = manager.registry.lock().unwrap();
            if let Some(agent) = reg.agents.get_mut(&id)
                && agent.status.is_live()
            {
                agent.status = status;
                agent.exit_code = code;
                agent.exited_at = exited_at;
                log(&format!("{} (id {id}) is {}", agent.name, status.as_str()));
                if let Err(e) = reg.save() {
                    log(&format!("saving registry: {e:#}"));
                }
            }
        });
    }
}

async fn holder_conn(id: u64) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(paths::holder_socket(id)).await?;
    holder_call(&mut stream, &HolderRequest::Hello { version: PROTOCOL_VERSION }).await?;
    Ok(stream)
}

async fn holder_call(stream: &mut UnixStream, req: &HolderRequest) -> Result<HolderResponse> {
    aio::write_json(stream, req).await?;
    loop {
        let Some((t, payload)) = aio::read_frame(stream).await? else { bail!("holder closed the connection") };
        if t == ty::CONTROL {
            return match serde_json::from_slice(&payload)? {
                HolderResponse::Error { message } => bail!(message),
                resp => Ok(resp),
            };
        }
    }
}

/// Returns the exit code once the holder reports it, or `None` on EOF.
async fn follow_holder(id: u64) -> Result<Option<i32>> {
    let mut stream = holder_conn(id).await?;
    holder_call(&mut stream, &HolderRequest::Subscribe { level: SubscribeLevel::Events }).await?;
    while let Some((t, payload)) = aio::read_frame(&mut stream).await? {
        if t == ty::EXIT && payload.len() == 4 {
            return Ok(Some(i32::from_be_bytes(payload[..4].try_into().unwrap())));
        }
    }
    Ok(None)
}

async fn signal_holder(id: u64, signal: i32) -> Result<()> {
    let mut stream = holder_conn(id).await?;
    holder_call(&mut stream, &HolderRequest::Signal { signal }).await?;
    Ok(())
}

/// Used when the holder is gone: its exit record says how the agent ended.
fn exit_from_disk(id: u64) -> (AgentStatus, Option<i32>, Option<u64>) {
    let path = paths::exit_record(&paths::agent_dir(id));
    match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<ExitRecord>(&b).ok()) {
        Some(rec) => (AgentStatus::Exited, Some(rec.code), Some(rec.exited_at)),
        None => (AgentStatus::Lost, None, None),
    }
}

#[derive(Default)]
struct Registry {
    next_id: u64,
    agents: BTreeMap<u64, AgentInfo>,
}

#[derive(Serialize, Deserialize)]
struct RegistryFile {
    next_id: u64,
    agents: Vec<AgentInfo>,
}

impl Registry {
    fn load() -> Result<Registry> {
        let path = paths::registry_file();
        let file: RegistryFile = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RegistryFile { next_id: 1, agents: vec![] },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let agents: BTreeMap<u64, AgentInfo> = file.agents.into_iter().map(|a| (a.id, a)).collect();
        // Never hand out an ID twice, even if the counter was lost.
        let next_id = file.next_id.max(agents.keys().max().map_or(1, |m| m + 1));
        Ok(Registry { next_id, agents })
    }

    fn save(&self) -> Result<()> {
        let path = paths::registry_file();
        let tmp = path.with_extension("json.tmp");
        let file = RegistryFile { next_id: self.next_id, agents: self.agents.values().cloned().collect() };
        fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn name_taken(&self, name: &str) -> bool {
        self.agents.values().any(|a| a.name == name)
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
