//! Long-running commands: `logs`, `ps -w`, `events`, `wait`.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{
    AgentEvent, AgentInfo, Availability, HOLDER_CAPABILITIES, HolderRequest, HolderResponse, Request, Response,
    SubscribeLevel,
};
use argus_proto::{HOLDER_PROTOCOL_VERSION, paths};

use crate::attach;
use crate::client::{self, Conn, PsOptions};
use crate::errors::{self, EXITED, NOT_READY, TIMEOUT, coded};
use crate::output::{self, AgentView, OneAgent};
use crate::wait;
use serde::Serialize;

/// Ends passive log rendering at a clean shell boundary. The allowlist means
/// only SGR state can remain. This is never written into redirected data.
const LOG_TERMINAL_END: &[u8] = b"\x1b[0m\r\n";

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

/// Prints recent output. Running agents are read from their holder (at most
/// the 1 MiB ring buffer); exited agents from the `output.log` it left.
pub fn logs(target: String, bytes: Option<u64>, follow: bool, raw: bool, screen: bool) -> Result<()> {
    if screen {
        return logs_screen(&target);
    }
    let agent = Conn::connect()?.find(&target)?;
    // A connect failure means it exited just now; its log is on disk.
    if agent.status.is_live()
        && let Ok(stream) = UnixStream::connect(paths::holder_socket(agent.id))
    {
        return logs_live(stream, &agent, bytes, follow, raw);
    }
    let path = paths::output_log(&paths::agent_dir(agent.id));
    let data = std::fs::read(&path).with_context(|| format!("no output recorded for {}", agent.name))?;
    let start = bytes.map_or(0, |n| data.len().saturating_sub(n as usize));
    let output = if raw { data[start..].to_vec() } else { argus_proto::ansi::filter_logs(&data[start..]) };
    let restore_terminal = !raw && io::stdout().is_terminal();
    let mut out = io::stdout().lock();
    out.write_all(&output)?;
    if restore_terminal {
        out.write_all(LOG_TERMINAL_END)?;
    }
    out.flush()?;
    Ok(())
}

/// The manager's virtual-terminal rendering of the agent's current screen: a
/// point-in-time snapshot of a moment, not a stream, so it takes its own path
/// through the manager rather than the holder ring buffer `logs` otherwise
/// reads. The bytes are entirely manager-synthesized (the same escape-code
/// generator `attach` restore uses), never a verbatim copy of agent output,
/// so this stays safe to print without the `LogFilter` allowlist.
fn logs_screen(target: &str) -> Result<()> {
    let mut conn = Conn::connect()?;
    let Response::ScreenDump { bytes, .. } = conn.request(&Request::ScreenDump { target: target.to_string() })? else {
        bail!("unexpected reply to ScreenDump");
    };
    let restore_terminal = io::stdout().is_terminal();
    let mut out = io::stdout().lock();
    out.write_all(&bytes)?;
    if restore_terminal {
        out.write_all(LOG_TERMINAL_END)?;
    }
    out.flush()?;
    Ok(())
}

fn logs_live(mut stream: UnixStream, agent: &AgentInfo, bytes: Option<u64>, follow: bool, raw: bool) -> Result<()> {
    let restore_terminal = !raw && io::stdout().is_terminal();
    let hello = attach::call(
        &mut stream,
        &HolderRequest::Hello { version: HOLDER_PROTOCOL_VERSION, capabilities: HOLDER_CAPABILITIES.to_vec() },
    )?;
    attach::validate_holder_hello(hello)?;
    let HolderResponse::Info(info) = attach::call(&mut stream, &HolderRequest::Info)? else {
        bail!("unexpected reply to Info");
    };
    let end = info.output_offset;
    let from = bytes.map_or(0, |n| end.saturating_sub(n));
    if !follow && from >= end {
        return Ok(());
    }
    let subscribe = HolderRequest::Subscribe { level: SubscribeLevel::Output, from_offset: Some(from) };
    attach::call(&mut stream, &subscribe)?;

    let mut out = io::stdout().lock();
    let mut log_filter = (!raw).then(argus_proto::ansi::LogFilter::default);
    while let Some((t, payload)) = frame::read_frame(&mut stream)? {
        match t {
            ty::DATA if payload.len() >= 8 => {
                let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
                let data = &payload[8..];
                if follow {
                    write_log_bytes(&mut out, &mut log_filter, data)?;
                    out.flush()?;
                    continue;
                }
                // Without --follow, stop at the end offset seen when we started.
                let keep = end.saturating_sub(offset).min(data.len() as u64) as usize;
                write_log_bytes(&mut out, &mut log_filter, &data[..keep])?;
                if offset + data.len() as u64 >= end {
                    break;
                }
            }
            ty::SKIPPED => eprintln!("[argus] output skipped: this terminal fell behind"),
            ty::EXIT if payload.len() == 4 => {
                out.flush()?;
                let code = i32::from_be_bytes(payload[..4].try_into().unwrap());
                eprintln!("[{} exited with code {code}]", agent.name);
                break;
            }
            _ => {}
        }
    }
    if let Some(filter) = &mut log_filter {
        let mut tail = Vec::new();
        filter.finish(&mut tail);
        out.write_all(&tail)?;
    }
    if restore_terminal {
        out.write_all(LOG_TERMINAL_END)?;
    }
    out.flush()?;
    Ok(())
}

fn write_log_bytes(
    out: &mut impl Write,
    filter: &mut Option<argus_proto::ansi::LogFilter>,
    bytes: &[u8],
) -> io::Result<()> {
    if let Some(filter) = filter {
        let mut safe = Vec::with_capacity(bytes.len());
        filter.write_filtered(bytes, &mut safe);
        out.write_all(&safe)
    } else {
        out.write_all(bytes)
    }
}

// ---------------------------------------------------------------------------
// Watch plumbing
// ---------------------------------------------------------------------------

pub(crate) type Messages = Receiver<Result<Option<Response>>>;

/// Starts a watch and moves the connection to a reader thread, so callers
/// can wait for messages with a timeout without breaking frame boundaries.
pub(crate) fn start_watch(ids: Option<Vec<u64>>, include_exited: bool) -> Result<(Vec<AgentInfo>, Messages)> {
    let mut conn = Conn::connect()?;
    let Response::Snapshot { agents, .. } = conn.request(&Request::Watch { ids, include_exited })? else {
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
    Ok((agents, rx))
}

/// Applies one watch message to a local copy of the agent table.
pub(crate) fn apply(table: &mut BTreeMap<u64, AgentInfo>, msg: &Response) {
    match msg {
        Response::Snapshot { agents, .. } => {
            *table = agents.iter().map(|a| (a.id, a.clone())).collect();
        }
        Response::Event { event, .. } => match event {
            AgentEvent::Created { agent } | AgentEvent::Updated { agent } | AgentEvent::Exited { agent } => {
                table.insert(agent.id, agent.clone());
            }
            AgentEvent::Removed { id } => {
                table.remove(id);
            }
            AgentEvent::Resync => {} // The snapshot that follows replaces everything.
        },
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// ps -w
// ---------------------------------------------------------------------------

pub fn ps_watch(opts: &PsOptions) -> Result<()> {
    loop {
        let (agents, rx) = start_watch(None, opts.all)?;
        let mut table: BTreeMap<u64, AgentInfo> = agents.into_iter().map(|a| (a.id, a)).collect();
        loop {
            render(&table, opts)?;
            // Redraw at least once a second so AGE keeps moving.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(Some(msg))) => apply(&mut table, &msg),
                Ok(Ok(None)) | Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        // The manager went away (restart or upgrade): reconnect.
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn render(table: &BTreeMap<u64, AgentInfo>, opts: &PsOptions) -> Result<()> {
    let agents: Vec<AgentInfo> = table.values().filter(|a| opts.keeps(a)).cloned().collect();
    let mut out = io::stdout().lock();
    let written = write!(out, "\x1b[H\x1b[2Jargus ps -w  (Ctrl-C to quit)\n\n{}", client::format_table(&agents))
        .and_then(|_| out.flush());
    if written.is_err() {
        std::process::exit(0); // stdout closed
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// Streams events until interrupted. A closed stdout (e.g. `| head`) ends it
/// quietly.
pub fn events(json: bool) -> Result<()> {
    let mut out = io::stdout().lock();
    loop {
        let (_, rx) = start_watch(None, true)?;
        while let Ok(Ok(Some(msg))) = rx.recv() {
            let line = if json { event_line(&msg) } else { describe(&msg) };
            if line.is_empty() {
                continue;
            }
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `agent` for changes to one, `id` once it is removed.
#[derive(Serialize)]
struct EventLine<'a> {
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<AgentView<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
}

/// An event as one `--json` line; empty for anything but an event.
fn event_line(msg: &Response) -> String {
    fn line<'a>(event: &'static str, agent: &'a AgentInfo) -> EventLine<'a> {
        EventLine { event, agent: Some(AgentView::new(agent)), id: None }
    }
    let Response::Event { event, .. } = msg else { return String::new() };
    output::to_line(match event {
        AgentEvent::Created { agent } => line("created", agent),
        AgentEvent::Updated { agent } => line("updated", agent),
        AgentEvent::Exited { agent } => line("exited", agent),
        AgentEvent::Removed { id } => EventLine { event: "removed", agent: None, id: Some(*id) },
        // Events may have been missed; re-read the list.
        AgentEvent::Resync => EventLine { event: "resync", agent: None, id: None },
    })
}

fn describe(msg: &Response) -> String {
    let Response::Event { event, .. } = msg else { return String::new() };
    let line = match event {
        AgentEvent::Created { agent } => format!("created  {:<4} {}", agent.id, agent.name),
        AgentEvent::Updated { agent } => format!(
            "updated  {:<4} {}  {}  activity={} attached={}",
            agent.id,
            agent.name,
            agent.status.as_str(),
            agent.activity,
            agent.attached
        ),
        AgentEvent::Exited { agent } => format!(
            "exited   {:<4} {}  {}",
            agent.id,
            agent.name,
            agent.exit_code.map_or("lost".into(), |c| format!("code {c}"))
        ),
        AgentEvent::Removed { id } => format!("removed  {id}"),
        AgentEvent::Resync => "resync".into(),
    };
    format!("{} {line}", clock())
}

fn clock() -> String {
    // SAFETY: localtime_r fills a local tm from a local time_t.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

// ---------------------------------------------------------------------------
// wait
// ---------------------------------------------------------------------------

/// Blocks until the agent exits (and exits with its code), or until its
/// activity equals `until`. Exits 124 on timeout, like `timeout(1)`.
/// Prints where a wait ended: `text`, or the agent under `--json`.
fn print_result(json: bool, agent: &AgentInfo, text: &str) {
    if json {
        output::print(OneAgent::new(agent));
    } else {
        println!("{text}");
    }
}

// ---------------------------------------------------------------------------
// send
// ---------------------------------------------------------------------------

pub struct SendOptions {
    /// The prompt; `-` reads it from stdin.
    pub text: String,
    pub enter: bool,
    pub force: bool,
    /// Wait until the agent can take the prompt instead of failing.
    pub wait: bool,
    /// After sending, block until the agent is waiting on someone again.
    pub then_wait: bool,
    pub timeout: Option<u64>,
    pub json: bool,
}

/// Types a prompt into an agent, but only while it waits for one; the
/// manager decides, so the check and the typing cannot race.
pub fn send(target: String, opts: SendOptions) -> Result<()> {
    let text = if opts.text == "-" {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text).context("reading the prompt from stdin")?;
        let trimmed = text.trim_end_matches(['\n', '\r']).len();
        text.truncate(trimmed);
        text
    } else {
        opts.text
    };
    let deadline = opts.timeout.map(|s| Instant::now() + Duration::from_secs(s));
    let mut conn = Conn::connect()?;
    if !opts.wait && !opts.then_wait {
        if !opts.json {
            return conn.request(&Request::Send { target, text, enter: opts.enter, force: opts.force }).map(drop);
        }
        let id = conn.find(&target)?.id.to_string();
        let reply = conn.request(&Request::Send { target: id.clone(), text, enter: opts.enter, force: opts.force })?;
        output::print(SentAgent { agent: AgentView::new(&conn.find(&id)?), turn: sent_turn(&reply) });
        return Ok(());
    }

    // Watch before sending, so no activity change after the send is missed.
    let agent = conn.find(&target)?;
    let (agents, rx) = start_watch(Some(vec![agent.id]), true)?;
    let mut current = agents.into_iter().next().unwrap_or(agent);
    let request = Request::Send { target: current.id.to_string(), text, enter: opts.enter, force: opts.force };
    let turn = loop {
        if opts.force || current.activity.awaits_prompt() {
            match conn.request(&request) {
                Ok(reply) => break sent_turn(&reply),
                Err(e) if opts.wait && errors::has_code(&e, NOT_READY) => {}
                Err(e) => return Err(e),
            }
        } else if !opts.wait {
            let why = current.activity.send_refusal().unwrap_or_default();
            return Err(coded(NOT_READY, format!("{} {why}", current.name)));
        }
        // Someone typing clears without an activity change, so poll too.
        next_update(&rx, &mut current, deadline, Duration::from_secs(1))?;
    };
    if !opts.then_wait {
        if opts.json {
            output::print(SentAgent { agent: AgentView::new(&current), turn });
        }
        return Ok(());
    }
    let Some(turn) = turn else {
        bail!("sent, but the manager predates turn counting and cannot wait for the turn; run `argus manager restart`");
    };
    let pickup = wait::Pickup { sent: current.clone(), at: Instant::now() };
    let goal = wait::Goal::Availability(Availability::Free);
    let agent = wait::after_turn(current, &rx, turn, &goal, deadline, Some(pickup), !opts.json)?;
    print_result(opts.json, &agent, &agent.activity.to_string());
    Ok(())
}

/// `argus send --json`: the agent, and its `turns` when the prompt went in
/// (`null` from a manager that predates turn counting).
#[derive(Serialize)]
struct SentAgent<'a> {
    agent: AgentView<'a>,
    turn: Option<u64>,
}

/// The turn a `Send` reply names; an older manager answers plain `Ok`.
fn sent_turn(reply: &Response) -> Option<u64> {
    match reply {
        Response::Sent { turn } => Some(*turn),
        _ => None,
    }
}

/// Waits at most `poll` for the next change to `current`. Exits 124 once
/// `deadline` passes; fails if the agent stops running.
pub fn next_update(rx: &Messages, current: &mut AgentInfo, deadline: Option<Instant>, poll: Duration) -> Result<()> {
    let wait_for = deadline.map_or(poll, |d| d.saturating_duration_since(Instant::now()).min(poll));
    match rx.recv_timeout(wait_for) {
        Ok(Ok(Some(msg))) => {
            let mut table = BTreeMap::from([(current.id, current.clone())]);
            apply(&mut table, &msg);
            match table.remove(&current.id) {
                Some(a) => *current = a,
                None => bail!("{} was removed", current.name),
            }
        }
        Err(RecvTimeoutError::Timeout) => {}
        Ok(Err(e)) => return Err(e),
        Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => bail!("lost connection to the manager"),
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(coded(TIMEOUT, format!("timed out waiting for {}", current.name)));
    }
    if !current.status.is_live() {
        return Err(coded(EXITED, format!("{} {}", current.name, current.status.as_str())));
    }
    Ok(())
}
