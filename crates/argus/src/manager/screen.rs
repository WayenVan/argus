//! Per-agent virtual terminal, kept in sync with the holder's live output so
//! `attach` and screen-preview TUIs can restore a screen instantly, without
//! depending on the agent to redraw itself.
//!
//! Only the current visible screen is tracked (no vt100 scrollback): once an
//! agent has entered the alternate screen, replaying its history is neither
//! possible (alt screen has none) nor useful. Before that, recovering
//! earlier output means replaying the holder's own ring buffer instead of a
//! synthetic redraw — see [`ScreenMode::Replay`].

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use argus_proto::msg::{HolderEvent, PreviewColor, PreviewLine, PreviewSpan, ScreenMode};

use super::{holder, log};

/// Pause before re-subscribing a tracker that failed, so a failure that
/// repeats on every attempt cannot spin.
const RESTART_DELAY: Duration = Duration::from_secs(1);

struct State {
    parser: vt100::Parser,
    /// Running total of bytes fed into `parser`: the holder offset it has
    /// been brought up to date with.
    offset: u64,
}

/// Told whether the agent shows a cursor on its alternate screen, each time
/// that changes; shared so it survives a tracker restart.
pub type OnCursor = Arc<dyn Fn(bool) + Send + Sync>;

pub struct ScreenReply {
    pub mode: ScreenMode,
    pub rows: u16,
    pub cols: u16,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// Each agent's state has its own lock, and the map's lock is never held
/// while vt100 runs: a panic in one agent's parser poisons only that agent,
/// whose tracker then starts over from a fresh parser.
pub struct Screens {
    states: Mutex<HashMap<u64, Arc<Mutex<State>>>>,
}

impl Screens {
    pub fn new() -> Arc<Screens> {
        Arc::new(Screens { states: Mutex::new(HashMap::new()) })
    }

    /// Subscribes to `id`'s output at `SubscribeLevel::Output` and keeps a
    /// virtual terminal in sync until the agent exits or the connection is
    /// lost. A separate holder connection from the lifecycle `follow` task,
    /// kept simple rather than threading screen state through it.
    ///
    /// A tracker that panics or finds its state poisoned is started over:
    /// re-subscribing backfills from the holder's ring buffer, the same way
    /// a tracker that starts late catches up.
    pub fn track(self: &Arc<Self>, id: u64, on_cursor: Option<OnCursor>) {
        let screens = self.clone();
        tokio::spawn(async move {
            loop {
                // Its own task, so a panic ends the attempt and not this loop.
                let attempt = tokio::spawn(screens.clone().follow(id, on_cursor.clone()));
                let failure = match attempt.await {
                    Ok(Ok(ControlFlow::Break(()))) => "its screen state was poisoned",
                    Err(e) if e.is_panic() => "it panicked",
                    // The agent exited or its holder went away.
                    _ => break,
                };
                screens.states.lock().unwrap().remove(&id);
                log(&format!("screen tracking for agent {id} stopped because {failure}; restarting it"));
                tokio::time::sleep(RESTART_DELAY).await;
            }
            screens.states.lock().unwrap().remove(&id);
        });
    }

    async fn follow(self: Arc<Self>, id: u64, on_cursor: Option<OnCursor>) -> anyhow::Result<ControlFlow<()>> {
        let on_event = |event: HolderEvent| self.on_event(id, event);
        let mut shown = false;
        let on_data = |start: u64, bytes: &[u8]| {
            let now_shown = self.on_data(id, start, bytes)?;
            if now_shown != shown {
                shown = now_shown;
                if let Some(f) = &on_cursor {
                    f(shown);
                }
            }
            ControlFlow::Continue(())
        };
        holder::follow_screen(id, on_event, on_data).await
    }

    fn state(&self, id: u64) -> Option<Arc<Mutex<State>>> {
        self.states.lock().unwrap().get(&id).cloned()
    }

    /// Runs `f` on `id`'s state. `None` when nothing is tracked for it, or
    /// when its parser panicked earlier and cannot be trusted.
    fn read<R>(&self, id: u64, f: impl FnOnce(&State) -> R) -> Option<R> {
        let state = self.state(id)?;
        let guard = state.lock().ok()?;
        Some(f(&guard))
    }

    /// Breaks when the state is poisoned, so the tracker starts over.
    fn on_event(&self, id: u64, event: HolderEvent) -> ControlFlow<()> {
        let HolderEvent::Resized { rows, cols } = event else { return ControlFlow::Continue(()) };
        match self.state(id) {
            Some(state) => lock(&state)?.parser.screen_mut().set_size(rows, cols),
            // The holder pushes the current size right after a subscribe
            // succeeds, so this is always the first event for a fresh state.
            None => {
                let state = State { parser: vt100::Parser::new(rows, cols, 0), offset: 0 };
                self.states.lock().unwrap().insert(id, Arc::new(Mutex::new(state)));
            }
        }
        ControlFlow::Continue(())
    }

    /// Continues with whether the agent is showing a cursor on its alternate
    /// screen; breaks when the state is poisoned.
    fn on_data(&self, id: u64, start: u64, bytes: &[u8]) -> ControlFlow<(), bool> {
        let Some(state) = self.state(id) else { return ControlFlow::Continue(false) }; // No size yet.
        let mut state = lock(&state)?;
        state.parser.process(bytes);
        state.offset = start + bytes.len() as u64;
        let screen = state.parser.screen();
        ControlFlow::Continue(screen.alternate_screen() && !screen.hide_cursor())
    }

    /// How `id`'s screen should be restored. `since_offset` lets a caller
    /// that already has the screen at that offset skip the bytes.
    pub fn get(&self, id: u64, since_offset: Option<u64>) -> ScreenReply {
        self.read(id, |state| snapshot(state, since_offset)).unwrap_or(ScreenReply {
            mode: ScreenMode::Unavailable,
            rows: 0,
            cols: 0,
            offset: 0,
            bytes: vec![],
        })
    }

    /// A one-shot rendering of `id`'s current screen — alternate or primary
    /// — for `argus logs --screen`. Unlike `get`, this never enters the
    /// alternate screen itself: it is a readback, not an attach restore
    /// hint, so the caller's own screen (whatever it is) is left alone.
    pub fn dump(&self, id: u64) -> Option<(u16, u16, Vec<u8>)> {
        self.read(id, |state| {
            let screen = state.parser.screen();
            let (rows, cols) = screen.size();
            let mut bytes = screen.state_formatted();
            append_styled_blanks(screen, &mut bytes);
            bytes.extend(screen.cursor_state_formatted());
            bytes.extend(screen.attributes_formatted());
            (rows, cols, bytes)
        })
    }

    /// Whether `id` has turned on bracketed paste; `None` when nothing is
    /// tracked for it yet.
    pub fn bracketed_paste(&self, id: u64) -> Option<bool> {
        self.read(id, |state| state.parser.screen().bracketed_paste())
    }

    /// A styled crop of `id`'s screen to `rows`x`cols`, left-aligned from its
    /// top-left corner. Empty when nothing is tracked for it yet.
    pub fn preview(&self, id: u64, rows: u16, cols: u16) -> Vec<PreviewLine> {
        self.read(id, |state| preview(state, rows, cols)).unwrap_or_default()
    }
}

/// Locks one agent's state, breaking when an earlier panic poisoned it.
fn lock(state: &Mutex<State>) -> ControlFlow<(), MutexGuard<'_, State>> {
    match state.lock() {
        Ok(guard) => ControlFlow::Continue(guard),
        Err(_) => ControlFlow::Break(()),
    }
}

fn snapshot(state: &State, since_offset: Option<u64>) -> ScreenReply {
    let screen = state.parser.screen();
    let (rows, cols) = screen.size();
    if !screen.alternate_screen() {
        // 0 tells the holder to replay from its oldest retained byte:
        // there is no screen-owned history to fall back on instead.
        return ScreenReply { mode: ScreenMode::Replay, rows, cols, offset: 0, bytes: vec![] };
    }
    if since_offset == Some(state.offset) {
        return ScreenReply { mode: ScreenMode::Snapshot, rows, cols, offset: state.offset, bytes: vec![] };
    }
    // `state_formatted` covers cell contents, cursor position/visibility
    // and input modes (mouse reporting, bracketed paste, keypad); the
    // alternate-screen switch itself is not part of it.
    let mut bytes = b"\x1b[?1049h".to_vec();
    bytes.extend(screen.state_formatted());
    // vt100 represents blank styled cells with ECH/EL. Some nested
    // terminals (notably editor terminals) erase with their default
    // background instead of the active SGR background. Paint those cells
    // again as literal spaces so input boxes and other filled regions
    // survive a synthetic attach restore everywhere.
    append_styled_blanks(screen, &mut bytes);
    bytes.extend(screen.cursor_state_formatted());
    bytes.extend(screen.attributes_formatted());
    ScreenReply { mode: ScreenMode::Snapshot, rows, cols, offset: state.offset, bytes }
}

fn preview(state: &State, rows: u16, cols: u16) -> Vec<PreviewLine> {
    let screen = state.parser.screen();
    let (screen_rows, screen_cols) = screen.size();
    let rows = rows.min(screen_rows);
    let cols = cols.min(screen_cols);
    (0..rows)
        .map(|row| {
            let mut spans: PreviewLine = Vec::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let text = if cell.has_contents() { cell.contents() } else { " " };
                let span = PreviewSpan {
                    text: text.to_owned(),
                    fg: preview_color(cell.fgcolor()),
                    bg: preview_color(cell.bgcolor()),
                    bold: cell.bold(),
                    dim: cell.dim(),
                    italic: cell.italic(),
                    underline: cell.underline(),
                    inverse: cell.inverse(),
                };
                if let Some(last) = spans.last_mut().filter(|last| same_style(last, &span)) {
                    last.text.push_str(text);
                } else {
                    spans.push(span);
                }
            }
            while spans.last().is_some_and(|span| span.text.chars().all(|c| c == ' ') && is_default(span)) {
                spans.pop();
            }
            spans
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    fn from_cell(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    fn is_default(self) -> bool {
        self.fg == vt100::Color::Default
            && self.bg == vt100::Color::Default
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.inverse
    }
}

fn append_styled_blanks(screen: &vt100::Screen, bytes: &mut Vec<u8>) {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        let mut col = 0;
        while col < cols {
            let Some(cell) = screen.cell(row, col) else { break };
            let style = CellStyle::from_cell(cell);
            if cell.has_contents() || cell.is_wide_continuation() || style.is_default() {
                col += 1;
                continue;
            }

            let start = col;
            col += 1;
            while col < cols {
                let Some(next) = screen.cell(row, col) else { break };
                if next.has_contents() || next.is_wide_continuation() || CellStyle::from_cell(next) != style {
                    break;
                }
                col += 1;
            }

            bytes.extend(format!("\x1b[{};{}H", row + 1, start + 1).as_bytes());
            append_sgr(style, bytes);
            bytes.extend(std::iter::repeat_n(b' ', usize::from(col - start)));
        }
    }
}

fn append_sgr(style: CellStyle, bytes: &mut Vec<u8>) {
    let mut params = vec![0];
    if style.bold {
        params.push(1);
    }
    if style.dim {
        params.push(2);
    }
    if style.italic {
        params.push(3);
    }
    if style.underline {
        params.push(4);
    }
    if style.inverse {
        params.push(7);
    }
    append_color_params(style.fg, false, &mut params);
    append_color_params(style.bg, true, &mut params);
    bytes.extend(b"\x1b[");
    for (i, param) in params.iter().enumerate() {
        if i != 0 {
            bytes.push(b';');
        }
        bytes.extend(param.to_string().as_bytes());
    }
    bytes.push(b'm');
}

fn append_color_params(color: vt100::Color, background: bool, params: &mut Vec<u16>) {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    let extended = if background { 48 } else { 38 };
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(index @ 0..=7) => params.push(base + u16::from(index)),
        vt100::Color::Idx(index @ 8..=15) => params.push(bright + u16::from(index - 8)),
        vt100::Color::Idx(index) => params.extend([extended, 5, u16::from(index)]),
        vt100::Color::Rgb(r, g, b) => {
            params.extend([extended, 2, u16::from(r), u16::from(g), u16::from(b)]);
        }
    }
}

fn preview_color(color: vt100::Color) -> PreviewColor {
    match color {
        vt100::Color::Default => PreviewColor::Default,
        vt100::Color::Idx(index) => PreviewColor::Indexed(index),
        vt100::Color::Rgb(r, g, b) => PreviewColor::Rgb([r, g, b]),
    }
}

fn same_style(a: &PreviewSpan, b: &PreviewSpan) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.dim == b.dim
        && a.italic == b.italic
        && a.underline == b.underline
        && a.inverse == b.inverse
}

fn is_default(span: &PreviewSpan) -> bool {
    span.fg == PreviewColor::Default
        && span.bg == PreviewColor::Default
        && !span.bold
        && !span.dim
        && !span.italic
        && !span.underline
        && !span.inverse
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screens() -> Screens {
        Screens { states: Mutex::new(HashMap::new()) }
    }

    fn resize(s: &Screens, id: u64, rows: u16, cols: u16) {
        assert!(s.on_event(id, HolderEvent::Resized { rows, cols }).is_continue());
    }

    fn feed(s: &Screens, id: u64, start: u64, bytes: &[u8]) {
        assert!(s.on_data(id, start, bytes).is_continue());
    }

    #[test]
    fn unavailable_before_any_state() {
        let s = screens();
        let r = s.get(1, None);
        assert_eq!(r.mode, ScreenMode::Unavailable);
        assert!(r.bytes.is_empty());
    }

    #[test]
    fn data_before_a_size_is_known_is_dropped() {
        let s = screens();
        feed(&s, 1, 0, b"too early");
        assert_eq!(s.get(1, None).mode, ScreenMode::Unavailable);
    }

    #[test]
    fn cursor_counts_only_once_shown_on_the_alternate_screen() {
        let s = screens();
        resize(&s, 1, 24, 80);
        let shown = |flow| flow == ControlFlow::Continue(true);
        assert!(!shown(s.on_data(1, 0, b"$ ")), "the primary screen's default cursor is not a prompt");
        assert!(!shown(s.on_data(1, 2, b"\x1b[?1049h\x1b[?25l")), "entered, cursor hidden while loading");
        assert!(shown(s.on_data(1, 16, b"\x1b[?25h")));
    }

    #[test]
    fn replay_mode_without_alternate_screen() {
        let s = screens();
        resize(&s, 1, 24, 80);
        feed(&s, 1, 0, b"conversation line\r\n");
        let r = s.get(1, None);
        assert_eq!(r.mode, ScreenMode::Replay);
        assert_eq!((r.rows, r.cols), (24, 80));
        // 0 tells the holder to replay from its oldest retained byte, not
        // from wherever the manager's own tracking happens to be.
        assert_eq!(r.offset, 0);
        assert!(r.bytes.is_empty());
    }

    #[test]
    fn snapshot_mode_in_alternate_screen() {
        let s = screens();
        resize(&s, 1, 24, 80);
        feed(&s, 1, 0, b"\x1b[?1049hHELLO");
        let r = s.get(1, None);
        assert_eq!(r.mode, ScreenMode::Snapshot);
        assert_eq!((r.rows, r.cols), (24, 80));
        assert_eq!(r.offset, "\x1b[?1049hHELLO".len() as u64);
        assert!(r.bytes.starts_with(b"\x1b[?1049h"), "must re-enter alt screen before the redraw: {:?}", r.bytes);
    }

    #[test]
    fn snapshot_writes_styled_blanks_as_literal_spaces() {
        let s = screens();
        resize(&s, 1, 3, 12);
        let input = b"\x1b[?1049h\x1b[2;3H\x1b[48;2;10;20;30m\x1b[5X";
        feed(&s, 1, 0, input);

        let r = s.get(1, None);
        assert!(r.bytes.windows(5).any(|window| window == b"     "));

        let mut restored = vt100::Parser::new(3, 12, 0);
        restored.process(&r.bytes);
        for col in 2..7 {
            assert_eq!(restored.screen().cell(1, col).unwrap().bgcolor(), vt100::Color::Rgb(10, 20, 30));
        }
        assert_eq!(restored.screen().cursor_position(), (1, 2));
        assert_eq!(restored.screen().bgcolor(), vt100::Color::Rgb(10, 20, 30));
    }

    #[test]
    fn since_offset_dedupes_an_unchanged_snapshot() {
        let s = screens();
        resize(&s, 1, 24, 80);
        feed(&s, 1, 0, b"\x1b[?1049hHELLO");
        let first = s.get(1, None);
        assert!(!first.bytes.is_empty());
        let second = s.get(1, Some(first.offset));
        assert_eq!(second.mode, ScreenMode::Snapshot);
        assert_eq!(second.offset, first.offset);
        assert!(second.bytes.is_empty());
    }

    #[test]
    fn resize_updates_the_tracked_size_in_place() {
        let s = screens();
        resize(&s, 1, 24, 80);
        resize(&s, 1, 30, 100);
        let r = s.get(1, None);
        assert_eq!((r.rows, r.cols), (30, 100));
    }

    /// vt100 0.16.2 panicked here (patched in vendor/vt100): a shrink or an
    /// insert that pushes a wide character's second half off the row, then
    /// a write or an erase over what is left of it.
    #[test]
    fn wide_char_cut_by_a_shrink_does_not_panic() {
        for tail in [&b"\x1b[1;5Hx"[..], b"\x1b[1;5H\x1b[K"] {
            let s = screens();
            resize(&s, 1, 2, 6);
            feed(&s, 1, 0, "abcd中".as_bytes());
            resize(&s, 1, 2, 5);
            feed(&s, 1, 7, tail);
            assert!(!s.preview(1, 2, 5).is_empty());
        }
        let s = screens();
        resize(&s, 1, 2, 6);
        feed(&s, 1, 0, "abc中\x1b[1;1H\x1b[@\x1b[1;6Hx".as_bytes());
        assert!(!s.preview(1, 2, 6).is_empty());
    }

    #[test]
    fn a_poisoned_state_is_unavailable_and_stops_only_its_tracker() {
        let s = screens();
        for id in [1, 2] {
            resize(&s, id, 24, 80);
            feed(&s, id, 0, b"\x1b[?1049hHELLO");
        }
        let state = s.state(1).unwrap();
        let _ = std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("a parser bug");
        })
        .join();

        assert_eq!(s.get(1, None).mode, ScreenMode::Unavailable);
        assert!(s.preview(1, 24, 80).is_empty());
        assert!(s.on_data(1, 13, b"more").is_break());
        assert!(s.on_event(1, HolderEvent::Resized { rows: 30, cols: 80 }).is_break());

        assert_eq!(s.get(2, None).mode, ScreenMode::Snapshot);
        assert!(s.on_data(2, 13, b"more").is_continue());
    }

    #[test]
    fn agents_are_tracked_independently() {
        let s = screens();
        resize(&s, 1, 24, 80);
        feed(&s, 1, 0, b"\x1b[?1049hone");
        // Agent 2 has never been heard from.
        assert_eq!(s.get(2, None).mode, ScreenMode::Unavailable);
        assert_eq!(s.get(1, None).mode, ScreenMode::Snapshot);
    }

    #[test]
    fn preview_preserves_terminal_styles() {
        let s = screens();
        resize(&s, 1, 2, 20);
        feed(&s, 1, 0, b"plain \x1b[1;38;2;10;20;30mbright\x1b[0m");

        let lines = s.preview(1, 1, 20);
        assert_eq!(lines[0][0].text, "plain ");
        assert_eq!(lines[0][0].fg, PreviewColor::Default);
        assert_eq!(lines[0][1].text, "bright");
        assert_eq!(lines[0][1].fg, PreviewColor::Rgb([10, 20, 30]));
        assert!(lines[0][1].bold);
    }

    #[test]
    fn dump_is_none_before_any_state() {
        let s = screens();
        assert!(s.dump(1).is_none());
    }

    #[test]
    fn dump_never_enters_the_alternate_screen() {
        let s = screens();
        resize(&s, 1, 24, 80);
        feed(&s, 1, 0, b"\x1b[?1049hHELLO");
        let (rows, cols, bytes) = s.dump(1).unwrap();
        assert_eq!((rows, cols), (24, 80));
        assert!(!bytes.starts_with(b"\x1b[?1049h"), "a readback must not toggle the caller's own screen: {bytes:?}");

        let mut restored = vt100::Parser::new(24, 80, 0);
        restored.process(&bytes);
        assert_eq!(restored.screen().contents(), "HELLO");
    }

    #[test]
    fn dump_works_on_the_primary_screen_too() {
        let s = screens();
        resize(&s, 1, 24, 80);
        feed(&s, 1, 0, b"conversation line\r\n");
        let (_, _, bytes) = s.dump(1).unwrap();

        let mut restored = vt100::Parser::new(24, 80, 0);
        restored.process(&bytes);
        assert_eq!(restored.screen().contents().trim_end(), "conversation line");
    }
}
