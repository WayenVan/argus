//! `argus attach`: connects this terminal straight to an agent's holder.
//!
//! Bytes pass through untouched. Input and output run on separate threads so
//! the detach key keeps working while the terminal is busy drawing.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{AttachRequest, HolderRequest, HolderResponse};
use argus_proto::{PROTOCOL_VERSION, paths};

/// Ctrl-\ (FS). In raw mode it arrives as a byte instead of SIGQUIT.
const DETACH_KEY: u8 = 0x1c;
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

const ENTER: &str = "\x1b[?1004h\x1b[H\x1b[2J";
/// Leaves the agent's terminal modes behind: focus reporting, alternate
/// screen, mouse modes, bracketed paste, hidden cursor, colours.
const RESET: &str = "\x1b[?1004l\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[0m";

pub struct Options {
    pub readonly: bool,
    pub steal: bool,
    pub replay: bool,
}

pub struct Target {
    pub socket: PathBuf,
    pub name: String,
}

enum Ending {
    Detached,
    Exited(i32),
    Kicked,
    Lost,
}

static WINCH_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_winch(_: libc::c_int) {
    let fd = WINCH_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: write is async-signal-safe.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

pub fn attach(target: &Target, opts: Options) -> Result<()> {
    // SAFETY: isatty has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        bail!("attach needs a terminal on stdin");
    }
    let mut stream = UnixStream::connect(&target.socket)
        .with_context(|| format!("{} is not reachable (has it exited?)", target.name))?;
    call(&mut stream, &HolderRequest::Hello { version: PROTOCOL_VERSION })?;
    let (rows, cols) = crate::client::terminal_size();
    call(
        &mut stream,
        &HolderRequest::Attach(AttachRequest {
            rows,
            cols,
            readonly: opts.readonly,
            steal: opts.steal,
            replay: opts.replay,
        }),
    )?;

    let ending = {
        let _raw = RawMode::enter()?;
        print_raw(ENTER);
        let ending = pump(stream, opts.readonly);
        print_raw(RESET);
        ending
    };

    match ending? {
        Ending::Detached => eprintln!("[detached from {}]", target.name),
        Ending::Exited(code) => eprintln!("[{} exited with code {code}]", target.name),
        Ending::Kicked => eprintln!("[{} was attached elsewhere]", target.name),
        Ending::Lost => eprintln!("[connection to {} lost]", target.name),
    }
    Ok(())
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

fn pump(stream: UnixStream, readonly: bool) -> Result<Ending> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let detached = Arc::new(AtomicBool::new(false));

    // Keyboard → holder.
    {
        let writer = writer.clone();
        let detached = detached.clone();
        std::thread::spawn(move || forward_input(writer, detached, readonly));
    }
    // Window size changes → holder.
    {
        let writer = writer.clone();
        let winch = install_winch()?;
        std::thread::spawn(move || forward_resizes(winch, writer));
    }

    // Holder → screen, on this thread. Blocking writes to stdout push back on
    // the holder, which drops the backlog instead of stalling the agent.
    let mut reader = stream;
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

fn forward_input(writer: Arc<Mutex<UnixStream>>, detached: Arc<AtomicBool>, readonly: bool) {
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut chunk = &buf[..n];
        let detach_at = chunk.iter().position(|&b| b == DETACH_KEY);
        if let Some(pos) = detach_at {
            chunk = &chunk[..pos];
        }

        // Terminals send focus reports as one write, so they arrive whole.
        let mut data = Vec::with_capacity(chunk.len());
        let mut focus = false;
        let mut i = 0;
        while i < chunk.len() {
            if chunk[i..].starts_with(FOCUS_IN) {
                focus = true;
                i += FOCUS_IN.len();
            } else if chunk[i..].starts_with(FOCUS_OUT) {
                i += FOCUS_OUT.len();
            } else {
                data.push(chunk[i]);
                i += 1;
            }
        }

        let mut w = writer.lock().unwrap();
        if focus && frame::write_frame(&mut *w, ty::FOCUS, &[]).is_err() {
            return;
        }
        if !readonly && !data.is_empty() && frame::write_frame(&mut *w, ty::DATA, &data).is_err() {
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

fn forward_resizes(winch: std::fs::File, writer: Arc<Mutex<UnixStream>>) {
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

/// Returns the read end of a pipe that receives a byte on every SIGWINCH.
fn install_winch() -> Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: fds is a valid 2-element buffer; the handler only calls write.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error()).context("pipe");
        }
        WINCH_PIPE.store(fds[1], Ordering::Relaxed);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_winch as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
        Ok(std::fs::File::from_raw_fd(fds[0]))
    }
}

/// Puts the terminal in raw mode and restores it on drop, including on panic.
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> Result<RawMode> {
        // SAFETY: tcgetattr/tcsetattr on stdin with a local termios.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return Err(io::Error::last_os_error()).context("reading terminal settings");
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error()).context("entering raw mode");
            }
            Ok(RawMode { saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the settings captured in `enter`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

fn print_raw(s: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
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
        return Ok(Target { socket: paths::holder_socket(id), name });
    }
    if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Target { socket: paths::holder_socket(target.parse()?), name: format!("agent {target}") });
    }
    let link = paths::name_socket(target);
    if link.exists() {
        return Ok(Target { socket: link, name: target.to_string() });
    }
    bail!("no running agent named {target} (the manager is not running, so use an ID or the full name)")
}
