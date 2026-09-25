//! Stands in for the terminal while none is attached.
//!
//! Agents ask the terminal about itself; Codex, for one, derives its palette
//! from the default background (OSC 11) and waits only briefly for it. While a
//! writable terminal is attached, the queries reach it along with the rest of
//! the output and it answers them itself. Otherwise the holder answers at once,
//! as the last terminal it knew of would have: the one `argus run` started
//! from, or the one attached most recently.

use argus_proto::msg::TerminalColors;

/// The queries answered, as `(OSC code, reply when the colour is unknown)`.
const COLOR_QUERIES: [(&[u8], &str); 2] = [(b"10", "rgb:e5e5/e5e5/e5e5"), (b"11", "rgb:0000/0000/0000")];
/// `ESC ] 1x ; ? ESC \`: the longest query, so the longest one that can
/// straddle two reads.
const LONGEST_QUERY: usize = 8;

#[derive(Default)]
pub struct StandIn {
    colors: TerminalColors,
    /// The end of the previous output, for a query split across two reads.
    tail: Vec<u8>,
}

impl StandIn {
    pub fn new(colors: TerminalColors) -> StandIn {
        let mut stand_in = StandIn::default();
        stand_in.learn(colors);
        stand_in
    }

    /// Takes on a terminal's colours, keeping what it could not tell.
    pub fn learn(&mut self, colors: TerminalColors) {
        let valid = |c: Option<String>| c.filter(|c| TerminalColors::is_color_spec(c));
        if let Some(fg) = valid(colors.foreground) {
            self.colors.foreground = Some(fg);
        }
        if let Some(bg) = valid(colors.background) {
            self.colors.background = Some(bg);
        }
    }

    /// The replies to the queries in `output`, in order, each terminated the
    /// way its query was. Every piece of output must pass through here, even
    /// while a terminal is attached and the replies are dropped, so that the
    /// tail stays in step.
    pub fn answer(&mut self, output: &[u8]) -> Vec<u8> {
        let combined = [self.tail.as_slice(), output].concat();
        let mut replies = Vec::new();
        let mut at = 0;
        while let Some(found) = find(&combined[at..], b"\x1b]") {
            at += found;
            let rest = &combined[at + 2..];
            for (code, fallback) in COLOR_QUERIES {
                let Some(query) = rest.strip_prefix(code).and_then(|r| r.strip_prefix(b";?")) else { continue };
                let Some(st) = [b"\x07".as_slice(), b"\x1b\\"].into_iter().find(|st| query.starts_with(st)) else {
                    continue;
                };
                // Wholly inside the tail: answered with the previous output.
                if at + 2 + code.len() + 2 + st.len() <= self.tail.len() {
                    continue;
                }
                let color = self.color(code).unwrap_or(fallback);
                replies.extend_from_slice(b"\x1b]");
                replies.extend_from_slice(code);
                replies.extend_from_slice(format!(";{color}").as_bytes());
                replies.extend_from_slice(st);
            }
            at += 2;
        }
        let keep = (LONGEST_QUERY - 1).min(combined.len());
        self.tail = combined[combined.len() - keep..].to_vec();
        replies
    }

    fn color(&self, code: &[u8]) -> Option<&str> {
        match code {
            b"10" => self.colors.foreground.as_deref(),
            _ => self.colors.background.as_deref(),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> TerminalColors {
        TerminalColors { foreground: Some("rgb:dddd/dddd/dddd".into()), background: Some("rgb:1e1e/1e1e/2e2e".into()) }
    }

    #[test]
    fn answers_with_the_known_colours_in_the_querys_own_terminator() {
        let mut stand_in = StandIn::new(known());
        assert_eq!(
            stand_in.answer(b"x\x1b]10;?\x1b\\y\x1b]11;?\x07z"),
            b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\\x1b]11;rgb:1e1e/1e1e/2e2e\x07"
        );
    }

    #[test]
    fn falls_back_when_no_terminal_said() {
        let mut stand_in = StandIn::default();
        assert_eq!(stand_in.answer(b"\x1b]11;?\x1b\\"), b"\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn a_query_split_across_reads_is_answered_once() {
        let mut stand_in = StandIn::new(known());
        assert_eq!(stand_in.answer(b"before\x1b]11"), b"");
        assert_eq!(stand_in.answer(b";?\x07after"), b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07");
        assert_eq!(stand_in.answer(b"more"), b"", "the tail still holds it");
        assert_eq!(stand_in.answer(b"\x1b]10;?\x07"), b"\x1b]10;rgb:dddd/dddd/dddd\x07");
        assert_eq!(stand_in.answer(b""), b"", "short queries fit in the tail whole");
    }

    #[test]
    fn a_later_terminal_replaces_only_what_it_knows() {
        let mut stand_in = StandIn::new(known());
        stand_in.learn(TerminalColors { foreground: None, background: Some("#ffffff".into()) });
        assert_eq!(
            stand_in.answer(b"\x1b]10;?\x07\x1b]11;?\x07"),
            b"\x1b]10;rgb:dddd/dddd/dddd\x07\x1b]11;#ffffff\x07"
        );
    }

    #[test]
    fn ignores_what_is_not_a_colour_spec() {
        let mut stand_in = StandIn::new(TerminalColors { foreground: None, background: Some("0\x1b]52;c;x".into()) });
        assert_eq!(stand_in.answer(b"\x1b]11;?\x07"), b"\x1b]11;rgb:0000/0000/0000\x07");
    }

    #[test]
    fn leaves_other_osc_sequences_alone() {
        let mut stand_in = StandIn::new(known());
        assert_eq!(stand_in.answer(b"\x1b]0;title\x07\x1b]11;rgb:1/2/3\x07\x1b]12;?\x07"), b"");
    }
}
