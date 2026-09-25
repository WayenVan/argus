//! The holder's single-threaded poll loop.
//!
//! Rules from the design:
//! - client input is handled before PTY output in every round;
//! - at most 64 KiB of output is read per round;
//! - output goes out at once after a quiet spell, and while it keeps coming
//!   it is batched for up to [`COALESCE`] or 64 KiB: PTYs hand it over in
//!   small reads (a few hundred bytes on macOS), and forwarding each one
//!   costs every downstream hop a wakeup;
//! - the PTY master is never left unread because of a slow client: a client
//!   whose backlog exceeds 1 MiB gets `Skipped` instead (see [`Conn`]);
//! - the most recently active attached client owns the PTY size, with
//!   size changes at most every 300 ms.

use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{
    AttachRequest, ExitRecord, HOLDER_CAPABILITIES, HolderEvent, HolderInfo, HolderRequest, HolderResponse, HolderSpec,
    SubscribeLevel, now_secs,
};
use argus_proto::{HOLDER_PROTOCOL_VERSION, paths};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use signal_hook::SigId;
use signal_hook::consts::SIGCHLD;

use crate::conn::{Conn, Role};
use crate::ring::Ring;
use crate::spawn::{self, cloexec_pipe, set_nonblocking};
use crate::stand_in::StandIn;

const READ_CHUNK: usize = 64 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(5);
const LINGER: Duration = Duration::from_secs(60);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(300);
const INPUT_EVENT_INTERVAL: Duration = Duration::from_secs(1);
/// How long output that keeps coming is held to be sent in one batch.
const COALESCE: Duration = Duration::from_millis(2);

pub struct Holder {
    spec: HolderSpec,
    listener: UnixListener,
    master: Option<OwnedFd>,
    /// Input waiting for the agent to read it.
    master_out: Vec<u8>,
    child: Pid,
    sigchld: OwnedFd,
    _sigchld_registration: SigId,
    conns: Vec<Conn>,
    next_conn_id: u64,
    ring: Ring,
    exit_code: Option<i32>,
    kill_deadline: Option<Instant>,
    linger_deadline: Option<Instant>,
    size: Size,
    /// Last attachment snapshot sent to subscribers.
    attached_reported: (u32, u32, Vec<argus_proto::msg::TmuxLocation>),
    last_input_event: Option<Instant>,
    stand_in: StandIn,
    /// Output read but not yet published.
    pending: Vec<u8>,
    /// Until when output is batched; `None` once a batch window passed quietly.
    hold_until: Option<Instant>,
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
        let sigchld_registration = signal_hook::low_level::pipe::register(SIGCHLD, sigchld_w)?;

        let spawned = match spawn::spawn_agent(&spec) {
            Ok(s) => s,
            Err(e) => {
                let _ = fs::remove_file(&spec.socket);
                return Err(e);
            }
        };
        set_nonblocking(spawned.master.as_raw_fd());
        let current = (spec.rows.max(1), spec.cols.max(1));
        let stand_in = StandIn::new(spec.colors.clone());

        Ok(Holder {
            spec,
            listener,
            master: Some(spawned.master),
            master_out: Vec::new(),
            child: spawned.pid,
            sigchld,
            _sigchld_registration: sigchld_registration,
            conns: Vec::new(),
            next_conn_id: 1,
            ring: Ring::new(),
            exit_code: None,
            kill_deadline: None,
            linger_deadline: None,
            size: Size { current, owner: None, history: Vec::new(), last_change: None, pending: None },
            attached_reported: (0, 0, Vec::new()),
            last_input_event: None,
            stand_in,
            pending: Vec::with_capacity(READ_CHUNK),
            hold_until: None,
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
        let mut master_events = PollFlags::POLLIN;
        if !self.master_out.is_empty() {
            master_events |= PollFlags::POLLOUT;
        }
        let master_fd = self.master.as_ref().map_or(self.sigchld.as_fd(), AsFd::as_fd);
        let mut fds = vec![
            PollFd::new(self.sigchld.as_fd(), PollFlags::POLLIN),
            PollFd::new(self.listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(master_fd, if self.master.is_some() { master_events } else { PollFlags::empty() }),
        ];
        for c in &self.conns {
            let mut events = PollFlags::POLLIN;
            if c.has_pending() {
                events |= PollFlags::POLLOUT;
            }
            fds.push(PollFd::new(c.stream.as_fd(), events));
        }

        if let Err(err) = poll(&mut fds, self.next_timeout()) {
            if err != nix::errno::Errno::EINTR {
                eprintln!("argus-holder: poll: {err}");
            }
            return;
        }

        let sigchld_ready = !fds[0].revents().unwrap_or_else(PollFlags::empty).is_empty();
        let listener_ready = !fds[1].revents().unwrap_or_else(PollFlags::empty).is_empty();
        let master_ready = fds[2].revents().unwrap_or_else(PollFlags::empty);
        let conn_ready: Vec<PollFlags> =
            fds[3..].iter().map(|fd| fd.revents().unwrap_or_else(PollFlags::empty)).collect();
        drop(fds);

        if sigchld_ready {
            drain(self.sigchld.as_raw_fd());
            self.reap();
        }

        // Clients first: their input must never wait behind agent output.
        for (i, ready) in conn_ready.into_iter().enumerate() {
            if ready.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR) {
                for (t, payload) in self.conns[i].read_frames() {
                    self.handle_frame(i, t, &payload);
                }
            }
            if ready.contains(PollFlags::POLLOUT) {
                self.conns[i].flush();
            }
        }

        if master_ready.contains(PollFlags::POLLOUT) {
            self.write_master();
        }
        if master_ready.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR) {
            self.read_master();
        }

        if listener_ready {
            self.accept();
        }

        self.remove_dead();
        self.run_timers();
    }

    fn next_timeout(&self) -> PollTimeout {
        let hold = self.hold_until.filter(|_| !self.pending.is_empty());
        let deadline = [self.kill_deadline, self.linger_deadline, self.size.pending, hold].into_iter().flatten().min();
        match deadline {
            None => PollTimeout::NONE,
            // Rounded up: poll counts whole milliseconds, and a timeout
            // rounded down to 0 would spin until the deadline.
            Some(d) => {
                let ms = d.saturating_duration_since(Instant::now()).as_micros().div_ceil(1000);
                PollTimeout::try_from(ms).unwrap_or(PollTimeout::MAX)
            }
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
        if self.hold_until.is_some_and(|d| now >= d) {
            // Keep batching while output flows; a quiet window ends it.
            self.hold_until = if self.pending.is_empty() { None } else { Some(now + COALESCE) };
            self.flush_pending();
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
                self.report_input();
            }
            (ty::MOUSE, Role::Attach { readonly: false }) => {
                if self.exit_code.is_none() {
                    self.master_out.extend_from_slice(payload);
                    self.write_master();
                }
            }
            (ty::RESIZE, Role::Attach { .. }) if payload.len() == 4 => {
                let rows = u16::from_be_bytes([payload[0], payload[1]]);
                let cols = u16::from_be_bytes([payload[2], payload[3]]);
                self.conns[i].size = (rows.max(1), cols.max(1));
                if self.size.owner == Some(self.conns[i].id) {
                    self.request_resize();
                }
            }
            (ty::FOCUS, Role::Attach { .. }) => {
                let focused = payload.first() != Some(&0);
                self.conns[i].focused = focused;
                if focused {
                    self.claim_size(i);
                }
                self.report_attached();
            }
            (ty::DETACH, Role::Attach { .. }) => self.conns[i].dead = true,
            _ => {}
        }
    }

    fn handle_control(&mut self, i: usize, payload: &[u8]) {
        let response = match serde_json::from_slice::<HolderRequest>(payload) {
            Err(e) => HolderResponse::Error { message: format!("bad request: {e}") },
            Ok(HolderRequest::Hello { .. }) => HolderResponse::Hello {
                version: HOLDER_PROTOCOL_VERSION,
                pid: std::process::id(),
                capabilities: HOLDER_CAPABILITIES.to_vec(),
            },
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
                let (count, focused, tmux_locations) = self.attached_state();
                self.conns[i].push(frame::encode_json(&HolderEvent::Attached { count, focused, tmux_locations }));
                if level == SubscribeLevel::Output {
                    let (rows, cols) = self.size.current;
                    self.conns[i].push(frame::encode_json(&HolderEvent::Resized { rows, cols }));
                }
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
        if !req.readonly {
            self.stand_in.learn(req.colors.clone());
        }
        let conn = &mut self.conns[i];
        conn.role = Role::Attach { readonly: req.readonly };
        conn.size = (req.rows.max(1), req.cols.max(1));
        conn.tmux = req.tmux.clone();
        conn.push(frame::encode_json(&HolderResponse::Ok));
        if let Some(offset) = req.from_offset {
            let (_, bytes) = self.ring.since(offset);
            let bytes = argus_proto::ansi::filter_replay(&bytes, req.allow_clipboard_replay);
            for chunk in bytes.chunks(READ_CHUNK) {
                conn.push(frame::encode(ty::DATA, chunk));
            }
        } else if req.replay {
            let contents = self.ring.contents();
            let contents = argus_proto::ansi::filter_replay(&contents, req.allow_clipboard_replay);
            for chunk in contents.chunks(READ_CHUNK) {
                conn.push(frame::encode(ty::DATA, chunk));
            }
        }
        if !req.readonly {
            self.make_owner(i);
            let size = self.conns[i].size;
            if size == self.size.current {
                if needs_jiggle(&req) {
                    self.jiggle();
                }
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
        self.notify_subscribers(&HolderEvent::Resized { rows, cols });
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

    fn writable_attached(&self) -> bool {
        self.conns.iter().any(|c| c.role == Role::Attach { readonly: false } && !c.dead)
    }

    fn attached_count(&self) -> u32 {
        self.attached_counts().0
    }

    /// Attached terminals, and how many of them currently have focus.
    fn attached_counts(&self) -> (u32, u32) {
        let attached = self.conns.iter().filter(|c| c.role.is_attach() && !c.dead);
        let (count, focused) = attached.fold((0, 0), |(n, f), c| (n + 1, f + u32::from(c.focused)));
        (count, focused)
    }

    fn attached_state(&self) -> (u32, u32, Vec<argus_proto::msg::TmuxLocation>) {
        let (count, focused) = self.attached_counts();
        let locations =
            self.conns.iter().filter(|c| c.role.is_attach() && !c.dead).filter_map(|c| c.tmux.clone()).collect();
        (count, focused, locations)
    }

    fn report_attached(&mut self) {
        let state = self.attached_state();
        if state == self.attached_reported {
            return;
        }
        self.attached_reported = state.clone();
        let (count, focused, tmux_locations) = state;
        self.notify_subscribers(&HolderEvent::Attached { count, focused, tmux_locations });
    }

    /// Tells subscribers someone typed, at most once per second.
    fn report_input(&mut self) {
        let now = Instant::now();
        if self.last_input_event.is_some_and(|t| now.duration_since(t) < INPUT_EVENT_INTERVAL) {
            return;
        }
        self.last_input_event = Some(now);
        self.notify_subscribers(&HolderEvent::Input);
    }

    /// Events are facts for whoever is subscribed right now; nothing is queued
    /// for subscribers that are not connected.
    fn notify_subscribers(&mut self, event: &HolderEvent) {
        let frame = frame::encode_json(event);
        for c in self.conns.iter_mut().filter(|c| c.role.is_subscriber()) {
            c.push(frame.clone());
        }
    }

    fn signal_agent(&self, signal: i32) {
        // The agent is a session leader, so its pid is also its process group.
        if let Ok(signal) = Signal::try_from(signal) {
            let _ = killpg(self.child, signal);
        }
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

    /// Reads what the agent wrote, up to 64 KiB, and returns how much.
    fn read_master(&mut self) -> usize {
        let Some(master) = &self.master else { return 0 };
        let fd = master.as_raw_fd();
        // Read straight onto the end of `pending`: no zeroed buffer, no copy.
        let start = self.pending.len();
        self.pending.reserve(READ_CHUNK);
        let mut filled = 0;
        let mut closed = false;
        while filled < READ_CHUNK {
            let len = self.pending.len();
            // SAFETY: `reserve` left at least READ_CHUNK - filled bytes of
            // capacity past `len`; only the bytes `read` wrote become part
            // of the vector.
            let n = unsafe { libc::read(fd, self.pending.as_mut_ptr().add(len).cast(), READ_CHUNK - filled) };
            if n > 0 {
                unsafe { self.pending.set_len(len + n as usize) };
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
            let replies = self.stand_in.answer(&self.pending[start..]);
            let now = Instant::now();
            if self.hold_until.is_none_or(|d| now >= d) {
                self.hold_until = Some(now + COALESCE);
                self.flush_pending();
            } else if self.pending.len() >= READ_CHUNK {
                self.flush_pending();
            }
            // A writable terminal receives the queries with the output and
            // answers them itself, through its input.
            if !replies.is_empty() && !self.writable_attached() {
                self.master_out.extend_from_slice(&replies);
                self.write_master();
            }
        }
        if closed {
            self.master = None;
        }
        filled
    }

    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        self.publish(&pending);
        self.pending = pending;
        self.pending.clear();
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
        while self.master.is_some() && self.read_master() > 0 {}
        self.flush_pending();
        self.hold_until = None;
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

fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reading into a local buffer from a non-blocking pipe.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// Whether an attach at unchanged PTY size still needs the resize-and-back
/// trick to make the agent redraw. Not needed when the client already
/// arrives with a picture — a manager snapshot (`from_offset`) or a raw
/// ring-buffer replay now streaming to it (`replay`) — since jiggling would
/// only flicker on top of it for no benefit.
fn needs_jiggle(req: &AttachRequest) -> bool {
    req.from_offset.is_none() && !req.replay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(from_offset: Option<u64>, replay: bool) -> AttachRequest {
        AttachRequest {
            rows: 24,
            cols: 80,
            readonly: false,
            steal: false,
            replay,
            allow_clipboard_replay: false,
            from_offset,
            colors: Default::default(),
            tmux: None,
        }
    }

    #[test]
    fn jiggles_a_blank_client() {
        assert!(needs_jiggle(&req(None, false)));
    }

    #[test]
    fn skips_the_jiggle_for_a_manager_restored_client() {
        assert!(!needs_jiggle(&req(Some(42), false)));
    }

    #[test]
    fn skips_the_jiggle_for_a_ring_buffer_replay() {
        assert!(!needs_jiggle(&req(None, true)));
    }
}
