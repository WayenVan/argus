//! User-facing commands. The client is stateless: one request, one response.

use std::fs::OpenOptions;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{AgentInfo, Request, Response, RunRequest, now_secs};
use argus_proto::{PROTOCOL_VERSION, paths};

const START_TIMEOUT: Duration = Duration::from_secs(2);

struct Conn {
    stream: UnixStream,
}

impl Conn {
    /// Connects to the manager, starting it first when `autostart` is set.
    fn open(autostart: bool) -> Result<Option<Conn>> {
        let socket = paths::manager_socket();
        let stream = match UnixStream::connect(&socket) {
            Ok(s) => s,
            Err(_) if autostart => {
                spawn_manager()?;
                wait_for_socket(&socket)?
            }
            Err(_) => return Ok(None),
        };
        let mut conn = Conn { stream };
        match conn.request(&Request::Hello { version: PROTOCOL_VERSION })? {
            Response::Hello { version, .. } if version == PROTOCOL_VERSION => Ok(Some(conn)),
            Response::Hello { version, .. } => bail!(
                "manager speaks protocol v{version}, this argus speaks v{PROTOCOL_VERSION}; run `argus manager stop` and retry"
            ),
            other => bail!("unexpected handshake reply: {other:?}"),
        }
    }

    fn connect() -> Result<Conn> {
        Ok(Conn::open(true)?.expect("autostart always yields a connection"))
    }

    fn request(&mut self, req: &Request) -> Result<Response> {
        frame::write_json(&mut self.stream, req).context("sending request to manager")?;
        let Some((t, payload)) = frame::read_frame(&mut self.stream)? else {
            bail!("manager closed the connection");
        };
        if t != ty::CONTROL {
            bail!("unexpected frame type {t:#x} from manager");
        }
        match serde_json::from_slice(&payload)? {
            Response::Error { message, .. } => bail!(message),
            resp => Ok(resp),
        }
    }
}

fn spawn_manager() -> Result<()> {
    paths::ensure_private_dir(&paths::runtime_dir())?;
    paths::ensure_private_dir(&paths::state_dir())?;
    let log = OpenOptions::new().create(true).append(true).open(paths::manager_log())?;
    let exe = std::env::current_exe().context("locating the argus binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(["manager", "run"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; it detaches the manager from our terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
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
                return Err(e).with_context(|| {
                    format!("manager did not start; see {}", paths::manager_log().display())
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub fn run(
    name: Option<String>,
    group: Option<String>,
    cwd: Option<PathBuf>,
    kind: String,
    args: Vec<String>,
) -> Result<()> {
    let cwd = match cwd {
        Some(dir) => std::fs::canonicalize(&dir).with_context(|| format!("no such directory: {}", dir.display()))?,
        None => std::env::current_dir()?,
    };
    let group = group.or_else(|| std::env::var("ARGUS_GROUP").ok()).filter(|g| !g.is_empty());
    let (rows, cols) = terminal_size();
    let mut command = vec![kind];
    command.extend(args);
    let req = RunRequest {
        command,
        name,
        group,
        cwd: cwd.to_string_lossy().into_owned(),
        env: std::env::vars().collect(),
        rows,
        cols,
    };
    match Conn::connect()?.request(&Request::Run(req))? {
        Response::Agent { agent } => {
            println!("{}\t{}", agent.id, agent.name);
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn ps(prefix: Option<String>, all: bool, json: bool) -> Result<()> {
    let Response::Agents { agents } = Conn::connect()?.request(&Request::List { all, prefix })? else {
        bail!("unexpected reply to List");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&agents)?);
        return Ok(());
    }
    print_table(&agents);
    Ok(())
}

fn print_table(agents: &[AgentInfo]) {
    let home = std::env::var("HOME").unwrap_or_default();
    let now = now_secs();
    let rows: Vec<[String; 6]> = agents
        .iter()
        .map(|a| {
            let status = match (a.status.as_str(), a.exit_code) {
                ("exited", Some(code)) => format!("exited({code})"),
                (s, _) => s.to_string(),
            };
            let cwd = match a.cwd.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => a.cwd.clone(),
            };
            [a.id.to_string(), a.name.clone(), a.kind.clone(), status, age(now.saturating_sub(a.created_at)), cwd]
        })
        .collect();
    let header = ["ID", "NAME", "KIND", "STATUS", "AGE", "CWD"].map(String::from);
    let mut widths = header.clone().map(|h| h.len());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    for row in std::iter::once(&header).chain(&rows) {
        let line: Vec<String> = row.iter().zip(widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        println!("{}", line.join("  ").trim_end());
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

pub fn manager_status() -> Result<()> {
    let Some(mut conn) = Conn::open(false)? else {
        println!("manager is not running");
        return Ok(());
    };
    let Response::Hello { pid, version } = conn.request(&Request::Hello { version: PROTOCOL_VERSION })? else {
        bail!("unexpected reply to Hello");
    };
    let Response::Agents { agents } = conn.request(&Request::List { all: true, prefix: None })? else {
        bail!("unexpected reply to List");
    };
    let running = agents.iter().filter(|a| a.status.is_live()).count();
    println!("manager running (pid {pid}, protocol v{version}), {running} running / {} total agents", agents.len());
    println!("socket: {}", paths::manager_socket().display());
    Ok(())
}

fn terminal_size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills a winsize struct; failure leaves it zeroed.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}
