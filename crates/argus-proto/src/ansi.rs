//! Streaming filters for terminal output with external side effects.

use vte::{Params, Parser, Perform};

/// Replayed history must not act on the attaching terminal again. Two kinds
/// of sequence would: OSC 52 clipboard writes (kept only when the user asks
/// for them), and queries. A query in history was answered when it was live,
/// by the terminal attached then or by the holder standing in for one; the
/// new terminal would answer it again, and its reply would reach the agent as
/// typed input. Every other byte, drawing included, passes through unchanged.
pub struct ReplayFilter {
    keep_clipboard: bool,
    state: State,
}

/// A CSI, OSC or DCS whose raw bytes are held until it can be classified;
/// past these lengths it cannot be a query and is passed through.
const MAX_CSI: usize = 64;
const MAX_OSC: usize = 64;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi(Vec<u8>),
    /// `raw` includes the introducer; `body` is what follows it.
    Osc {
        raw: Vec<u8>,
        body: Vec<u8>,
    },
    Dcs(Vec<u8>),
    /// Inside a string being passed through (`copy`) or dropped, up to its
    /// terminator: ST, or also BEL for an OSC.
    String {
        copy: bool,
        bel_ends: bool,
        esc: bool,
    },
}

impl ReplayFilter {
    pub fn new(keep_clipboard: bool) -> Self {
        Self { keep_clipboard, state: State::Ground }
    }

    pub fn write_filtered(&mut self, input: &[u8], output: &mut Vec<u8>) {
        for &byte in input {
            self.write_byte(byte, output);
        }
    }

    fn write_byte(&mut self, byte: u8, output: &mut Vec<u8>) {
        self.state = match std::mem::take(&mut self.state) {
            State::Ground => match byte {
                0x1b => State::Esc,
                0x9d => State::Osc { raw: vec![byte], body: Vec::new() },
                _ => {
                    output.push(byte);
                    State::Ground
                }
            },
            State::Esc => match byte {
                b'[' => State::Csi(vec![0x1b, byte]),
                b']' => State::Osc { raw: vec![0x1b, byte], body: Vec::new() },
                b'P' => State::Dcs(vec![0x1b, byte]),
                0x1b => {
                    output.push(0x1b);
                    State::Esc
                }
                _ => {
                    output.extend_from_slice(&[0x1b, byte]);
                    State::Ground
                }
            },
            State::Csi(mut raw) => {
                raw.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    if !is_csi_query(&raw[2..raw.len() - 1], byte) {
                        output.extend_from_slice(&raw);
                    }
                    State::Ground
                } else if byte == 0x1b || raw.len() >= MAX_CSI {
                    // Not a well-formed CSI; hand it on and start over.
                    raw.pop();
                    output.extend_from_slice(&raw);
                    self.state = State::Ground;
                    return self.write_byte(byte, output);
                } else {
                    State::Csi(raw)
                }
            }
            State::Osc { mut raw, mut body } => {
                raw.push(byte);
                let terminated = byte == 0x07 || byte == 0x9c || raw.ends_with(b"\x1b\\");
                if terminated {
                    if !is_osc_query(&body) && !(body.starts_with(b"52;") && !self.keep_clipboard) {
                        output.extend_from_slice(&raw);
                    }
                    State::Ground
                } else {
                    if byte != 0x1b {
                        body.push(byte);
                    }
                    if body == b"52;" && !self.keep_clipboard {
                        State::String { copy: false, bel_ends: true, esc: false }
                    } else if raw.len() >= MAX_OSC {
                        output.extend_from_slice(&raw);
                        State::String { copy: true, bel_ends: true, esc: byte == 0x1b }
                    } else {
                        State::Osc { raw, body }
                    }
                }
            }
            State::Dcs(mut raw) => {
                raw.push(byte);
                if raw.ends_with(b"\x1b\\") || byte == 0x9c {
                    output.extend_from_slice(&raw);
                    State::Ground
                } else if raw.len() < 4 {
                    State::Dcs(raw)
                } else {
                    // DECRQSS (`$q`) and XTGETTCAP (`+q`) ask; the rest draw.
                    let query = matches!(&raw[2..4], b"$q" | b"+q");
                    if !query {
                        output.extend_from_slice(&raw);
                    }
                    State::String { copy: !query, bel_ends: false, esc: byte == 0x1b }
                }
            }
            State::String { copy, bel_ends, esc } => {
                if copy {
                    output.push(byte);
                }
                if byte == 0x9c || (esc && byte == b'\\') || (bel_ends && byte == 0x07) {
                    State::Ground
                } else {
                    State::String { copy, bel_ends, esc: byte == 0x1b }
                }
            }
        };
    }

    /// Hands on a sequence cut off by the end of the replay: live output
    /// continues it, and that part is live. A dropped string stays dropped.
    pub fn finish(&mut self, output: &mut Vec<u8>) {
        match std::mem::take(&mut self.state) {
            State::Esc => output.push(0x1b),
            State::Csi(raw) | State::Osc { raw, .. } | State::Dcs(raw) => output.extend_from_slice(&raw),
            State::Ground | State::String { .. } => {}
        }
    }
}

/// Whether the CSI with parameters `params` (everything between `[` and the
/// final byte) and final byte `action` asks the terminal for a reply.
fn is_csi_query(params: &[u8], action: u8) -> bool {
    let (prefix, rest) = match params.first() {
        Some(&p @ (b'<' | b'=' | b'>' | b'?')) => (Some(p), &params[1..]),
        _ => (None, params),
    };
    let split = rest.iter().position(|b| (0x20..=0x2f).contains(b)).unwrap_or(rest.len());
    let (numbers, intermediates) = rest.split_at(split);
    let first = std::str::from_utf8(numbers).ok().and_then(|n| n.split(';').next()?.parse::<u32>().ok());
    match (prefix, intermediates, action) {
        // DA1, DA2, DA3.
        (None | Some(b'>' | b'='), [], b'c') => true,
        // DSR: status, cursor position, colour scheme, …
        (None | Some(b'?'), [], b'n') => true,
        // DECRQM.
        (None | Some(b'?'), [b'$'], b'p') => true,
        // Kitty keyboard flags, modifyOtherKeys level, XTVERSION.
        (Some(b'?'), [], b'u') => numbers.is_empty(),
        (Some(b'?'), [], b'm') => true,
        (Some(b'>'), [], b'q') => true,
        // XTWINOPS reports (sizes, position, title); the rest act.
        (None, [], b't') => matches!(first, Some(11 | 13 | 14 | 15 | 16 | 18 | 19 | 20 | 21)),
        _ => false,
    }
}

/// Whether an OSC body (between the introducer and the terminator) asks the
/// terminal for a reply: a `?` in place of a value, as in `11;?` (default
/// background), `4;1;?` (palette entry) or `52;c;?` (clipboard contents).
fn is_osc_query(body: &[u8]) -> bool {
    body.split(|&b| b == b';').skip(1).any(|field| field == b"?")
}

/// [`ReplayFilter`] over a whole buffer.
pub fn filter_replay(input: &[u8], keep_clipboard: bool) -> Vec<u8> {
    let mut filter = ReplayFilter::new(keep_clipboard);
    let mut output = Vec::with_capacity(input.len());
    filter.write_filtered(input, &mut output);
    filter.finish(&mut output);
    output
}

/// Passive human-readable logs use a strict allowlist: printable Unicode,
/// LF, TAB, and standard SGR styling. Every other terminal protocol is
/// discarded, including all OSC/DCS/APC/PM/SOS strings, private CSI modes,
/// queries, cursor movement, keyboard protocols, BEL, CR and backspace.
pub struct LogFilter {
    parser: Parser,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self { parser: Parser::new() }
    }
}

impl LogFilter {
    pub fn write_filtered(&mut self, input: &[u8], output: &mut Vec<u8>) {
        self.parser.advance(&mut SafeLogOutput { output }, input);
    }

    /// Incomplete control sequences remain discarded. There is deliberately
    /// nothing to flush: only fully parsed allowlisted output is emitted.
    pub fn finish(&mut self, _output: &mut Vec<u8>) {}
}

struct SafeLogOutput<'a> {
    output: &'a mut Vec<u8>,
}

impl Perform for SafeLogOutput<'_> {
    fn print(&mut self, c: char) {
        let mut encoded = [0; 4];
        self.output.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\t') {
            self.output.push(byte);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore || action != 'm' || !intermediates.is_empty() {
            return;
        }
        self.output.extend_from_slice(b"\x1b[");
        for (index, param) in params.iter().enumerate() {
            if index > 0 {
                self.output.push(b';');
            }
            for (subindex, value) in param.iter().enumerate() {
                if subindex > 0 {
                    self.output.push(b':');
                }
                self.output.extend_from_slice(value.to_string().as_bytes());
            }
        }
        self.output.push(b'm');
    }
}

pub fn filter_logs(input: &[u8]) -> Vec<u8> {
    let mut filter = LogFilter::default();
    let mut output = Vec::with_capacity(input.len());
    filter.write_filtered(input, &mut output);
    filter.finish(&mut output);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_drops_clipboard_writes_unless_asked() {
        let input = b"a\x1b]52;c;Y29weQ==\x07b\x1b]0;title\x07\x1b[?1003h";
        assert_eq!(filter_replay(input, false), b"ab\x1b]0;title\x07\x1b[?1003h");
        assert_eq!(filter_replay(input, true), input);
    }

    #[test]
    fn replay_drops_every_query() {
        let queries: &[&[u8]] = &[
            b"\x1b]10;?\x1b\\",
            b"\x1b]11;?\x07",
            b"\x1b]4;1;?\x07",
            b"\x1b]52;c;?\x07",
            b"\x1b[c",
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[6n",
            b"\x1b[?996n",
            b"\x1b[?2026$p",
            b"\x1b[?u",
            b"\x1b[?4m",
            b"\x1b[>0q",
            b"\x1b[14t",
            b"\x1bP$q q\x1b\\",
            b"\x1bP+q544e\x1b\\",
        ];
        for query in queries {
            let input = [b"a".as_slice(), query, b"b"].concat();
            assert_eq!(filter_replay(&input, true), b"ab", "{:?}", String::from_utf8_lossy(query));
        }
    }

    #[test]
    fn replay_keeps_everything_that_draws() {
        let input: &[u8] = b"\x1b[1;31mred\x1b[0m\x1b[2J\x1b[5;10H\x1b[?1049h\x1b[>1u\x1b[<u\x1b[2 q\x1b[8;24;80t\
            \x1b]0;title\x07\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\\x1bPq#0;2;0;0;0~\x1b\\\x1b7\x1b8\xe7\x9d\x85";
        assert_eq!(filter_replay(input, false), input);
    }

    #[test]
    fn replay_filter_handles_sequences_split_across_frames() {
        let mut filter = ReplayFilter::new(false);
        let mut output = Vec::new();
        for frame in
            [b"before\x1b]5".as_slice(), b"2;c;payload\x1b", b"\\mid\x1b[", b"6", b"nafter\x1b]1", b"1;?\x07end"]
        {
            filter.write_filtered(frame, &mut output);
        }
        filter.finish(&mut output);
        assert_eq!(output, b"beforemidafterend");
    }

    #[test]
    fn a_sequence_cut_off_by_the_end_of_replay_is_handed_on() {
        assert_eq!(filter_replay(b"a\x1b[6", false), b"a\x1b[6");
        assert_eq!(filter_replay(b"a\x1b]11;", false), b"a\x1b]11;");
        assert_eq!(filter_replay(b"a\x1b]52;c;abc", false), b"a", "a clipboard write stays dropped");
    }

    #[test]
    fn logs_allow_text_newlines_tabs_and_sgr() {
        let input = "你好\tworld\r\n\x1b[1;38;5;220mcolor\x1b[m".as_bytes();
        assert_eq!(filter_logs(input), "你好\tworld\n\x1b[1;38;5;220mcolor\x1b[0m".as_bytes());
    }

    #[test]
    fn logs_drop_every_non_sgr_terminal_protocol() {
        let input = b"a\x07\x08\r\x1b7\x1b[H\x1b[?1003h\x1b[>5u\x1b[>4;2m\x1b[c\x1b]52;c;x\x07\x1bP$qm\x1b\\\x1b_payload\x1b\\b";
        assert_eq!(filter_logs(input), b"ab");
    }

    #[test]
    fn logs_reject_private_csi_ending_in_m() {
        assert_eq!(filter_logs(b"a\x1b[>4;2mb"), b"ab");
    }

    #[test]
    fn log_filter_handles_sequences_split_across_frames() {
        let mut filter = LogFilter::default();
        let mut output = Vec::new();
        filter.write_filtered(b"before\x1b[38;5", &mut output);
        filter.write_filtered(b";220mafter\x1b]0;ti", &mut output);
        filter.write_filtered(b"tle\x07end", &mut output);
        filter.finish(&mut output);
        assert_eq!(output, b"before\x1b[38;5;220mafterend");
    }
}
