//! Streaming filters for terminal output with external side effects.

use vte::{Params, Parser, Perform};

/// Removes OSC 52 clipboard sequences while preserving every other byte.
/// This narrowly scoped filter is for interactive attach replay, where all
/// terminal drawing commands must survive but historical clipboard writes
/// must not run again.
#[derive(Default)]
pub struct Osc52Filter {
    state: OscState,
}

#[derive(Default)]
enum OscState {
    #[default]
    Ground,
    Esc,
    Command {
        raw: Vec<u8>,
        command: Vec<u8>,
    },
    Osc52 {
        esc: bool,
    },
    Pass {
        esc: bool,
    },
}

impl Osc52Filter {
    pub fn write_filtered(&mut self, input: &[u8], output: &mut Vec<u8>) {
        for &byte in input {
            self.write_byte(byte, output);
        }
    }

    fn write_byte(&mut self, byte: u8, output: &mut Vec<u8>) {
        let state = std::mem::take(&mut self.state);
        self.state = match state {
            OscState::Ground => match byte {
                0x1b => OscState::Esc,
                0x9d => OscState::Command { raw: vec![0x9d], command: Vec::new() },
                _ => {
                    output.push(byte);
                    OscState::Ground
                }
            },
            OscState::Esc if byte == b']' => OscState::Command { raw: vec![0x1b, b']'], command: Vec::new() },
            OscState::Esc => {
                output.push(0x1b);
                self.state = OscState::Ground;
                self.write_byte(byte, output);
                return;
            }
            OscState::Command { mut raw, mut command } => {
                raw.push(byte);
                if byte == b';' {
                    if command == b"52" {
                        OscState::Osc52 { esc: false }
                    } else {
                        output.extend_from_slice(&raw);
                        OscState::Pass { esc: false }
                    }
                } else if byte == 0x07 || byte == 0x9c || raw.ends_with(b"\x1b\\") {
                    output.extend_from_slice(&raw);
                    OscState::Ground
                } else if command.len() >= 16 {
                    output.extend_from_slice(&raw);
                    OscState::Pass { esc: byte == 0x1b }
                } else {
                    command.push(byte);
                    OscState::Command { raw, command }
                }
            }
            OscState::Osc52 { esc } => {
                if byte == 0x07 || byte == 0x9c || (esc && byte == b'\\') {
                    OscState::Ground
                } else {
                    OscState::Osc52 { esc: byte == 0x1b }
                }
            }
            OscState::Pass { esc } => {
                output.push(byte);
                if byte == 0x07 || byte == 0x9c || (esc && byte == b'\\') {
                    OscState::Ground
                } else {
                    OscState::Pass { esc: byte == 0x1b }
                }
            }
        };
    }

    /// Flushes an incomplete non-OSC-52 prefix. An incomplete OSC 52 is
    /// discarded conservatively rather than leaking a clipboard write.
    pub fn finish(&mut self, output: &mut Vec<u8>) {
        match std::mem::take(&mut self.state) {
            OscState::Esc => output.push(0x1b),
            OscState::Command { raw, .. } => output.extend_from_slice(&raw),
            OscState::Ground | OscState::Osc52 { .. } | OscState::Pass { .. } => {}
        }
    }
}

pub fn strip_osc52(input: &[u8]) -> Vec<u8> {
    let mut filter = Osc52Filter::default();
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
    fn attach_replay_strips_only_osc52() {
        let input = b"a\x1b]52;c;Y29weQ==\x07b\x1b]0;title\x07\x1b[?1003h";
        assert_eq!(strip_osc52(input), b"ab\x1b]0;title\x07\x1b[?1003h");
    }

    #[test]
    fn osc52_filter_handles_frames_split_inside_the_sequence() {
        let mut filter = Osc52Filter::default();
        let mut output = Vec::new();
        filter.write_filtered(b"before\x1b]5", &mut output);
        filter.write_filtered(b"2;c;payload\x1b", &mut output);
        filter.write_filtered(b"\\after", &mut output);
        filter.finish(&mut output);
        assert_eq!(output, b"beforeafter");
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
