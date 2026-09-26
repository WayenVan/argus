//! Which terminal modes an agent's output turned on, so that leaving an
//! attachment can turn exactly those off again.
//!
//! [`ModeTracker`] watches every byte on its way to the caller's terminal and
//! records state that outlives the agent's screen. Whole families are handled
//! by one rule each, new members included: DECSET private modes, kitty
//! keyboard stacks (one per screen), XTMODKEYS resources and OSC dynamic
//! colours. [`ModeTracker::undo`] writes the inverse of what is still in
//! effect, back to the terminal's defaults.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// DECSET modes that are on unless something turns them off: autowrap and
/// the visible cursor. Every other mode is off by default.
const DEFAULT_ON: &[u16] = &[7, 25];
/// DECSET modes that are not left to the generic rule: 3 (DECCOLM) resizes the
/// window, 1048 saves or restores the cursor rather than being a mode, the
/// screen switches (47, 1047, 1049) belong to the attach session, and 1004
/// (focus reports) and 2026 (synchronized output) are always reset by it.
const NOT_TRACKED: &[u16] = &[3, 47, 1004, 1047, 1048, 1049, 2026];

/// Longest CSI parameter string worth reading; past it, the sequence is
/// skipped to its final byte unread.
const MAX_CSI: usize = 64;
/// Enough OSC body to read its code and see whether it is a query.
const MAX_OSC: usize = 16;

/// Which screen an attachment draws on: the one the agent is on, so the
/// caller's terminal (and tmux's mouse-wheel binding, which only enters copy
/// mode off the alternate screen) sees what running the agent directly would
/// show. Also the screen the agent's output has the terminal on now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// The agent draws on the normal screen, so its output reaches the
    /// caller's scrollback. Wrapping it in an alternate screen would not
    /// hold anyway: an agent that toggles 1049 itself (omp does on every
    /// resize) drops the terminal out of it at an arbitrary point.
    Normal,
    /// The agent is on its alternate screen. Argus enters one for it, since
    /// the snapshot and the agent's later redraws do not select it again.
    Alternate,
}

/// Kitty keyboard state of one screen, which has its own stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct KittyStack {
    /// Entries the agent pushed and has not popped.
    depth: u32,
    /// The agent set the flags below any entry of its own.
    base_set: bool,
}

impl KittyStack {
    fn is_clean(&self) -> bool {
        self.depth == 0 && !self.base_set
    }

    fn undo(&mut self, out: &mut String) {
        if self.depth > 0 {
            let _ = write!(out, "\x1b[<{}u", self.depth);
        }
        if self.base_set {
            out.push_str("\x1b[=0;1u");
        }
        *self = Self::default();
    }
}

#[derive(Clone, Copy)]
enum State {
    Ground,
    Esc,
    /// Parameter and intermediate bytes collect in the buffer.
    Csi,
    /// The first [`MAX_OSC`] bytes of the body collect in the buffer.
    Osc,
    /// DCS, APC, PM or SOS: skipped up to its terminator.
    Str,
}

pub struct ModeTracker {
    state: State,
    /// The CSI parameters or OSC body read so far, and whether more came than
    /// fit: a CSI that long is left unread.
    buf: [u8; MAX_CSI],
    len: usize,
    overflow: bool,
    screen: Screen,
    /// DECSET modes and the value the agent last gave each.
    decset: BTreeMap<u16, bool>,
    normal_keys: KittyStack,
    alternate_keys: KittyStack,
    /// XTMODKEYS resources (`CSI > Pp ; Pv m`) the agent set.
    modify_keys: BTreeSet<u16>,
    /// OSC 4 and 10–19 colours the agent set; each resets with its code + 100.
    colors: BTreeSet<u16>,
    keypad_application: bool,
    scroll_region: bool,
}

impl ModeTracker {
    /// Starts on `screen`: the one the attach session selected.
    pub fn new(screen: Screen) -> Self {
        Self {
            state: State::Ground,
            buf: [0; MAX_CSI],
            len: 0,
            overflow: false,
            screen,
            decset: BTreeMap::new(),
            normal_keys: KittyStack::default(),
            alternate_keys: KittyStack::default(),
            modify_keys: BTreeSet::new(),
            colors: BTreeSet::new(),
            keypad_application: false,
            scroll_region: false,
        }
    }

    /// Runs of text, parameters and string bodies are scanned for where they
    /// end rather than stepped through byte by byte: this sits on the output
    /// path of every attachment.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            match self.state {
                State::Ground => {
                    let Some(esc) = memchr::memchr(0x1b, rest) else { return };
                    self.state = State::Esc;
                    i += esc + 1;
                    if let Some(&byte) = bytes.get(i) {
                        self.escape(byte);
                        i += 1;
                    }
                }
                State::Esc => {
                    self.escape(rest[0]);
                    i += 1;
                }
                State::Csi => {
                    let end = rest.iter().position(|b| !(0x20..=0x3f).contains(b));
                    self.collect(&rest[..end.unwrap_or(rest.len())], MAX_CSI);
                    let Some(end) = end else { return };
                    i += end + 1;
                    match rest[end] {
                        byte @ 0x40..=0x7e => {
                            if !self.overflow {
                                self.csi(byte);
                            }
                            self.state = State::Ground;
                        }
                        0x1b => self.state = State::Esc,
                        // Other C0 controls act without ending the sequence.
                        0x00..=0x17 | 0x19 | 0x1c..=0x1f => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Osc | State::Str => {
                    let end = rest.iter().position(|b| matches!(b, 0x07 | 0x18 | 0x1a | 0x1b));
                    if matches!(self.state, State::Osc) {
                        self.collect(&rest[..end.unwrap_or(rest.len())], MAX_OSC);
                    }
                    let Some(end) = end else { return };
                    i += end + 1;
                    // BEL, or ST (`ESC \`) or the start of whatever cuts
                    // the string short, ends it; CAN and SUB cancel it.
                    if matches!(self.state, State::Osc) && matches!(rest[end], 0x07 | 0x1b) {
                        self.osc();
                    }
                    self.state = if rest[end] == 0x1b { State::Esc } else { State::Ground };
                }
            }
        }
    }

    fn collect(&mut self, bytes: &[u8], max: usize) {
        let room = max - self.len;
        let take = bytes.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        self.overflow |= bytes.len() > room;
    }

    fn escape(&mut self, byte: u8) {
        self.len = 0;
        self.overflow = false;
        self.state = match byte {
            b'[' => State::Csi,
            b']' => State::Osc,
            b'P' | b'_' | b'^' | b'X' => State::Str,
            0x1b => State::Esc,
            b'=' | b'>' => {
                self.keypad_application = byte == b'=';
                State::Ground
            }
            b'c' => {
                // RIS: the terminal is back to its defaults.
                *self = Self::new(Screen::Normal);
                State::Ground
            }
            _ => State::Ground,
        };
    }

    fn csi(&mut self, action: u8) {
        let params = &self.buf[..self.len];
        let (prefix, rest) = match params.first() {
            Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), &params[1..]),
            // Only a scroll region matters without a prefix; SGR and cursor
            // movement, most of any output, end here.
            _ if action != b'r' => return,
            _ => (None, params),
        };
        if rest.iter().any(|b| !b.is_ascii_digit() && !matches!(b, b';' | b':')) {
            // Intermediates (DECSCUSR, DECRQM, …): nothing tracked.
            return;
        }
        // A `:` sub-field only qualifies its parameter; the first part is the value.
        let numbers = rest
            .split(|&b| b == b';')
            .filter(|_| !rest.is_empty())
            .map(|p| p.split(|&b| b == b':').next().and_then(|n| std::str::from_utf8(n).ok()?.parse::<u16>().ok()));
        let first = numbers.clone().next().flatten();
        let count = numbers.clone().count();
        match (prefix, action) {
            (Some(b'?'), b'h' | b'l') => {
                let modes: Vec<u16> = numbers.flatten().collect();
                for mode in modes {
                    self.decset(mode, action == b'h');
                }
            }
            (Some(b'>'), b'u') => self.keys().depth += 1,
            (Some(b'<'), b'u') => {
                let keys = self.keys();
                keys.depth = keys.depth.saturating_sub(u32::from(first.unwrap_or(1).max(1)));
            }
            (Some(b'='), b'u') => {
                let keys = self.keys();
                if keys.depth == 0 {
                    keys.base_set = true;
                }
            }
            (Some(b'>'), b'm') => {
                let resource = first.unwrap_or(0);
                if count > 1 {
                    self.modify_keys.insert(resource);
                } else {
                    self.modify_keys.remove(&resource);
                }
            }
            (None, b'r') => self.scroll_region = count > 0,
            _ => {}
        }
    }

    fn decset(&mut self, mode: u16, on: bool) {
        if matches!(mode, 47 | 1047 | 1049) {
            self.screen = if on { Screen::Alternate } else { Screen::Normal };
        }
        if !NOT_TRACKED.contains(&mode) {
            self.decset.insert(mode, on);
        }
    }

    fn osc(&mut self) {
        let mut fields = self.buf[..self.len].split(|&b| b == b';');
        let Some(code) = fields.next().and_then(|c| std::str::from_utf8(c).ok()?.parse::<u16>().ok()) else {
            return;
        };
        // A `?` asks for the value instead of setting it.
        if (code == 4 || (10..=19).contains(&code)) && !fields.any(|f| f == b"?") {
            self.colors.insert(code);
        }
    }

    fn keys(&mut self) -> &mut KittyStack {
        match self.screen {
            Screen::Normal => &mut self.normal_keys,
            Screen::Alternate => &mut self.alternate_keys,
        }
    }

    /// The screen the agent's output last selected.
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// Everything the agent changed back to the terminal's defaults, except
    /// the screen itself and keyboard stacks of the other screen: those are
    /// for the caller, which knows how it is leaving (see [`Self::undo_keys`]).
    pub fn undo(&mut self) -> String {
        let mut out = String::new();
        self.keys().undo(&mut out);
        for (&mode, &on) in &self.decset {
            let default = DEFAULT_ON.contains(&mode);
            if on != default {
                let _ = write!(out, "\x1b[?{mode}{}", if default { 'h' } else { 'l' });
            }
        }
        for resource in &self.modify_keys {
            let _ = write!(out, "\x1b[>{resource}m");
        }
        for code in &self.colors {
            let _ = write!(out, "\x1b]{}\x1b\\", code + 100);
        }
        if self.keypad_application {
            out.push_str("\x1b>");
        }
        if self.scroll_region {
            out.push_str("\x1b[r");
        }
        self.decset.clear();
        self.modify_keys.clear();
        self.colors.clear();
        self.keypad_application = false;
        self.scroll_region = false;
        out
    }

    /// Empties what the agent left on `screen`'s keyboard stack. Only valid
    /// while the terminal is on that screen.
    pub fn undo_keys(&mut self, screen: Screen) -> String {
        let mut out = String::new();
        match screen {
            Screen::Normal => self.normal_keys.undo(&mut out),
            Screen::Alternate => self.alternate_keys.undo(&mut out),
        }
        out
    }

    /// Whether `screen`'s keyboard stack still holds anything of the agent's.
    pub fn keys_dirty(&self, screen: Screen) -> bool {
        match screen {
            Screen::Normal => !self.normal_keys.is_clean(),
            Screen::Alternate => !self.alternate_keys.is_clean(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ModeTracker, Screen};

    fn undo(screen: Screen, output: &[&[u8]]) -> (String, ModeTracker) {
        let mut tracker = ModeTracker::new(screen);
        for chunk in output {
            tracker.feed(chunk);
        }
        (tracker.undo(), tracker)
    }

    #[test]
    fn codex_pushes_keyboard_flags_on_both_screens() {
        let (out, mut tracker) = undo(Screen::Normal, &[b"\x1b[>7u\x1b[?1049h\x1b[>7u\x1b[?2004h"]);
        assert_eq!(tracker.screen(), Screen::Alternate);
        assert_eq!(out, "\x1b[<1u\x1b[?2004l");
        assert!(tracker.keys_dirty(Screen::Normal));
        assert_eq!(tracker.undo_keys(Screen::Normal), "\x1b[<1u");
        assert!(!tracker.keys_dirty(Screen::Normal));
    }

    #[test]
    fn keyboard_stack_counts_pushes_and_pops() {
        let (out, _) = undo(Screen::Normal, &[b"\x1b[>1u\x1b[>3u\x1b[>7u\x1b[<2u"]);
        assert_eq!(out, "\x1b[<1u");
        let (out, _) = undo(Screen::Normal, &[b"\x1b[>1u\x1b[<u"]);
        assert_eq!(out, "");
        let (out, _) = undo(Screen::Normal, &[b"\x1b[=5;1u"]);
        assert_eq!(out, "\x1b[=0;1u");
        let (out, _) = undo(Screen::Normal, &[b"\x1b[?u"]);
        assert_eq!(out, "", "a query changes nothing");
    }

    #[test]
    fn decset_modes_return_to_their_defaults() {
        let (out, _) = undo(Screen::Normal, &[b"\x1b[?1000;1006h\x1b[?25l\x1b[?7l\x1b[?1007l\x1b[?9999h"]);
        assert_eq!(out, "\x1b[?7h\x1b[?25h\x1b[?1000l\x1b[?1006l\x1b[?9999l");
        let (out, _) = undo(Screen::Normal, &[b"\x1b[?2004h\x1b[?2004l\x1b[?25l\x1b[?25h"]);
        assert_eq!(out, "", "already back to default");
    }

    #[test]
    fn screen_switches_and_session_modes_are_left_to_the_session() {
        let (out, tracker) = undo(Screen::Normal, &[b"\x1b[?1049h\x1b[?1004h\x1b[?2026h\x1b[?1048h\x1b[?3h"]);
        assert_eq!(out, "");
        assert_eq!(tracker.screen(), Screen::Alternate);
    }

    #[test]
    fn other_stateful_sequences() {
        let (out, _) = undo(Screen::Normal, &[b"\x1b[>4;2m\x1b[>1;2m\x1b[>1m\x1b=\x1b[5;20r"]);
        assert_eq!(out, "\x1b[>4m\x1b>\x1b[r");
        let (out, _) = undo(Screen::Normal, &[b"\x1b]11;?\x1b\\\x1b]12;#ff0000\x07\x1b]4;1;?\x07\x1b]4;1;#000\x1b\\"]);
        assert_eq!(out, "\x1b]104\x1b\\\x1b]112\x1b\\");
    }

    #[test]
    fn sequences_split_across_reads() {
        let (out, _) = undo(Screen::Normal, &[b"text\x1b", b"[?10", b"00h more", b" \x1b]1", b"1;#000\x07"]);
        assert_eq!(out, "\x1b[?1000l\x1b]111\x1b\\");
    }

    #[test]
    fn strings_and_intermediates_hide_lookalikes() {
        let (out, _) = undo(Screen::Normal, &[b"\x1bPq[?1000h\x1b\\\x1b]0;\x07\x1b[?2004$p\x1b[2 q"]);
        assert_eq!(out, "");
    }

    #[test]
    fn full_reset_forgets_everything() {
        let (out, tracker) = undo(Screen::Alternate, &[b"\x1b[>7u\x1b[?1000h\x1bc"]);
        assert_eq!(out, "");
        assert_eq!(tracker.screen(), Screen::Normal);
    }
}
