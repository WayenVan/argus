//! The holder's single-threaded poll loop.
//!
//! Rules from the design:
//! - client input is handled before PTY output in every round;
//! - at most 64 KiB of output is read per round, coalesced without timers;
//! - the PTY master is never left unread because of a slow subscriber:
//!   a subscriber whose backlog exceeds 1 MiB gets `Skipped` instead.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argus_proto::frame::{self, MAX_FRAME, ty};
use argus_proto::msg::{
    ExitRecord, HolderInfo, HolderRequest, HolderResponse, HolderSpec, SubscribeLevel, now_secs,
};
use argus_proto::{PROTOCOL_VERSION, paths};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::spawn::{self, cloexec_pipe, set_nonblocking};

const READ_CHUNK: usize = 64 * 1024;
const RING_INITIAL: usize = 256 * 1024;
const RING_MAX: usize = 1024 * 1024;
const QUEUE_LIMIT: usize = 1024 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(5);
const LINGER: Duration = Duration::from_secs(60);

static SIGCHLD_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_sigchld(_: libc::c_int) {
    let fd = SIGCHLD_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: write is async-signal-safe; the pipe is non-blocking.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

pub struct Holder {
    spec: HolderSpec,
    listener: UnixListener,
    master: Option<OwnedFd>,
    child: Pid,
    sigchld: OwnedFd,
    _sigchld_w: OwnedFd,
    clients: Vec<Client>,
    ring: Ring,
    exit_code: Option<i32>,
    kill_deadline: Option<Instant>,
    linger_deadline: Option<Instant>,
}

impl Holder {
    pub fn start(spec: HolderSpec) -> Result<Holder> {
        if let Some(dir) = spec.socket.parent() {
            paths::ensure_private_dir(dir)?;
        }
        paths::ensure_private_dir(&spec.state_dir)?;

        let _ = fs::remove_file(&spec.socket);
        let listener = UnixListener::bind(&spec.socket)
            .with_context(|| format!("bind {}", spec.socket.display()))?;
        fs::set_permissions(&spec.socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        // Install the SIGCHLD handler before the agent exists so no exit is missed.
        let (sigchld, sigchld_w) = cloexec_pipe()?;
        set_nonblocking(sigchld.as_raw_fd());
        set_nonblocking(sigchld_w.as_raw_fd());
        SIGCHLD_PIPE.store(sigchld_w.as_raw_fd(), Ordering::Relaxed);
        // SAFETY: the handler only calls write(2).
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_sigchld as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
        }

        let spawned = match spawn::spawn_agent(&spec) {
            Ok(s) => s,
            Err(e) => {
                let _ = fs::remove_file(&spec.socket);
                return Err(e);
            }
        };
        set_nonblocking(spawned.master.as_raw_fd());

        Ok(Holder {
            spec,
            listener,
            master: Some(spawned.master),
            child: spawned.pid,
            sigchld,
            _sigchld_w: sigchld_w,
            clients: Vec::new(),
            ring: Ring::new(),
            exit_code: None,
            kill_deadline: None,
            linger_deadline: None,
        })
    }

    pub fn agent_pid(&self) -> u32 {
        self.child.as_raw() as u32
    }

    pub fn run(mut self) -> ! {
        loop {
            if self.should_exit() {
                break;
            }
            self.poll_once();
        }
        let _ = fs::remove_file(&self.spec.socket);
        std::process::exit(0)
    }

    fn should_exit(&self) -> bool {
        self.exit_code.is_some()
            && (self.clients.is_empty() || self.linger_deadline.is_some_and(|d| Instant::now() >= d))
    }

    fn poll_once(&mut self) {
        let master_fd = self.master.as_ref().map_or(-1, |m| m.as_raw_fd());
        let mut fds = vec![
            pollfd(self.sigchld.as_raw_fd(), libc::POLLIN),
            pollfd(self.listener.as_raw_fd(), libc::POLLIN),
            pollfd(master_fd, libc::POLLIN),
        ];
        for c in &self.clients {
            let mut events = libc::POLLIN;
            if !c.out.is_empty() {
                events |= libc::POLLOUT;
            }
            fds.push(pollfd(c.stream.as_raw_fd(), events));
        }

        let timeout = self.next_timeout();
        // SAFETY: fds is a valid, correctly sized pollfd array.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                eprintln!("argus-holder: poll: {err}");
            }
            return;
        }

        if fds[0].revents != 0 {
            drain(self.sigchld.as_raw_fd());
            self.reap();
        }

        // Clients first: their input must never wait behind agent output.
        for (i, pfd) in fds[3..].iter().enumerate() {
            if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                self.read_client(i);
            }
            if pfd.revents & libc::POLLOUT != 0 {
                self.clients[i].flush();
            }
        }

        if fds[2].revents != 0 {
            self.read_master();
        }

        if fds[1].revents != 0 {
            self.accept();
        }

        self.clients.retain(|c| !c.dead);
        self.run_timers();
    }

    fn next_timeout(&self) -> i32 {
        let deadline = [self.kill_deadline, self.linger_deadline].into_iter().flatten().min();
        match deadline {
            None => -1,
            Some(d) => d.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32,
        }
    }

    fn run_timers(&mut self) {
        if let Some(d) = self.kill_deadline
            && Instant::now() >= d
        {
            self.kill_deadline = None;
            if self.exit_code.is_none() {
                self.signal_agent(libc::SIGKILL);
            }
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        self.clients.push(Client::new(stream));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("argus-holder: accept: {e}");
                    break;
                }
            }
        }
    }

    fn read_client(&mut self, i: usize) {
        let frames = self.clients[i].read_frames();
        for (t, payload) in frames {
            self.handle_frame(i, t, &payload);
        }
    }

    fn handle_frame(&mut self, i: usize, t: u8, payload: &[u8]) {
        if t != ty::CONTROL {
            // Stream frames belong to attach (M1).
            return;
        }
        let response = match serde_json::from_slice::<HolderRequest>(payload) {
            Err(e) => HolderResponse::Error { message: format!("bad request: {e}") },
            Ok(HolderRequest::Hello { .. }) => {
                HolderResponse::Hello { version: PROTOCOL_VERSION, pid: std::process::id() }
            }
            Ok(HolderRequest::Info) => HolderResponse::Info(HolderInfo {
                id: self.spec.id,
                holder_pid: std::process::id(),
                agent_pid: self.agent_pid(),
                running: self.exit_code.is_none(),
                exit_code: self.exit_code,
                output_offset: self.ring.end,
            }),
            Ok(HolderRequest::Subscribe { level }) => {
                self.clients[i].role = match level {
                    SubscribeLevel::Events => Role::Events,
                    SubscribeLevel::Output => Role::Output,
                };
                self.clients[i].push(frame::encode_json(&HolderResponse::Ok), None);
                if let Some(code) = self.exit_code {
                    self.clients[i].push(frame::encode(ty::EXIT, &code.to_be_bytes()), None);
                }
                return;
            }
            Ok(HolderRequest::Signal { signal }) => {
                if self.exit_code.is_some() {
                    HolderResponse::Error { message: "agent has already exited".into() }
                } else {
                    self.signal_agent(signal);
                    if signal == libc::SIGTERM {
                        self.kill_deadline = Some(Instant::now() + KILL_GRACE);
                    }
                    HolderResponse::Ok
                }
            }
        };
        self.clients[i].push(frame::encode_json(&response), None);
    }

    fn signal_agent(&self, signal: i32) {
        // The agent is a session leader, so its pid is also its process group.
        // SAFETY: kill(2) with a negative pid signals that process group.
        unsafe { libc::kill(-self.child.as_raw(), signal) };
    }

    fn read_master(&mut self) {
        let Some(master) = &self.master else { return };
        let fd = master.as_raw_fd();
        let mut buf = vec![0u8; READ_CHUNK];
        let mut filled = 0;
        let mut closed = false;
        while filled < READ_CHUNK {
            // SAFETY: reading into the unfilled tail of buf.
            let n = unsafe { libc::read(fd, buf[filled..].as_mut_ptr().cast(), READ_CHUNK - filled) };
            if n > 0 {
                filled += n as usize;
                continue;
            }
            if n < 0 {
                match io::Error::last_os_error().kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    _ => {} // EIO: every slave fd is closed.
                }
            }
            closed = true;
            break;
        }
        if filled > 0 {
            self.publish(&buf[..filled]);
        }
        if closed {
            self.master = None;
        }
    }

    fn publish(&mut self, bytes: &[u8]) {
        let start = self.ring.end;
        self.ring.push(bytes);
        if !self.clients.iter().any(|c| c.role == Role::Output) {
            return;
        }
        let mut payload = Vec::with_capacity(8 + bytes.len());
        payload.extend_from_slice(&start.to_be_bytes());
        payload.extend_from_slice(bytes);
        let data = frame::encode(ty::DATA, &payload);
        for c in self.clients.iter_mut().filter(|c| c.role == Role::Output) {
            c.push(data.clone(), Some((start, self.ring.end)));
        }
    }

    fn reap(&mut self) {
        if self.exit_code.is_some() {
            return;
        }
        loop {
            match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => return self.on_exit(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => return self.on_exit(128 + sig as i32),
                Ok(WaitStatus::StillAlive) => return,
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    eprintln!("argus-holder: waitpid: {e}");
                    return self.on_exit(255);
                }
            }
        }
    }

    fn on_exit(&mut self, code: i32) {
        // Collect whatever the agent wrote right before exiting.
        while self.master.is_some() {
            let before = self.ring.end;
            self.read_master();
            if self.ring.end == before {
                break;
            }
        }
        self.master = None;
        self.exit_code = Some(code);
        self.kill_deadline = None;
        self.linger_deadline = Some(Instant::now() + LINGER);

        let dir = &self.spec.state_dir;
        let record = ExitRecord { code, exited_at: now_secs() };
        if let Err(e) = fs::write(paths::exit_record(dir), serde_json::to_vec(&record).unwrap()) {
            eprintln!("argus-holder: writing exit record: {e}");
        }
        if let Err(e) = fs::write(paths::output_log(dir), self.ring.contents()) {
            eprintln!("argus-holder: writing output log: {e}");
        }

        let exit = frame::encode(ty::EXIT, &code.to_be_bytes());
        for c in self.clients.iter_mut().filter(|c| c.role != Role::New) {
            c.push(exit.clone(), None);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Control requests only (e.g. `Signal` from `argus kill`).
    New,
    Events,
    Output,
}

struct Queued {
    bytes: Vec<u8>,
    /// Output offsets carried by this frame; such frames may be dropped.
    data: Option<(u64, u64)>,
}

struct Client {
    stream: UnixStream,
    inbuf: Vec<u8>,
    out: VecDeque<Queued>,
    out_pos: usize,
    queued: usize,
    role: Role,
    dead: bool,
}

impl Client {
    fn new(stream: UnixStream) -> Client {
        Client {
            stream,
            inbuf: Vec::new(),
            out: VecDeque::new(),
            out_pos: 0,
            queued: 0,
            role: Role::New,
            dead: false,
        }
    }

    /// Queues a frame. Output frames that would push the backlog past the
    /// limit replace the droppable backlog with a single `Skipped` frame.
    fn push(&mut self, bytes: Vec<u8>, data: Option<(u64, u64)>) {
        if let Some((start, end)) = data
            && self.queued + bytes.len() > QUEUE_LIMIT
        {
            let mut from = None;
            let keep_front = usize::from(self.out_pos > 0);
            let mut kept = VecDeque::new();
            for (idx, q) in self.out.drain(..).enumerate() {
                match q.data {
                    Some((start, _)) if idx >= keep_front => {
                        from.get_or_insert(start);
                    }
                    _ => kept.push_back(q),
                }
            }
            self.out = kept;
            self.queued = self.out.iter().map(|q| q.bytes.len()).sum::<usize>() - self.out_pos;
            let from = from.unwrap_or(start);
            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&from.to_be_bytes());
            payload.extend_from_slice(&end.to_be_bytes());
            let skipped = frame::encode(ty::SKIPPED, &payload);
            self.queued += skipped.len();
            self.out.push_back(Queued { bytes: skipped, data: None });
        } else {
            self.queued += bytes.len();
            self.out.push_back(Queued { bytes, data });
        }
        self.flush();
    }

    fn flush(&mut self) {
        while let Some(front) = self.out.front() {
            match self.stream.write(&front.bytes[self.out_pos..]) {
                Ok(n) => {
                    self.out_pos += n;
                    self.queued -= n;
                    if self.out_pos == front.bytes.len() {
                        self.out.pop_front();
                        self.out_pos = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
    }

    fn read_frames(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut tmp = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    self.dead = true;
                    break;
                }
                Ok(n) => self.inbuf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
            if self.inbuf.len() > MAX_FRAME + 5 {
                break;
            }
        }

        let mut frames = Vec::new();
        let mut used = 0;
        loop {
            match frame::try_decode(&self.inbuf[used..]) {
                Ok(Some((t, payload, n))) => {
                    frames.push((t, payload.to_vec()));
                    used += n;
                }
                Ok(None) => break,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
        self.inbuf.drain(..used);
        frames
    }
}

/// Recent output, addressed by a monotonically increasing byte offset.
struct Ring {
    buf: VecDeque<u8>,
    /// Offset one past the last byte ever written.
    end: u64,
}

impl Ring {
    fn new() -> Ring {
        Ring { buf: VecDeque::with_capacity(RING_INITIAL), end: 0 }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.end += bytes.len() as u64;
        let bytes = &bytes[bytes.len().saturating_sub(RING_MAX)..];
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(RING_MAX);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

fn pollfd(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd { fd, events, revents: 0 }
}

fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reading into a local buffer from a non-blocking pipe.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_tail_and_offsets() {
        let mut ring = Ring::new();
        ring.push(&vec![b'a'; RING_MAX]);
        ring.push(b"xyz");
        assert_eq!(ring.end, RING_MAX as u64 + 3);
        let contents = ring.contents();
        assert_eq!(contents.len(), RING_MAX);
        assert!(contents.ends_with(b"axyz"));
    }
}
