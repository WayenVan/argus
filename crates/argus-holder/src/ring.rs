//! Recent output, addressed by a monotonically increasing byte offset.

use std::collections::{BTreeSet, VecDeque};

use vte::{Params, Parser, Perform};

const RING_INITIAL: usize = 256 * 1024;
const RING_MAX: usize = 1024 * 1024;

pub struct Ring {
    buf: VecDeque<u8>,
    /// Terminal mode at the byte just before the retained output begins.
    start_mode: ModeTracker,
    /// Offset one past the last byte ever written.
    pub end: u64,
}

impl Ring {
    pub fn new() -> Ring {
        Ring { buf: VecDeque::with_capacity(RING_INITIAL), start_mode: ModeTracker::default(), end: 0 }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.end += bytes.len() as u64;
        // Advance the checkpoint through precisely the bytes evicted from the
        // ring. The parser retains partial CSI sequences across push calls.
        if bytes.len() >= RING_MAX {
            let old: Vec<u8> = self.buf.drain(..).collect();
            self.start_mode.advance(&old);
            self.start_mode.advance(&bytes[..bytes.len() - RING_MAX]);
            self.buf.extend(&bytes[bytes.len() - RING_MAX..]);
            return;
        }
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(RING_MAX);
        let evicted: Vec<u8> = self.buf.drain(..overflow).collect();
        self.start_mode.advance(&evicted);
        self.buf.extend(bytes);
    }

    pub fn alternate_at_start(&self) -> bool {
        self.start_mode.alternate
    }

    pub fn input_modes_at_start(&self) -> Vec<u16> {
        self.start_mode.input_modes.iter().copied().collect()
    }

    pub fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    /// Bytes from `offset` on, clamped to the oldest byte still held.
    /// Returns the offset the bytes actually start at.
    pub fn since(&self, offset: u64) -> (u64, Vec<u8>) {
        let oldest = self.end - self.buf.len() as u64;
        let start = offset.clamp(oldest, self.end);
        let skip = (start - oldest) as usize;
        (start, self.buf.range(skip..).copied().collect())
    }
}

#[derive(Default)]
struct ModeTracker {
    parser: Parser,
    alternate: bool,
    input_modes: BTreeSet<u16>,
}

impl ModeTracker {
    fn advance(&mut self, bytes: &[u8]) {
        // Track the modes needed to restore an attached TUI. vte keeps escape
        // sequences split across ring evictions in their correct state.
        struct ScreenMode<'a> {
            alternate: &'a mut bool,
            input_modes: &'a mut BTreeSet<u16>,
        }
        impl Perform for ScreenMode<'_> {
            fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
                if ignore || intermediates != b"?" || !matches!(action, 'h' | 'l') {
                    return;
                }
                for param in params.iter() {
                    match param.first().copied() {
                        Some(47 | 1047 | 1049) => *self.alternate = action == 'h',
                        Some(mode @ (1000 | 1002 | 1003 | 1004 | 1006 | 2004)) => {
                            if action == 'h' {
                                self.input_modes.insert(mode);
                            } else {
                                self.input_modes.remove(&mode);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        self.parser
            .advance(&mut ScreenMode { alternate: &mut self.alternate, input_modes: &mut self.input_modes }, bytes);
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

        assert_eq!(ring.since(ring.end - 2), (ring.end - 2, b"yz".to_vec()));
        assert_eq!(ring.since(0).0, 3); // oldest byte still held
        assert_eq!(ring.since(u64::MAX), (ring.end, vec![]));
    }

    #[test]
    fn remembers_alternate_screen_enter_evicted_from_ring() {
        let mut ring = Ring::new();
        ring.push(b"\x1b[?1049h\x1b[?1000;1006h");
        ring.push(&vec![b'x'; RING_MAX]);
        assert!(ring.alternate_at_start());
        assert_eq!(ring.input_modes_at_start(), [1000, 1006]);
        ring.push(b"\x1b[?1049l");
        ring.push(b"\x1b[?1000;1006l");
        ring.push(&vec![b'y'; RING_MAX]);
        assert!(!ring.alternate_at_start());
        assert!(ring.input_modes_at_start().is_empty());
    }

    #[test]
    fn tracks_split_escape_at_eviction_boundary() {
        let mut ring = Ring::new();
        ring.push(&vec![b'x'; RING_MAX - 5]);
        ring.push(b"\x1b[?10");
        ring.push(b"49h");
        ring.push(&vec![b'y'; RING_MAX]);
        assert!(ring.alternate_at_start());
    }
}
