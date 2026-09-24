//! Recent output, addressed by a monotonically increasing byte offset.

use std::collections::VecDeque;

const RING_INITIAL: usize = 256 * 1024;
const RING_MAX: usize = 1024 * 1024;

pub struct Ring {
    buf: VecDeque<u8>,
    /// Offset one past the last byte ever written.
    pub end: u64,
}

impl Ring {
    pub fn new() -> Ring {
        Ring { buf: VecDeque::with_capacity(RING_INITIAL), end: 0 }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.end += bytes.len() as u64;
        let bytes = &bytes[bytes.len().saturating_sub(RING_MAX)..];
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(RING_MAX);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    pub fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_tail_and_offsets() {
        let mut ring = Ring::new();
        ring.push(&vec![b'a'; RING_MAX]);
        ring.push(b"xyz");
        assert_eq!(ring.end, RING_MAX as u64 + 3);
        let contents = ring.contents();
        assert_eq!(contents.len(), RING_MAX);
        assert!(contents.ends_with(b"axyz"));
    }
}
