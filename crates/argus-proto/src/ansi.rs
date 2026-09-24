//! Streaming filters for terminal control sequences with external side effects.

/// Removes OSC 52 clipboard sequences while preserving every other byte.
/// Handles both `ESC ]`/C1 OSC introducers and BEL/ST terminators, including
/// sequences split across transport frames.
#[derive(Default)]
pub struct Osc52Filter {
    state: State,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Esc,
    OscCommand {
        raw: Vec<u8>,
        command: Vec<u8>,
    },
    Osc52 {
        esc: bool,
    },
    PassOsc {
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
            State::Ground => match byte {
                0x1b => State::Esc,
                0x9d => State::OscCommand { raw: vec![0x9d], command: Vec::new() },
                _ => {
                    output.push(byte);
                    State::Ground
                }
            },
            State::Esc if byte == b']' => State::OscCommand { raw: vec![0x1b, b']'], command: Vec::new() },
            State::Esc => {
                output.push(0x1b);
                self.state = State::Ground;
                self.write_byte(byte, output);
                return;
            }
            State::OscCommand { mut raw, mut command } => {
                raw.push(byte);
                if byte == b';' {
                    if command == b"52" {
                        State::Osc52 { esc: false }
                    } else {
                        output.extend_from_slice(&raw);
                        State::PassOsc { esc: false }
                    }
                } else if byte == 0x07 || byte == 0x9c || raw.ends_with(b"\x1b\\") {
                    output.extend_from_slice(&raw);
                    State::Ground
                } else if command.len() >= 16 {
                    output.extend_from_slice(&raw);
                    State::PassOsc { esc: byte == 0x1b }
                } else {
                    command.push(byte);
                    State::OscCommand { raw, command }
                }
            }
            State::Osc52 { esc } => {
                if byte == 0x07 || byte == 0x9c || (esc && byte == b'\\') {
                    State::Ground
                } else {
                    State::Osc52 { esc: byte == 0x1b }
                }
            }
            State::PassOsc { esc } => {
                output.push(byte);
                if byte == 0x07 || byte == 0x9c || (esc && byte == b'\\') {
                    State::Ground
                } else {
                    State::PassOsc { esc: byte == 0x1b }
                }
            }
        };
    }

    /// Flushes an incomplete non-OSC-52 prefix. An incomplete OSC 52 is
    /// discarded conservatively rather than leaking a clipboard write.
    pub fn finish(&mut self, output: &mut Vec<u8>) {
        match std::mem::take(&mut self.state) {
            State::Esc => output.push(0x1b),
            State::OscCommand { raw, .. } => output.extend_from_slice(&raw),
            State::Ground | State::Osc52 { .. } | State::PassOsc { .. } => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bel_and_st_terminated_osc52() {
        let input = b"a\x1b]52;c;Y29weQ==\x07b\x1b]52;p;dGVzdA==\x1b\\c";
        assert_eq!(strip_osc52(input), b"abc");
    }

    #[test]
    fn handles_frames_split_inside_the_sequence() {
        let mut filter = Osc52Filter::default();
        let mut output = Vec::new();
        filter.write_filtered(b"before\x1b]5", &mut output);
        filter.write_filtered(b"2;c;payload\x1b", &mut output);
        filter.write_filtered(b"\\after", &mut output);
        filter.finish(&mut output);
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn preserves_other_osc_sequences_exactly() {
        let input = b"\x1b]0;title\x07\x1b]11;?\x1b\\";
        assert_eq!(strip_osc52(input), input);
    }
}
