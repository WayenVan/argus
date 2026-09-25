//! User-facing one-shot commands. The client is stateless.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{AgentInfo, Capability, MANAGER_CAPABILITIES, Request, Response, RunRequest, now_secs};
use argus_proto::{MANAGER_PROTOCOL_VERSION, paths};
use nix::sys::signal::kill as signal_process;
use nix::unistd::{Pid, setsid};

use crate::{attach, naming, stream};

const START_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Conn {
    stream: UnixStream,
    capabilities: Vec<Capability>,
}

impl Conn {
    /// Connects to the manager, starting it first when `autostart` is set.
    pub fn open(autostart: bool) -> Result<Option<Conn>> {
        let socket = paths::manager_socket();
        let stream = match UnixStream::connect(&socket) {
            Ok(s) => s,
            Err(_) if autostart => {
                spawn_manager()?;
                wait_for_socket(&socket)?
            }
            Err(_) => return Ok(None),
        };
        let mut conn = Conn { stream, capabilities: Vec::new() };
        match conn.request(&hello_request())? {
            Response::Hello { version, capabilities, .. } if version == MANAGER_PROTOCOL_VERSION => {
                conn.capabilities = capabilities;
                Ok(Some(conn))
            }
            Response::Hello { version, .. } => bail!(
                "manager speaks protocol v{version}, this argus speaks v{MANAGER_PROTOCOL_VERSION}; run `argus manager stop` and retry"
            ),
            other => bail!("unexpected handshake reply: {other:?}"),
        }
    }

    pub fn connect() -> Result<Conn> {
        Ok(Conn::open(true)?.expect("autostart always yields a connection"))
    }

    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn request(&mut self, req: &Request) -> Result<Response> {
        frame::write_json(&mut self.stream, req).context("sending request to manager")?;
        self.next()?.context("manager closed the connection")
    }

    /// Reads the next message; `None` when the manager closed the connection.
    pub fn next(&mut self) -> Result<Option<Response>> {
        let Some((t, payload)) = frame::read_frame(&mut self.stream)? else { return Ok(None) };
        if t != ty::CONTROL {
            bail!("unexpected frame type {t:#x} from manager");
        }
        match serde_json::from_slice(&payload)? {
            Response::Error { message, .. } => bail!(message),
            resp => Ok(Some(resp)),
        }
    }

    pub fn list(&mut self, all: bool) -> Result<Vec<AgentInfo>> {
        match self.request(&Request::List { all, prefix: None })? {
            Response::Agents { agents } => Ok(agents),
            other => bail!("unexpected reply to List: {other:?}"),
        }
    }

    /// Resolves a target that must name exactly one agent.
    pub fn find(&mut self, target: &str) -> Result<AgentInfo> {
        let mut agents = self.list(true)?;
        let ids = naming::resolve(target, agents.iter())?;
        let [id] = ids[..] else { bail!("{target} matches {} agents", ids.len()) };
        let idx = agents.iter().position(|a| a.id == id).expect("resolved from this list");
        Ok(agents.swap_remove(idx))
    }
}

fn spawn_manager() -> Result<()> {
    paths::ensure_private_dir(&paths::runtime_dir())?;
    paths::ensure_private_dir(&paths::state_dir())?;
    let log = OpenOptions::new().create(true).append(true).open(paths::manager_log())?;
    let exe = std::env::current_exe().context("locating the argus binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(["manager", "run"]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid is async-signal-safe; it detaches the manager from our terminal.
    unsafe {
        cmd.pre_exec(|| {
            setsid().map_err(io::Error::other)?;
            Ok(())
        });
    }
    // The manager outlives us; init reaps it once we exit.
    cmd.spawn().context("starting the manager")?;
    Ok(())
}

fn wait_for_socket(socket: &PathBuf) -> Result<UnixStream> {
    let started = Instant::now();
    loop {
        match UnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(e) if started.elapsed() > START_TIMEOUT => {
                return Err(e)
                    .with_context(|| format!("manager did not start; see {}", paths::manager_log().display()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub struct RunOptions {
    pub name: Option<String>,
    pub group: Option<String>,
    pub cwd: Option<PathBuf>,
    pub labels: Vec<(String, String)>,
    pub kind: Option<String>,
    pub attach: bool,
}

pub fn run(opts: RunOptions, kind: String, args: Vec<String>) -> Result<()> {
    let cwd = match opts.cwd {
        Some(dir) => std::fs::canonicalize(&dir).with_context(|| format!("no such directory: {}", dir.display()))?,
        None => std::env::current_dir()?,
    };
    let group = opts.group.or_else(|| std::env::var("ARGUS_GROUP").ok()).filter(|g| !g.is_empty());
    let (rows, cols) = terminal_size();
    let mut command = vec![kind];
    command.extend(args);
    let req = RunRequest {
        command,
        name: opts.name,
        group,
        cwd: cwd.to_string_lossy().into_owned(),
        env: std::env::vars().collect(),
        rows,
        cols,
        labels: opts.labels.into_iter().collect(),
        kind: opts.kind,
    };
    let reply = Conn::connect()?.request(&Request::Run(req))?;
    if let Response::Agent { warnings, .. } = &reply {
        for w in warnings {
            eprintln!("argus: warning: {w}");
        }
    }
    match reply {
        Response::Agent { agent, .. } if opts.attach => {
            let target =
                attach::Target { socket: paths::holder_socket(agent.id), name: agent.name, id: Some(agent.id) };
            attach::attach(
                &target,
                attach::Options { readonly: false, steal: false, replay: false, allow_clipboard_replay: false },
            )
        }
        Response::Agent { agent, .. } => {
            println!("{}\t{}", agent.id, agent.name);
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub struct PsOptions {
    pub prefix: Option<String>,
    pub all: bool,
    pub labels: Vec<(String, String)>,
    pub json: bool,
    pub watch: bool,
}

impl PsOptions {
    pub fn keeps(&self, agent: &AgentInfo) -> bool {
        (self.all || agent.status.is_live())
            && self.prefix.as_deref().is_none_or(|p| naming::in_prefix(&agent.name, p))
            && self.labels.iter().all(|(k, v)| agent.labels.get(k) == Some(v))
    }
}

pub fn ps(opts: PsOptions) -> Result<()> {
    if opts.watch {
        return stream::ps_watch(&opts);
    }
    let agents: Vec<AgentInfo> = Conn::connect()?.list(true)?.into_iter().filter(|a| opts.keeps(a)).collect();
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&agents)?);
    } else {
        print!("{}", format_table(&agents));
    }
    Ok(())
}

pub fn format_table(agents: &[AgentInfo]) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let now = now_secs();
    let with_labels = agents.iter().any(|a| !a.labels.is_empty());
    let mut header = vec!["ID", "GROUP", "NAME", "KIND", "STATUS", "ACTIVITY", "AGE", "ATTACHED", "CWD"];
    if with_labels {
        header.push("LABELS");
    }
    let rows: Vec<Vec<String>> = agents
        .iter()
        .map(|a| {
            let (group, name) = split_name(&a.name);
            let status = match (a.status.as_str(), a.exit_code) {
                ("exited", Some(code)) => format!("exited({code})"),
                (s, _) => s.to_string(),
            };
            let cwd = match a.cwd.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => a.cwd.clone(),
            };
            let attached = if a.status.is_live() { a.attached.to_string() } else { "-".into() };
            let mut row = vec![
                a.id.to_string(),
                group.to_string(),
                name.to_string(),
                a.kind.clone(),
                status,
                activity(a, now),
                age(now.saturating_sub(a.created_at)),
                attached,
                cwd,
            ];
            if with_labels {
                row.push(a.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","));
            }
            row
        })
        .collect();
    let header: Vec<String> = header.into_iter().map(String::from).collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let line: Vec<String> = row.iter().zip(&widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// Splits the canonical path-shaped name for human table output. Nested
/// groups remain intact; an ungrouped agent gets an explicit marker.
fn split_name(name: &str) -> (&str, &str) {
    name.rsplit_once('/').unwrap_or(("-", name))
}

/// `done 3m` — how long a result has been waiting is what matters most.
fn activity(a: &AgentInfo, now: u64) -> String {
    if !a.status.is_live() {
        return "-".into();
    }
    match (a.activity.as_str(), a.activity_since) {
        ("done" | "blocked" | "error", Some(since)) => {
            format!("{} {}", a.activity, age(now.saturating_sub(since)))
        }
        _ => a.activity.clone(),
    }
}

fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

pub fn attach(target: String, opts: attach::Options) -> Result<()> {
    let from_manager = match Conn::open(false)? {
        Some(mut conn) => {
            let agent = conn.find(&target)?;
            if !agent.status.is_live() {
                bail!("{} has {}", agent.name, agent.status.as_str());
            }
            Some((agent.id, agent.name))
        }
        None => None,
    };
    attach::attach(&attach::resolve(&target, from_manager)?, opts)
}

pub fn kill(target: String, signal: Option<i32>) -> Result<()> {
    match Conn::connect()?.request(&Request::Kill { target, signal })? {
        Response::Killed { ids } => {
            for id in ids {
                println!("{id}");
            }
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn rm(target: String) -> Result<()> {
    Conn::connect()?.request(&Request::Remove { target })?;
    Ok(())
}

pub fn prune(prefix: Option<String>, older_than: Option<u64>) -> Result<()> {
    let Response::Pruned { agents } = Conn::connect()?.request(&Request::Prune { older_than, prefix })? else {
        bail!("unexpected reply to Prune");
    };
    for a in &agents {
        println!("{}\t{}", a.id, a.name);
    }
    eprintln!("removed {} exited agent{}", agents.len(), if agents.len() == 1 { "" } else { "s" });
    Ok(())
}

/// Types `text` into the agent. Enter is sent as a separate write so TUIs
/// see a keypress rather than a pasted line ending.
pub fn send(target: String, text: String, enter: bool) -> Result<()> {
    let mut conn = Conn::connect()?;
    if !text.is_empty() {
        conn.request(&Request::Send { target: target.clone(), text })?;
    }
    if enter {
        std::thread::sleep(Duration::from_millis(30));
        conn.request(&Request::Send { target, text: "\r".into() })?;
    }
    Ok(())
}

pub fn rename(target: String, name: String) -> Result<()> {
    if let Response::Agent { agent, .. } = Conn::connect()?.request(&Request::Rename { target, name })? {
        println!("{}\t{}", agent.id, agent.name);
    }
    Ok(())
}

/// `argus mv <target> <group>`: keeps the last segment, changes the group.
pub fn mv(target: String, group: String) -> Result<()> {
    let dest = if group.ends_with('/') { group } else { format!("{group}/") };
    rename(target, dest)
}

pub fn label(target: String, changes: Vec<String>) -> Result<()> {
    let mut set = BTreeMap::new();
    let mut unset = Vec::new();
    for change in changes {
        if let Some(key) = change.strip_suffix('-').filter(|k| !k.contains('=')) {
            unset.push(key.to_string());
        } else {
            let (k, v) = naming::parse_label(&change)?;
            set.insert(k, v);
        }
    }
    if let Response::Agents { agents } = Conn::connect()?.request(&Request::Label { target, set, unset })? {
        for a in agents {
            let labels: Vec<String> = a.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            println!("{}\t{}\t{}", a.id, a.name, labels.join(","));
        }
    }
    Ok(())
}

pub fn ack(target: String) -> Result<()> {
    Conn::connect()?.request(&Request::Ack { target })?;
    Ok(())
}

pub fn manager_start() -> Result<()> {
    Conn::connect()?;
    manager_status()
}

pub fn manager_stop(kill_agents: bool) -> Result<()> {
    match Conn::open(false)? {
        Some(mut conn) => {
            conn.request(&Request::Shutdown { kill_agents })?;
            println!("manager stopped");
        }
        None => println!("manager is not running"),
    }
    Ok(())
}

/// Replaces the running manager with the installed binary, e.g. after an
/// upgrade. Agents keep running; the new manager reconnects to their holders.
pub fn manager_restart() -> Result<()> {
    if let Some(mut conn) = Conn::open(false)? {
        let Response::Hello { pid, .. } = conn.request(&hello_request())? else {
            bail!("unexpected reply to Hello");
        };
        conn.request(&Request::Shutdown { kill_agents: false })?;
        // The old manager holds the single-instance lock until it exits.
        let deadline = Instant::now() + Duration::from_secs(5);
        while signal_process(Pid::from_raw(pid as i32), None).is_ok() {
            if Instant::now() > deadline {
                bail!("the old manager (pid {pid}) did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Conn::connect()?;
    manager_status()
}

pub fn manager_status() -> Result<()> {
    let Some(mut conn) = Conn::open(false)? else {
        println!("manager is not running");
        return Ok(());
    };
    let Response::Hello { pid, version, capabilities } = conn.request(&hello_request())? else {
        bail!("unexpected reply to Hello");
    };
    let agents = conn.list(true)?;
    let running = agents.iter().filter(|a| a.status.is_live()).count();
    println!("manager running (pid {pid}, protocol v{version}), {running} running / {} total agents", agents.len());
    println!("capabilities: {}", capabilities.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>().join(", "));
    println!("socket: {}", paths::manager_socket().display());
    Ok(())
}

fn hello_request() -> Request {
    Request::Hello { version: MANAGER_PROTOCOL_VERSION, capabilities: MANAGER_CAPABILITIES.to_vec() }
}

pub fn terminal_size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills a winsize struct; failure leaves it zeroed.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}

#[cfg(test)]
mod tests {
    use super::split_name;

    #[test]
    fn table_separates_group_from_final_name() {
        assert_eq!(split_name("codex-1"), ("-", "codex-1"));
        assert_eq!(split_name("frontend/codex-1"), ("frontend", "codex-1"));
        assert_eq!(split_name("company/frontend/codex-1"), ("company/frontend", "codex-1"));
    }
}
