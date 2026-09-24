//! The holder's single-threaded poll loop.
//!
//! Rules from the design:
//! - client input is handled before PTY output in every round;
//! - at most 64 KiB of output is read per round, coalesced without timers;
//! - the PTY master is never left unread because of a slow client: a client
//!   whose backlog exceeds 1 MiB gets `Skipped` instead (see [`Conn`]);
//! - the most recently active attached client owns the PTY size, with
//!   size changes at most every 300 ms.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{
    AttachRequest, ExitRecord, HolderEvent, HolderInfo, HolderRequest, HolderResponse, HolderSpec, SubscribeLevel,
    now_secs,
};
use argus_proto::{PROTOCOL_VERSION, paths};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::conn::{Conn, Role};
use crate::ring::Ring;
use crate::spawn::{self, cloexec_pipe, set_nonblocking};

const READ_CHUNK: usize = 64 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(5);
const LINGER: Duration = Duration::from_secs(60);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(300);

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
    /// Input waiting for the agent to read it.
    master_out: Vec<u8>,
    child: Pid,
    sigchld: OwnedFd,
    _sigchld_w: OwnedFd,
    conns: Vec<Conn>,
    next_conn_id: u64,
    ring: Ring,
    exit_code: Option<i32>,
    kill_deadline: Option<Instant>,
    linger_deadline: Option<Instant>,
    size: Size,
    attached_reported: u32,
}

/// PTY size and who decides it.
struct Size {
    current: (u16, u16),
    /// Attached connection that owns the size.
    owner: Option<u64>,
    /// Owners in the order they became active, most recent last.
    history: Vec<u64>,
    last_change: Option<Instant>,
    /// A size change held back by the debounce window.
    pending: Option<Instant>,
}

impl Holder {
    pub fn start(spec: HolderSpec) -> Result<Holder> {
        if let Some(dir) = spec.socket.parent() {
            paths::ensure_private_dir(dir)?;
        }
        paths::ensure_private_dir(&spec.state_dir)?;

        let _ = fs::remove_file(&spec.socket);
        let listener = UnixListener::bind(&spec.socket).with_context(|| format!("bind {}", spec.socket.display()))?;
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
        let current = (spec.rows.max(1), spec.cols.max(1));

        Ok(Holder {
            spec,
            listener,
            master: Some(spawned.master),
            master_out: Vec::new(),
            child: spawned.pid,
            sigchld,
            _sigchld_w: sigchld_w,
            conns: Vec::new(),
            next_conn_id: 1,
            ring: Ring::new(),
            exit_code: None,
            kill_deadline: None,
            linger_deadline: None,
            size: Size { current, owner: None, history: Vec::new(), last_change: None, pending: None },
            attached_reported: 0,
        })
    }

    pub fn agent_pid(&self) -> u32 {
        self.child.as_raw() as u32
    }

    pub fn run(mut self) -> ! {
        while !self.should_exit() {
            self.poll_once();
        }
        let _ = fs::remove_file(&self.spec.socket);
        std::process::exit(0)
    }

    fn should_exit(&self) -> bool {
        self.exit_code.is_some() && (self.conns.is_empty() || self.linger_deadline.is_some_and(|d| Instant::now() >= d))
    }

    fn poll_once(&mut self) {
        let master_fd = self.master.as_ref().map_or(-1, |m| m.as_raw_fd());
        let mut master_events = libc::POLLIN;
        if !self.master_out.is_empty() {
            master_events |= libc::POLLOUT;
        }
        let mut fds = vec![
            pollfd(self.sigchld.as_raw_fd(), libc::POLLIN),
            pollfd(self.listener.as_raw_fd(), libc::POLLIN),
            pollfd(master_fd, master_events),
        ];
        for c in &self.conns {
            let mut events = libc::POLLIN;
            if c.has_pending() {
                events |= libc::POLLOUT;
            }
            fds.push(pollfd(c.stream.as_raw_fd(), events));
        }

        // SAFETY: fds is a valid, correctly sized pollfd array.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, self.next_timeout()) };
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
                for (t, payload) in self.conns[i].read_frames() {
                    self.handle_frame(i, t, &payload);
                }
            }
            if pfd.revents & libc::POLLOUT != 0 {
                self.conns[i].flush();
            }
        }

        if fds[2].revents & libc::POLLOUT != 0 {
            self.write_master();
        }
        if fds[2].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            self.read_master();
        }

        if fds[1].revents != 0 {
            self.accept();
        }

        self.remove_dead();
        self.run_timers();
    }

    fn next_timeout(&self) -> i32 {
        let deadline = [self.kill_deadline, self.linger_deadline, self.size.pending].into_iter().flatten().min();
        match deadline {
            None => -1,
            Some(d) => d.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32,
        }
    }

    fn run_timers(&mut self) {
        let now = Instant::now();
        if self.kill_deadline.is_some_and(|d| now >= d) {
            self.kill_deadline = None;
            if self.exit_code.is_none() {
                self.signal_agent(libc::SIGKILL);
            }
        }
        if self.size.pending.is_some_and(|d| now >= d) {
            self.size.pending = None;
            self.apply_owner_size();
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        self.conns.push(Conn::new(self.next_conn_id, stream));
                        self.next_conn_id += 1;
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

    fn remove_dead(&mut self) {
        let before = self.conns.len();
        let dead: Vec<u64> = self.conns.iter().filter(|c| c.dead).map(|c| c.id).collect();
        self.conns.retain(|c| !c.dead);
        if self.conns.len() == before {
            return;
        }
        self.size.history.retain(|id| !dead.contains(id));
        if self.size.owner.is_some_and(|o| dead.contains(&o)) {
            // Hand the size to whoever was active before.
            self.size.owner = self.size.history.last().copied();
            self.request_resize();
        }
        self.report_attached();
    }

    fn handle_frame(&mut self, i: usize, t: u8, payload: &[u8]) {
        let role = self.conns[i].role;
        match (t, role) {
            (ty::CONTROL, _) => self.handle_control(i, payload),
            (ty::DATA, Role::Attach { readonly: false }) => {
                if self.exit_code.is_none() {
                    self.master_out.extend_from_slice(payload);
                    self.write_master();
                }
                self.claim_size(i);
            }
            (ty::RESIZE, Role::Attach { .. }) if payload.len() == 4 => {
                let rows = u16::from_be_bytes([payload[0], payload[1]]);
                let cols = u16::from_be_bytes([payload[2], payload[3]]);
                self.conns[i].size = (rows.max(1), cols.max(1));
                if self.size.owner == Some(self.conns[i].id) {
                    self.request_resize();
                }
            }
            (ty::FOCUS, Role::Attach { .. }) => self.claim_size(i),
            (ty::DETACH, Role::Attach { .. }) => self.conns[i].dead = true,
            _ => {}
        }
    }

    fn handle_control(&mut self, i: usize, payload: &[u8]) {
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
                attached: self.attached_count(),
            }),
            Ok(HolderRequest::Subscribe { level, from_offset }) => {
                self.conns[i].role = match level {
                    SubscribeLevel::Events => Role::Events,
                    SubscribeLevel::Output => Role::Output,
                };
                self.conns[i].push(frame::encode_json(&HolderResponse::Ok));
                let count = self.attached_count();
                self.conns[i].push(frame::encode_json(&HolderEvent::Attached { count }));
                if let (SubscribeLevel::Output, Some(offset)) = (level, from_offset) {
                    let (start, bytes) = self.ring.since(offset);
                    let mut at = start;
                    for chunk in bytes.chunks(READ_CHUNK) {
                        let mut payload = Vec::with_capacity(8 + chunk.len());
                        payload.extend_from_slice(&at.to_be_bytes());
                        payload.extend_from_slice(chunk);
                        self.conns[i].push(frame::encode(ty::DATA, &payload));
                        at += chunk.len() as u64;
                    }
                }
                if let Some(code) = self.exit_code {
                    self.conns[i].push(frame::encode(ty::EXIT, &code.to_be_bytes()));
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
            Ok(HolderRequest::Write { text }) => {
                if self.exit_code.is_some() {
                    HolderResponse::Error { message: "agent has already exited".into() }
                } else {
                    self.master_out.extend_from_slice(text.as_bytes());
                    self.write_master();
                    HolderResponse::Ok
                }
            }
            Ok(HolderRequest::Attach(req)) => return self.attach(i, req),
        };
        self.conns[i].push(frame::encode_json(&response));
    }

    fn attach(&mut self, i: usize, req: AttachRequest) {
        if self.exit_code.is_some() {
            let message = "agent has already exited".to_string();
            return self.conns[i].push(frame::encode_json(&HolderResponse::Error { message }));
        }
        if req.steal {
            let me = self.conns[i].id;
            for c in self.conns.iter_mut().filter(|c| c.role.is_attach() && c.id != me) {
                c.push(frame::encode(ty::KICKED, &[]));
                c.dead = true;
            }
        }
        let conn = &mut self.conns[i];
        conn.role = Role::Attach { readonly: req.readonly };
        conn.size = (req.rows.max(1), req.cols.max(1));
        conn.push(frame::encode_json(&HolderResponse::Ok));
        if req.replay {
            let contents = self.ring.contents();
            for chunk in contents.chunks(READ_CHUNK) {
                conn.push(frame::encode(ty::DATA, chunk));
            }
        }
        if !req.readonly {
            self.make_owner(i);
            // Attaching always redraws: the new terminal starts out blank.
            let size = self.conns[i].size;
            if size == self.size.current {
                self.jiggle();
            } else {
                self.apply_size(size);
            }
        }
        self.report_attached();
    }

    /// Input or focus from an attached client makes it the size owner.
    fn claim_size(&mut self, i: usize) {
        if self.conns[i].role != (Role::Attach { readonly: false }) {
            return;
        }
        if self.size.owner != Some(self.conns[i].id) {
            self.make_owner(i);
            self.request_resize();
        }
    }

    fn make_owner(&mut self, i: usize) {
        let id = self.conns[i].id;
        self.size.owner = Some(id);
        self.size.history.retain(|&h| h != id);
        self.size.history.push(id);
    }

    /// Applies the owner's size now, or when the debounce window ends.
    fn request_resize(&mut self) {
        let Some(last) = self.size.last_change else { return self.apply_owner_size() };
        let ready_at = last + RESIZE_DEBOUNCE;
        if Instant::now() >= ready_at {
            self.apply_owner_size();
        } else {
            self.size.pending = Some(ready_at);
        }
    }

    fn apply_owner_size(&mut self) {
        let Some(owner) = self.size.owner else { return };
        let Some(conn) = self.conns.iter().find(|c| c.id == owner) else { return };
        let size = conn.size;
        if size != self.size.current {
            self.apply_size(size);
        }
    }

    fn apply_size(&mut self, (rows, cols): (u16, u16)) {
        self.set_winsize(rows, cols);
        self.size.current = (rows, cols);
        self.size.last_change = Some(Instant::now());
        self.size.pending = None;
    }

    /// Changes the size and back so the agent gets SIGWINCH and redraws.
    fn jiggle(&mut self) {
        let (rows, cols) = self.size.current;
        self.set_winsize(if rows > 1 { rows - 1 } else { rows + 1 }, cols);
        self.set_winsize(rows, cols);
        self.size.last_change = Some(Instant::now());
    }

    fn set_winsize(&self, rows: u16, cols: u16) {
        let Some(master) = &self.master else { return };
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ on our PTY master; the kernel signals the agent.
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }

    fn attached_count(&self) -> u32 {
        self.conns.iter().filter(|c| c.role.is_attach() && !c.dead).count() as u32
    }

    fn report_attached(&mut self) {
        let count = self.attached_count();
        if count == self.attached_reported {
            return;
        }
        self.attached_reported = count;
        let event = frame::encode_json(&HolderEvent::Attached { count });
        for c in self.conns.iter_mut().filter(|c| c.role.is_subscriber()) {
            c.push(event.clone());
        }
    }

    fn signal_agent(&self, signal: i32) {
        // The agent is a session leader, so its pid is also its process group.
        // SAFETY: kill(2) with a negative pid signals that process group.
        unsafe { libc::kill(-self.child.as_raw(), signal) };
    }

    fn write_master(&mut self) {
        let Some(master) = &self.master else {
            self.master_out.clear();
            return;
        };
        while !self.master_out.is_empty() {
            // SAFETY: writing from an initialized buffer to our PTY master.
            let n = unsafe { libc::write(master.as_raw_fd(), self.master_out.as_ptr().cast(), self.master_out.len()) };
            if n > 0 {
                self.master_out.drain(..n as usize);
                continue;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break; // WouldBlock: POLLOUT will call us again.
            }
        }
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
        let end = self.ring.end;
        if !self.conns.iter().any(|c| c.role.wants_output()) {
            return;
        }
        let raw = frame::encode(ty::DATA, bytes);
        let mut tagged = None;
        let mut owner_skipped = false;
        for c in self.conns.iter_mut() {
            let frame = match c.role {
                Role::Attach { .. } => raw.clone(),
                Role::Output => tagged
                    .get_or_insert_with(|| {
                        let mut payload = Vec::with_capacity(8 + bytes.len());
                        payload.extend_from_slice(&start.to_be_bytes());
                        payload.extend_from_slice(bytes);
                        frame::encode(ty::DATA, &payload)
                    })
                    .clone(),
                _ => continue,
            };
            if c.push_output(frame, start, end) && self.size.owner == Some(c.id) {
                owner_skipped = true;
            }
        }
        // The owner lost frames: have the agent repaint the whole screen.
        if owner_skipped {
            self.jiggle();
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
        self.size.pending = None;
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
        for c in self.conns.iter_mut().filter(|c| c.role != Role::New) {
            c.push(exit.clone());
        }
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
