//! The caller's terminal: raw mode, and what it says about itself.
//!
//! Argus asks the terminal once per process, before anything of its own or an
//! agent's has changed it, and keeps the answers as a [`Profile`]: attach
//! restores the cursor shape from it on the way out, and holders answer an
//! agent's colour queries from it while no terminal is attached.

use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argus_proto::msg::TerminalColors;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::isatty;

/// DECRQSS for the cursor shape (DECSCUSR), OSC 10 / OSC 11 for the default
/// colours, then DA1. Every terminal answers DA1 and replies come back in
/// order, so its arrival means every other answer is in: a terminal that
/// ignores some of these costs no timeout.
const PROBE: &str = "\x1bP$q q\x1b\\\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c";
/// Upper bound for a terminal that answers nothing, not even DA1.
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

static PROFILE: OnceLock<Profile> = OnceLock::new();

#[derive(Debug, Default)]
pub struct Profile {
    /// DECSCUSR parameter. Neither the alternate screen nor anything else
    /// scopes the shape, so one agent's (Codex resets it to the terminal
    /// default every frame) would otherwise carry into the tree and the next
    /// agent attached.
    pub cursor_shape: Option<u16>,
    pub colors: TerminalColors,
}

/// The terminal's profile, probed on first use. Call it before anything
/// changes the terminal: `argus tree` does so at startup, since by its second
/// attach the cursor shape is whatever the first agent left.
pub fn profile() -> &'static Profile {
    PROFILE.get_or_init(|| parse_profile(&probe()))
}

/// Resets the cursor shape to what the profile found; to the terminal's
/// default if it could not say.
pub fn restore_cursor_shape() -> String {
    format!("\x1b[{} q", profile().cursor_shape.unwrap_or(0))
}

/// Writes [`PROBE`] and collects the replies up to DA1's. Empty unless both
/// stdin and stdout are the terminal: the probe must not end up in a pipe,
/// nor wait on input that is not a terminal's.
fn probe() -> Vec<u8> {
    if !isatty(io::stdin().as_raw_fd()).unwrap_or(false) || !isatty(io::stdout().as_raw_fd()).unwrap_or(false) {
        return Vec::new();
    }
    let Ok(_raw) = RawMode::enter() else { return Vec::new() };
    let mut out = io::stdout().lock();
    if out.write_all(PROBE.as_bytes()).and_then(|()| out.flush()).is_err() {
        return Vec::new();
    }
    drop(out);

    let stdin = io::stdin();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    while !has_da1_reply(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::try_from(left).unwrap_or(PollTimeout::ZERO)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(_) => break,
        }
        match nix::unistd::read(stdin.as_raw_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
            Err(Errno::EINTR | Errno::EAGAIN) => {}
            Err(_) => break,
        }
    }
    reply
}

fn parse_profile(reply: &[u8]) -> Profile {
    Profile {
        cursor_shape: parse_cursor_shape(reply),
        colors: TerminalColors { foreground: parse_osc_color(reply, b"10"), background: parse_osc_color(reply, b"11") },
    }
}

/// Whether `reply` holds a complete DA1 answer, `ESC [ ? … c`.
fn has_da1_reply(reply: &[u8]) -> bool {
    find(reply, b"\x1b[?").is_some_and(|i| reply[i + 3..].contains(&b'c'))
}

/// The shape in a DECRQSS answer, `ESC P 1 $ r <n> SP q ESC \`. tmux echoes
/// the request in front (`… $ r SP q <n> SP q …`), so the number is taken as
/// the digits right before the final `SP q`.
fn parse_cursor_shape(reply: &[u8]) -> Option<u16> {
    let start = find(reply, b"\x1bP1$r")? + 5;
    let end = start + find(&reply[start..], b"\x1b\\")?;
    let body = std::str::from_utf8(&reply[start..end]).ok()?.strip_suffix(" q")?;
    let digits = body.rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    body[digits..].parse().ok().filter(|&n| n <= 6)
}

/// The colour in an `ESC ] <code> ; <spec> ST` reply, ST being `ESC \` or BEL.
fn parse_osc_color(reply: &[u8], code: &[u8]) -> Option<String> {
    let prefix = [b"\x1b]", code, b";"].concat();
    let start = find(reply, &prefix)? + prefix.len();
    let end = start + reply[start..].iter().position(|&b| b == 0x1b || b == 0x07)?;
    let spec = std::str::from_utf8(&reply[start..end]).ok()?;
    TerminalColors::is_color_spec(spec).then(|| spec.to_string())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Writes an OSC 52 clipboard-set sequence for `text` straight to stdout.
/// Safe to call while ratatui owns the terminal (alternate screen, raw
/// mode): it's a plain escape sequence, the same kind an attached agent
/// would emit itself, just not routed through a holder.
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))?;
    out.flush()
}

const BASE64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let sextets = [(n >> 18) & 0x3f, (n >> 12) & 0x3f, (n >> 6) & 0x3f, n & 0x3f];
        for (i, s) in sextets.iter().enumerate() {
            out.push(if i <= chunk.len() { BASE64_ALPHABET[*s as usize] as char } else { '=' });
        }
    }
    out
}

/// Puts the terminal in raw mode and restores it on drop, including on panic.
pub struct RawMode {
    saved: Termios,
}

impl RawMode {
    pub fn enter() -> Result<RawMode> {
        let stdin = io::stdin();
        let saved = tcgetattr(stdin.as_fd()).context("reading terminal settings")?;
        let mut raw = saved.clone();
        cfmakeraw(&mut raw);
        tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw).context("entering raw mode")?;
        Ok(RawMode { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(io::stdin().as_fd(), SetArg::TCSANOW, &self.saved);
    }
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, has_da1_reply, parse_cursor_shape, parse_osc_color, parse_profile};

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"research/claude-1"), "cmVzZWFyY2gvY2xhdWRlLTE=");
    }

    #[test]
    fn a_full_reply() {
        let reply = b"\x1bP1$r6 q\x1b\\\x1b]10;rgb:dcdc/dfdf/e4e4\x1b\\\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[?62;22c";
        let profile = parse_profile(reply);
        assert_eq!(profile.cursor_shape, Some(6));
        assert_eq!(profile.colors.foreground.as_deref(), Some("rgb:dcdc/dfdf/e4e4"));
        assert_eq!(profile.colors.background.as_deref(), Some("rgb:1e1e/1e1e/2e2e"));
    }

    #[test]
    fn a_terminal_that_only_answers_da1() {
        let profile = parse_profile(b"\x1b[?1;2c");
        assert_eq!(profile.cursor_shape, None);
        assert_eq!(profile.colors, Default::default());
    }

    #[test]
    fn cursor_shape_replies() {
        assert_eq!(parse_cursor_shape(b"\x1bP1$r q2 q\x1b\\"), Some(2), "tmux");
        assert_eq!(parse_cursor_shape(b"\x1bP1$r0 q\x1b\\"), Some(0));
        assert_eq!(parse_cursor_shape(b"\x1bP0$r\x1b\\\x1b[?1;2c"), None, "not supported");
        assert_eq!(parse_cursor_shape(b"\x1bP1$r9 q\x1b\\"), None, "out of range");
    }

    #[test]
    fn colour_replies() {
        assert_eq!(parse_osc_color(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07", b"11").as_deref(), Some("rgb:1e1e/1e1e/2e2e"));
        assert_eq!(parse_osc_color(b"\x1b]11;rgb:0/0/0", b"11"), None, "unterminated");
        assert_eq!(parse_osc_color(b"\x1b]11;?;x\x1b\\", b"11"), None, "not a colour");
    }

    #[test]
    fn da1_ends_the_probe() {
        assert!(has_da1_reply(b"\x1bP1$r6 q\x1b\\\x1b[?62;22c"));
        assert!(!has_da1_reply(b"\x1bP1$r6 q\x1b\\\x1b[?62;2"));
    }
}
