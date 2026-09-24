//! One connection to the holder, with a bounded outgoing queue.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use argus_proto::frame::{self, MAX_FRAME, ty};

/// Backlog above which output is dropped in favour of a `Skipped` frame.
const QUEUE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Control requests only (e.g. `Signal` from `argus kill`).
    New,
    /// Manager subscription: lifecycle events.
    Events,
    /// Manager subscription: events plus offset-tagged output.
    Output,
    /// An attached terminal in stream mode.
    Attach { readonly: bool },
}

impl Role {
    pub fn is_subscriber(self) -> bool {
        matches!(self, Role::Events | Role::Output)
    }

    pub fn is_attach(self) -> bool {
        matches!(self, Role::Attach { .. })
    }

    pub fn wants_output(self) -> bool {
        matches!(self, Role::Output | Role::Attach { .. })
    }
}

struct Queued {
    bytes: Vec<u8>,
    /// Output offsets carried by this frame; such frames may be dropped.
    data: Option<(u64, u64)>,
}

pub struct Conn {
    /// Unique for the holder's lifetime; indices shift, ids do not.
    pub id: u64,
    pub stream: UnixStream,
    pub role: Role,
    /// Terminal size reported by an attached client.
    pub size: (u16, u16),
    /// Whether this attached terminal reported having focus.
    pub focused: bool,
    pub dead: bool,
    inbuf: Vec<u8>,
    out: VecDeque<Queued>,
    out_pos: usize,
    queued: usize,
}

impl Conn {
    pub fn new(id: u64, stream: UnixStream) -> Conn {
        Conn {
            id,
            stream,
            role: Role::New,
            size: (0, 0),
            focused: false,
            dead: false,
            inbuf: Vec::new(),
            out: VecDeque::new(),
            out_pos: 0,
            queued: 0,
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.out.is_empty()
    }

    pub fn push(&mut self, bytes: Vec<u8>) {
        self.queued += bytes.len();
        self.out.push_back(Queued { bytes, data: None });
        self.flush();
    }

    /// Queues output covering offsets `start..end`. If that would push the
    /// backlog past the limit, the droppable backlog and this frame are
    /// replaced by one `Skipped` frame. Returns true when output was skipped.
    pub fn push_output(&mut self, bytes: Vec<u8>, start: u64, end: u64) -> bool {
        if self.queued + bytes.len() <= QUEUE_LIMIT {
            self.queued += bytes.len();
            self.out.push_back(Queued { bytes, data: Some((start, end)) });
            self.flush();
            return false;
        }

        // A partially written frame must be finished or the stream breaks.
        let keep_front = usize::from(self.out_pos > 0);
        let mut from = start;
        let mut kept = VecDeque::with_capacity(self.out.len());
        for (idx, q) in self.out.drain(..).enumerate() {
            match q.data {
                Some((s, _)) if idx >= keep_front => from = from.min(s),
                _ => kept.push_back(q),
            }
        }
        self.out = kept;
        self.queued = self.out.iter().map(|q| q.bytes.len()).sum::<usize>() - self.out_pos;

        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&from.to_be_bytes());
        payload.extend_from_slice(&end.to_be_bytes());
        self.push(frame::encode(ty::SKIPPED, &payload));
        true
    }

    pub fn flush(&mut self) {
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

    /// Reads what is available and returns every complete frame.
    pub fn read_frames(&mut self) -> Vec<(u8, Vec<u8>)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Conn, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        (Conn::new(1, a), b)
    }

    #[test]
    fn backlog_turns_into_skipped() {
        let (mut conn, peer) = pair();
        let chunk = frame::encode(ty::DATA, &vec![b'x'; 60 * 1024]);
        let mut offset = 0;
        let mut skipped = false;
        // Nobody reads `peer`, so the kernel buffer fills and the queue grows.
        for _ in 0..64 {
            let end = offset + 60 * 1024;
            skipped |= conn.push_output(chunk.clone(), offset, end);
            offset = end;
        }
        assert!(skipped);
        assert!(conn.queued <= QUEUE_LIMIT + chunk.len());
        drop(peer);
    }
}
