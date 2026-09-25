//! `argus attach`: connects this terminal straight to an agent's holder.
//!
//! Bytes pass through untouched. Input and output run on separate threads so
//! the detach key keeps working while the terminal is busy drawing.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{
    AttachRequest, HOLDER_CAPABILITIES, HolderRequest, HolderResponse, Request, Response, ScreenMode,
};
use argus_proto::{HOLDER_PROTOCOL_VERSION, paths};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::isatty;
use signal_hook::SigId;
use signal_hook::consts::SIGWINCH;

use crate::client::Conn;
use crate::term;

/// Ctrl-\ (FS). In raw mode it arrives as a byte instead of SIGQUIT, unless
/// the agent turned on an extended keyboard mode: see [`find_detach`].
const DETACH_KEY: u8 = 0x1c;
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

/// Argus owns an alternate screen for the whole attachment. Some agents
/// (notably Codex) draw on the normal screen, so relying on the child to enter
/// one leaves its last frame in the caller's scrollback after detach.
const ENTER: &str = "\x1b[?1049h\x1b[?1004h";
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_LEAVE: &str = "\x1b[?1049l";
/// Home + clear, for when there is no screen restore to draw instead.
const CLEAR: &str = "\x1b[H\x1b[2J";
/// Leaves the agent's terminal modes behind: synchronized-output hold
/// (released first, so the rest of this actually reaches the screen instead
/// of sitting in a buffered frame until the terminal's own timeout), kitty
/// keyboard flags (popped before leaving the alternate screen, which has its
/// own stack), modifyOtherKeys, focus reporting, alternate screen, mouse
/// modes, bracketed paste, colour scheme reports, hidden cursor, cursor
/// colour, colours. The cursor shape is restored separately, to what it was
/// before argus started: see [`term::Profile::cursor_shape`].
const RESET: &str = "\x1b[?2026l\x1b[<u\x1b[>4m\x1b[?2031l\x1b[?1004l\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b]112\x07\x1b[0m";

pub struct Options {
    pub readonly: bool,
    pub steal: bool,
    pub replay: bool,
    pub allow_clipboard_replay: bool,
    /// The caller already holds the alternate screen and redraws it after:
    /// the TUI. Switching screens around the session would flash whatever
    /// the normal screen holds on the way in and out.
    pub shared_screen: bool,
}

pub struct Target {
    pub socket: PathBuf,
    pub name: String,
    /// Known when resolved through the manager (or given as a bare ID); lets
    /// `attach` ask the manager for a screen to restore.
    pub id: Option<u64>,
}

enum Ending {
    Detached,
    Exited(i32),
    Kicked,
    Lost,
}

pub fn attach(target: &Target, opts: Options) -> Result<()> {
    eprintln!("{}", session(target, opts)?);
    Ok(())
}

/// Runs one attach session and returns the line describing how it ended,
/// leaving it to the caller whether to print it: the TUI shows it in its
/// footer instead, so repeated attaches don't pile lines up behind it.
pub fn session(target: &Target, opts: Options) -> Result<String> {
    if !isatty(io::stdin().as_raw_fd())? {
        bail!("attach needs a terminal on stdin");
    }
    let profile = term::profile();
    let (rows, cols) = crate::client::terminal_size();
    let restore = target.id.and_then(fetch_screen);

    let mut stream = UnixStream::connect(&target.socket)
        .with_context(|| format!("{} is not reachable (has it exited?)", target.name))?;
    let hello = call(
        &mut stream,
        &HolderRequest::Hello { version: HOLDER_PROTOCOL_VERSION, capabilities: HOLDER_CAPABILITIES.to_vec() },
    )?;
    validate_holder_hello(hello)?;
    // A Snapshot or Replay reply gives an offset to pick up from; the
    // manager already fed the bytes before that offset into it (Snapshot)
    // or they are being drawn locally from the ring buffer (Replay).
    let from_offset = restore.as_ref().filter(|r| !matches!(r.mode, ScreenMode::Unavailable)).map(|r| r.offset);
    call(
        &mut stream,
        &HolderRequest::Attach(AttachRequest {
            rows,
            cols,
            readonly: opts.readonly,
            steal: opts.steal,
            replay: opts.replay,
            allow_clipboard_replay: opts.allow_clipboard_replay,
            from_offset,
            colors: profile.colors.clone(),
            tmux: crate::tmux::current_location(),
        }),
    )?;

    let ending = {
        let _raw = term::RawMode::enter()?;
        let _display = DisplaySession::enter(opts.shared_screen);
        match &restore {
            // Only worth drawing if it is a real redraw of the size we are
            // about to show it at; otherwise the holder will resize the PTY
            // for real and the agent redraws itself for the new dimensions.
            Some(r) if r.mode == ScreenMode::Snapshot && (r.rows, r.cols) == (rows, cols) => {
                print_raw_bytes(snapshot_body(&r.bytes))
            }
            _ => print_raw(CLEAR),
        }
        pump(stream, opts.readonly)
    };

    Ok(match ending? {
        Ending::Detached => format!("[detached from {}]", target.name),
        Ending::Exited(code) => format!("[{} exited with code {code}]", target.name),
        Ending::Kicked => format!("[{} was attached elsewhere]", target.name),
        Ending::Lost => format!("[connection to {} lost]", target.name),
    })
}

struct Restore {
    mode: ScreenMode,
    rows: u16,
    cols: u16,
    offset: u64,
    bytes: Vec<u8>,
}

/// Asks the manager for a screen to restore `id` with. `None` if the manager
/// is unreachable or the reply is not a screen; either way `attach` falls
/// back to the first-stage clear-and-resize dance.
fn fetch_screen(id: u64) -> Option<Restore> {
    let mut conn = Conn::open(false).ok().flatten()?;
    match conn.request(&Request::Screen { target: id.to_string(), since_offset: None }).ok()? {
        Response::Screen { mode, rows, cols, offset, bytes } => Some(Restore { mode, rows, cols, offset, bytes }),
        _ => None,
    }
}

/// One request/response exchange with a holder, before any stream frames.
pub fn call(stream: &mut UnixStream, req: &HolderRequest) -> Result<HolderResponse> {
    frame::write_json(stream, req)?;
    let Some((t, payload)) = frame::read_frame(stream)? else { bail!("holder closed the connection") };
    if t != ty::CONTROL {
        bail!("unexpected frame {t:#x} from holder");
    }
    match serde_json::from_slice(&payload)? {
        HolderResponse::Error { message } => bail!(message),
        resp => Ok(resp),
    }
}

pub fn validate_holder_hello(response: HolderResponse) -> Result<()> {
    match response {
        HolderResponse::Hello { version, .. } if version == HOLDER_PROTOCOL_VERSION => Ok(()),
        HolderResponse::Hello { version, .. } => {
            bail!("holder speaks protocol v{version}, this argus speaks v{HOLDER_PROTOCOL_VERSION}")
        }
        other => bail!("unexpected holder handshake reply: {other:?}"),
    }
}

fn pump(stream: UnixStream, readonly: bool) -> Result<Ending> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let detached = Arc::new(AtomicBool::new(false));
    // The user just ran `argus attach` here, so this terminal has focus now;
    // terminals only report focus when it changes.
    frame::write_frame(&mut *writer.lock().unwrap(), ty::FOCUS, &[1])?;

    // Keyboard → holder. The input thread must be gone before this returns:
    // left blocked on stdin, it would keep eating keystrokes meant for the
    // caller (the TUI, or the next attach) and send them to this holder,
    // which lingers after its agent exits. Dropping `stop_tx` wakes it.
    let (stop_rx, stop_tx) = UnixStream::pair()?;
    let input = {
        let writer = writer.clone();
        let detached = detached.clone();
        std::thread::spawn(move || forward_input(writer, detached, readonly, stop_rx))
    };
    // Window size changes → holder. Keep the registration in this function's
    // scope: dropping it unregisters SIGWINCH and closes the pipe writer.
    let (winch, _winch_registration) = winch_pipe()?;
    {
        let writer = writer.clone();
        std::thread::spawn(move || forward_resizes(winch, writer));
    }

    let ending = forward_output(&stream, &detached);
    drop(stop_tx);
    // Also unblocks an input thread stuck writing to a holder that stopped reading.
    let _ = stream.shutdown(Shutdown::Both);
    let _ = input.join();
    ending
}

/// Holder → screen, on the calling thread. Blocking writes to stdout push
/// back on the holder, which drops the backlog instead of stalling the agent.
fn forward_output(stream: &UnixStream, detached: &AtomicBool) -> Result<Ending> {
    // Buffered: a frame's header and payload, and often several frames,
    // then arrive in one read.
    let mut reader = io::BufReader::with_capacity(frame::READ_BUFFER, stream);
    let mut out = io::stdout().lock();
    loop {
        let frame = match frame::read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) if detached.load(Ordering::SeqCst) => return Ok(Ending::Detached),
            Ok(None) | Err(_) => return Ok(Ending::Lost),
        };
        match frame {
            (ty::DATA, bytes) => {
                out.write_all(&bytes)?;
                out.flush()?;
            }
            (ty::EXIT, code) if code.len() == 4 => {
                return Ok(Ending::Exited(i32::from_be_bytes(code[..4].try_into().unwrap())));
            }
            (ty::KICKED, _) => return Ok(Ending::Kicked),
            (ty::SKIPPED, range) if range.len() == 16 => {
                let from = u64::from_be_bytes(range[..8].try_into().unwrap());
                let to = u64::from_be_bytes(range[8..].try_into().unwrap());
                let skipped = to.saturating_sub(from);
                write!(out, "\x1b[H\x1b[2J[argus] skipped {} of output, see `argus logs`\r\n", human_bytes(skipped))?;
                out.flush()?;
            }
            _ => {}
        }
    }
}

/// Reads fd 0 directly rather than through `io::stdin()`: its lock and buffer
/// would outlive this session, and `stop` could not interrupt a blocked read.
fn forward_input(writer: Arc<Mutex<UnixStream>>, detached: Arc<AtomicBool>, readonly: bool, stop: UnixStream) {
    let stdin = io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN), PollFd::new(stop.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(_) => return,
        }
        // Any event on `stop` means its other end was dropped.
        if fds[1].revents().is_some_and(|r| !r.is_empty()) {
            return;
        }
        if fds[0].revents().is_none_or(|r| r.is_empty()) {
            continue;
        }
        let n = match nix::unistd::read(stdin.as_raw_fd(), &mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(Errno::EINTR | Errno::EAGAIN) => continue,
            Err(_) => return,
        };
        let mut chunk = &buf[..n];
        let detach_at = find_detach(chunk);
        if let Some(pos) = detach_at {
            chunk = &chunk[..pos];
        }

        // Terminals send focus reports as one write, so they arrive whole.
        let mut data = Vec::with_capacity(chunk.len());
        let mut focus = None;
        let mut i = 0;
        while i < chunk.len() {
            if chunk[i..].starts_with(FOCUS_IN) {
                focus = Some(true);
                i += FOCUS_IN.len();
            } else if chunk[i..].starts_with(FOCUS_OUT) {
                focus = Some(false);
                i += FOCUS_OUT.len();
            } else {
                data.push(chunk[i]);
                i += 1;
            }
        }

        let mut w = writer.lock().unwrap();
        if let Some(focused) = focus
            && frame::write_frame(&mut *w, ty::FOCUS, &[u8::from(focused)]).is_err()
        {
            return;
        }
        // Mouse moves are forwarded, but they are not the user doing anything.
        let kind = if only_mouse_reports(&data) { ty::MOUSE } else { ty::DATA };
        if !readonly && !data.is_empty() && frame::write_frame(&mut *w, kind, &data).is_err() {
            return;
        }
        if detach_at.is_some() {
            detached.store(true, Ordering::SeqCst);
            let _ = frame::write_frame(&mut *w, ty::DETACH, &[]);
            let _ = w.shutdown(Shutdown::Both);
            return;
        }
    }
}

/// Where the detach key starts in `input`. Besides the plain byte, agents
/// such as Claude Code turn on the kitty keyboard protocol or xterm's
/// modifyOtherKeys, and the terminal then sends Ctrl-\\ as `ESC [ 92 ; 5 u`
/// or `ESC [ 27 ; 5 ; 92 ~`.
fn find_detach(input: &[u8]) -> Option<usize> {
    (0..input.len())
        .find(|&i| input[i] == DETACH_KEY || input[i..].starts_with(b"\x1b[") && is_ctrl_backslash(&input[i + 2..]))
}

/// Whether the CSI sequence whose parameters start `csi` is Ctrl-\\.
fn is_ctrl_backslash(csi: &[u8]) -> bool {
    let Some(end) = csi.iter().position(|&b| (0x40..=0x7e).contains(&b)) else { return false };
    let Ok(params) = std::str::from_utf8(&csi[..end]) else { return false };
    // Each parameter may carry `:`-separated sub-fields; the first is the value.
    let params: Vec<Vec<&str>> = params.split(';').map(|p| p.split(':').collect()).collect();
    let value = |i: usize, j: usize| params.get(i).and_then(|p| p.get(j)).map(|v| v.parse::<u32>().ok());
    // Modifiers are 1 + a bitmask; Caps Lock (64) and Num Lock (128) do not count.
    let ctrl_only = |m: Option<Option<u32>>| matches!(m, Some(Some(m)) if m >= 1 && (m - 1) & !(64 | 128) == 4);
    match csi[end] {
        // Kitty: `92[:alternates];modifiers[:event]u`, press or repeat only.
        b'u' => {
            value(0, 0) == Some(Some(92)) && ctrl_only(value(1, 0)) && matches!(value(1, 1), None | Some(Some(1 | 2)))
        }
        // modifyOtherKeys: `27;modifiers;92~`.
        b'~' => {
            params.len() == 3
                && value(0, 0) == Some(Some(27))
                && ctrl_only(value(1, 0))
                && value(2, 0) == Some(Some(92))
        }
        _ => false,
    }
}

/// Whether `data` consists solely of terminal mouse reports: SGR
/// (`ESC [ < b ; x ; y M|m`) or legacy X10 (`ESC [ M` + 3 bytes).
fn only_mouse_reports(mut data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    while !data.is_empty() {
        if let Some(rest) = data.strip_prefix(b"\x1b[<") {
            let Some(end) = rest.iter().position(|&b| b == b'M' || b == b'm') else { return false };
            if !rest[..end].iter().all(|&b| b.is_ascii_digit() || b == b';') {
                return false;
            }
            data = &rest[end + 1..];
        } else if let Some(rest) = data.strip_prefix(b"\x1b[M") {
            if rest.len() < 3 {
                return false;
            }
            data = &rest[3..];
        } else {
            return false;
        }
    }
    true
}

fn forward_resizes(winch: UnixStream, writer: Arc<Mutex<UnixStream>>) {
    let mut winch = winch;
    let mut buf = [0u8; 64];
    while matches!(winch.read(&mut buf), Ok(n) if n > 0) {
        let (rows, cols) = crate::client::terminal_size();
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&rows.to_be_bytes());
        payload.extend_from_slice(&cols.to_be_bytes());
        if frame::write_frame(&mut *writer.lock().unwrap(), ty::RESIZE, &payload).is_err() {
            return;
        }
    }
}

/// A self-pipe registered with signal-hook. Dropping it unregisters the
/// handler and closes the writer, waking the reader thread cleanly.
struct SignalRegistration {
    registration: SigId,
}

fn winch_pipe() -> Result<(UnixStream, SignalRegistration)> {
    let (reader, writer) = UnixStream::pair()?;
    let registration = signal_hook::low_level::pipe::register(SIGWINCH, writer)?;
    Ok((reader, SignalRegistration { registration }))
}

impl Drop for SignalRegistration {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.registration);
    }
}

/// Keeps the agent's drawing off the caller's normal screen and restores all
/// terminal modes even when attaching returns with an error or unwinds.
struct DisplaySession {
    shared: bool,
}

impl DisplaySession {
    fn enter(shared: bool) -> Self {
        print_raw(if shared { ENTER.trim_start_matches("\x1b[?1049h") } else { ENTER });
        Self { shared }
    }
}

impl Drop for DisplaySession {
    fn drop(&mut self) {
        if self.shared {
            print_raw(&RESET.replace(ALT_SCREEN_LEAVE, ""));
        } else {
            print_raw(RESET);
        }
        print_raw(&term::restore_cursor_shape());
    }
}

/// Manager snapshots already begin by selecting the alternate screen. Attach
/// has selected its own above, so repeating 1049h could overwrite the
/// terminal's saved normal-screen cursor/state on some emulators.
fn snapshot_body(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(ALT_SCREEN_ENTER).unwrap_or(bytes)
}

fn print_raw(s: &str) {
    print_raw_bytes(s.as_bytes());
}

fn print_raw_bytes(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / (1 << 10) as f64),
        n => format!("{n} bytes"),
    }
}

/// Finds the holder socket for a target, preferring the manager's view
/// (which understands short names) and falling back to IDs and name links
/// when the manager is not running.
pub fn resolve(target: &str, from_manager: Option<(u64, String)>) -> Result<Target> {
    if let Some((id, name)) = from_manager {
        return Ok(Target { socket: paths::holder_socket(id), name, id: Some(id) });
    }
    if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
        let id: u64 = target.parse()?;
        return Ok(Target { socket: paths::holder_socket(id), name: format!("agent {target}"), id: Some(id) });
    }
    let link = paths::name_socket(target);
    if link.exists() {
        // The manager is unreachable (else `from_manager` would be set), so
        // there is no id to ask it for a screen with.
        return Ok(Target { socket: link, name: target.to_string(), id: None });
    }
    bail!("no running agent named {target} (the manager is not running, so use an ID or the full name)")
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::Duration;

    use super::{find_detach, only_mouse_reports, snapshot_body, winch_pipe};

    #[test]
    fn detach_key_in_every_encoding() {
        assert_eq!(find_detach(b"ab\x1c"), Some(2));
        assert_eq!(find_detach(b"x\x1b[92;5u"), Some(1));
        assert_eq!(find_detach(b"\x1b[92;69u"), Some(0), "caps lock on");
        assert_eq!(find_detach(b"\x1b[92:124;5:1u"), Some(0));
        assert_eq!(find_detach(b"\x1b[27;5;92~"), Some(0));
        assert_eq!(find_detach(b"\x1b[92;5:3u"), None, "release");
        assert_eq!(find_detach(b"\x1b[92;7u"), None, "ctrl+alt");
        assert_eq!(find_detach(b"\x1b[92u"), None, "plain backslash");
        assert_eq!(find_detach(b"\x1b[97;5u\x1b[<0;1;2M"), None);
    }

    #[test]
    fn mouse_reports_are_not_input() {
        assert!(only_mouse_reports(b"\x1b[<35;40;12M"));
        assert!(only_mouse_reports(b"\x1b[<35;40;12M\x1b[<35;41;12M\x1b[<0;41;12m"));
        assert!(only_mouse_reports(b"\x1b[M #!"));
        assert!(!only_mouse_reports(b"a"));
        assert!(!only_mouse_reports(b"\x1b[<35;40;12Mx"), "a keypress mixed in is input");
        assert!(!only_mouse_reports(b"\x1b[A"), "arrow keys are input");
        assert!(!only_mouse_reports(b"\x1b"), "a lone Esc is input");
        assert!(!only_mouse_reports(b""));
    }

    #[test]
    fn snapshot_does_not_enter_the_attach_screen_twice() {
        assert_eq!(snapshot_body(b"\x1b[?1049h\x1b[Hframe"), b"\x1b[Hframe");
        assert_eq!(snapshot_body(b"\x1b[Hframe"), b"\x1b[Hframe");
    }

    #[test]
    fn winch_registration_stays_live_until_its_guard_drops() {
        let (mut reader, registration) = winch_pipe().unwrap();
        reader.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();

        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        drop(registration);
    }
}
