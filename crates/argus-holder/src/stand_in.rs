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
        let mut replies = Vec::new();
        // Queries that start in the previous output's tail: only the seam
        // needs joining, not the whole output. One wholly inside the tail
        // was answered with the previous output.
        if !self.tail.is_empty() {
            let seam = [self.tail.as_slice(), &output[..output.len().min(LONGEST_QUERY)]].concat();
            for at in memchr::memmem::find_iter(&seam, b"\x1b]").take_while(|&at| at < self.tail.len()) {
                if let Some(query) = Query::parse(&seam[at..])
                    && at + query.len > self.tail.len()
                {
                    self.reply(&query, &mut replies);
                }
            }
        }
        for at in memchr::memmem::find_iter(output, b"\x1b]") {
            if let Some(query) = Query::parse(&output[at..]) {
                self.reply(&query, &mut replies);
            }
        }
        self.keep_tail(output);
        replies
    }

    fn reply(&self, query: &Query, replies: &mut Vec<u8>) {
        let color = self.color(query.code).unwrap_or(query.fallback);
        replies.extend_from_slice(b"\x1b]");
        replies.extend_from_slice(query.code);
        replies.extend_from_slice(format!(";{color}").as_bytes());
        replies.extend_from_slice(query.st);
    }

    /// Keeps the last `LONGEST_QUERY - 1` bytes seen, for a query split
    /// across two reads.
    fn keep_tail(&mut self, output: &[u8]) {
        let keep = LONGEST_QUERY - 1;
        if output.len() >= keep {
            self.tail.clear();
            self.tail.extend_from_slice(&output[output.len() - keep..]);
        } else {
            self.tail.extend_from_slice(output);
            let excess = self.tail.len().saturating_sub(keep);
            self.tail.drain(..excess);
        }
    }

    fn color(&self, code: &[u8]) -> Option<&str> {
        match code {
            b"10" => self.colors.foreground.as_deref(),
            _ => self.colors.background.as_deref(),
        }
    }
}

/// A colour query we answer.
struct Query {
    code: &'static [u8],
    /// The reply when the colour is unknown.
    fallback: &'static str,
    /// The terminator, repeated in the reply.
    st: &'static [u8],
    len: usize,
}

impl Query {
    /// The query `bytes` starts with, if it is one we answer.
    fn parse(bytes: &[u8]) -> Option<Query> {
        let rest = bytes.strip_prefix(b"\x1b]")?;
        COLOR_QUERIES.into_iter().find_map(|(code, fallback)| {
            let query = rest.strip_prefix(code)?.strip_prefix(b";?")?;
            let st: &'static [u8] = [b"\x07".as_slice(), b"\x1b\\"].into_iter().find(|st| query.starts_with(st))?;
            Some(Query { code, fallback, st, len: 2 + code.len() + 2 + st.len() })
        })
    }
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
    fn any_split_gives_the_same_replies() {
        let stream = b"a\x1b]11;?\x07b\x1b]0;t\x07\x1b]10;?\x1b\\\x1b]11;?\x1b\\c\x1b]11;?\x07";
        let whole = StandIn::new(known()).answer(stream);
        assert_eq!(whole.iter().filter(|&&b| b == b']').count(), 4);
        for cut in 0..=stream.len() {
            let mut stand_in = StandIn::new(known());
            let split = [stand_in.answer(&stream[..cut]), stand_in.answer(&stream[cut..])].concat();
            assert_eq!(split, whole, "cut at {cut}");
        }
        for size in 1..=3 {
            let mut stand_in = StandIn::new(known());
            let pieces: Vec<u8> = stream.chunks(size).flat_map(|c| stand_in.answer(c)).collect();
            assert_eq!(pieces, whole, "pieces of {size}");
        }
    }

    #[test]
    fn leaves_other_osc_sequences_alone() {
        let mut stand_in = StandIn::new(known());
        assert_eq!(stand_in.answer(b"\x1b]0;title\x07\x1b]11;rgb:1/2/3\x07\x1b]12;?\x07"), b"");
    }
}
