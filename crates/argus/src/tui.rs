//! `argus tui`: full-screen agent dashboards, one shared main loop switching
//! between modes — `Grid` (a live thumbnail grid, `argus grid`) and `Tree`
//! (a group-path tree with a live detail pane for the selected agent,
//! `argus tree`). The two share state and event handling; they are not
//! separate programs, and there is deliberately no generic widget framework
//! behind them — see the design doc's `argus tui` section for why.
//!
//! One process takes over the whole terminal (like `htop`), the same as
//! `attach` does — nothing here nests a terminal inside another; it paints
//! standard widgets into whichever real terminal (or tmux pane) is running
//! it. The agent list is pushed by `Watch`, applied as it arrives; only
//! screen previews are still a pull, one `ScreenPreview` request per tick,
//! since `Watch` deliberately never carries output.

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use anyhow::Result;
use argus_proto::msg::{AgentInfo, Capability, PreviewColor, PreviewLine, PreviewSpan, Request, Response};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::attach;
use crate::client::{Conn, PsOptions};
use crate::{stream, term};

/// How often the visible screen preview(s) get refreshed. Unlike the agent
/// list (pushed by Watch, applied as it arrives), preview bytes are always a
/// pull — Watch deliberately never carries output.
const PREVIEW_TICK: Duration = Duration::from_millis(500);
/// Upper bound on how long one loop iteration blocks on keyboard input, so a
/// pending watch event never waits behind a slow poll to be drawn.
const INPUT_POLL: Duration = Duration::from_millis(100);
/// The one accent color used for focus/selection everywhere, so the whole
/// app reads as one thing rather than a pile of differently-styled widgets.
const ACCENT: Color = Color::Cyan;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Grid,
    Tree,
}

impl Mode {
    const ALL: [Mode; 2] = [Mode::Grid, Mode::Tree];

    fn label(self) -> &'static str {
        match self {
            Mode::Grid => "Grid",
            Mode::Tree => "Tree",
        }
    }

    fn next(self) -> Mode {
        match self {
            Mode::Grid => Mode::Tree,
            Mode::Tree => Mode::Grid,
        }
    }

    /// Same as [`Self::next`] while there are only two modes; kept distinct
    /// so `[`/`]` mean what they look like once a third mode exists.
    fn prev(self) -> Mode {
        self.next()
    }
}

pub fn run(mode: Mode, prefix: Option<String>, labels: Vec<(String, String)>) -> Result<()> {
    let opts = PsOptions { prefix, all: false, labels, json: false, watch: false };
    let mut conn = Conn::connect()?;
    if !conn.supports(Capability::StyledPreview) {
        anyhow::bail!("the running manager does not support styled previews; restart it with `argus manager restart`");
    }
    // Before ratatui takes stdin and before any agent changes the terminal;
    // see `term::profile`.
    term::profile();
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut conn, &opts, mode);
    ratatui::restore();
    result
}

struct Tile {
    info: AgentInfo,
    lines: Vec<PreviewLine>,
}

#[derive(Default)]
struct GridState {
    tiles: Vec<Tile>,
    selected: usize,
}

#[derive(Default)]
struct TreeState {
    /// Group paths (e.g. `"company/frontend"`) currently collapsed.
    collapsed: HashSet<String>,
    selected: usize,
    preview: Vec<PreviewLine>,
}

/// One flattened, orderable line of the tree: either a group heading or a
/// leaf agent. Groups always sort before agents at the same depth.
enum Row {
    Group { path: String, name: String, depth: usize, count: usize, expanded: bool },
    Agent { info: AgentInfo, depth: usize },
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    conn: &mut Conn,
    opts: &PsOptions,
    mut mode: Mode,
) -> Result<()> {
    let mut grid = GridState::default();
    let mut tree = TreeState::default();
    // Due immediately, so the first frame is not empty.
    let mut last_tick = Instant::now() - PREVIEW_TICK;

    let (agents, mut rx) = stream::start_watch(None, opts.all)?;
    let mut table: BTreeMap<u64, AgentInfo> = agents.into_iter().map(|a| (a.id, a)).collect();

    loop {
        // Non-blocking: apply whatever the watch thread has queued up since
        // the last frame. A disconnected or ended watch means the manager
        // went away (restart or upgrade); reconnect and keep going.
        loop {
            match rx.try_recv() {
                Ok(Ok(Some(msg))) => stream::apply(&mut table, &msg),
                Ok(Ok(None)) | Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(200));
                    let (agents, new_rx) = stream::start_watch(None, opts.all)?;
                    table = agents.into_iter().map(|a| (a.id, a)).collect();
                    rx = new_rx;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        // Only Tree needs the flattened rows; building them walks the whole
        // table, so skip it while Grid is on screen.
        let rows = match mode {
            Mode::Tree => tree_rows(&table, opts, &tree.collapsed),
            Mode::Grid => Vec::new(),
        };
        if mode == Mode::Tree {
            tree.selected = tree.selected.min(rows.len().saturating_sub(1));
        }

        if last_tick.elapsed() >= PREVIEW_TICK {
            let area: Rect = terminal.size()?.into();
            match mode {
                Mode::Grid => {
                    grid.tiles = refresh_grid(conn, &table, opts, area).unwrap_or_default();
                    grid.selected = grid.selected.min(grid.tiles.len().saturating_sub(1));
                }
                Mode::Tree => {
                    tree.preview =
                        refresh_tree_preview(conn, &rows, tree.selected, detail_preview_rect(area)).unwrap_or_default();
                }
            }
            last_tick = Instant::now();
        }

        terminal.draw(|frame| draw(frame, mode, &grid, &tree, &rows))?;

        let timeout = INPUT_POLL.min(PREVIEW_TICK.saturating_sub(last_tick.elapsed()));
        if !event::poll(timeout)? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char(']') => {
                mode = mode.next();
                last_tick = Instant::now() - PREVIEW_TICK; // Refresh right away.
            }
            KeyCode::Char('[') => {
                mode = mode.prev();
                last_tick = Instant::now() - PREVIEW_TICK;
            }
            KeyCode::Char('1') => mode = Mode::Grid,
            KeyCode::Char('2') => mode = Mode::Tree,
            code => match mode {
                Mode::Grid => {
                    let cols = grid_cols(grid.tiles.len());
                    match code {
                        KeyCode::Left | KeyCode::Char('h') => grid.selected = grid.selected.saturating_sub(1),
                        KeyCode::Right | KeyCode::Char('l') if !grid.tiles.is_empty() => {
                            grid.selected = (grid.selected + 1).min(grid.tiles.len() - 1);
                        }
                        KeyCode::Up | KeyCode::Char('k') => grid.selected = grid.selected.saturating_sub(cols),
                        KeyCode::Down | KeyCode::Char('j') if !grid.tiles.is_empty() => {
                            grid.selected = (grid.selected + cols).min(grid.tiles.len() - 1);
                        }
                        KeyCode::Enter => {
                            if let Some(tile) = grid.tiles.get(grid.selected) {
                                let (id, name) = (tile.info.id, tile.info.name.clone());
                                attach_to(terminal, id, name)?;
                                last_tick = Instant::now() - PREVIEW_TICK;
                            }
                        }
                        _ => {}
                    }
                }
                Mode::Tree => match code {
                    KeyCode::Up | KeyCode::Char('k') => tree.selected = tree.selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') if !rows.is_empty() => {
                        tree.selected = (tree.selected + 1).min(rows.len() - 1);
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(Row::Group { path, .. }) = rows.get(tree.selected) {
                            tree.collapsed.remove(path);
                        }
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        if let Some(Row::Group { path, .. }) = rows.get(tree.selected) {
                            tree.collapsed.insert(path.clone());
                        }
                    }
                    KeyCode::Enter => match rows.get(tree.selected) {
                        Some(Row::Agent { info, .. }) => {
                            let (id, name) = (info.id, info.name.clone());
                            attach_to(terminal, id, name)?;
                            last_tick = Instant::now() - PREVIEW_TICK;
                        }
                        Some(Row::Group { path, expanded, .. }) => {
                            if *expanded {
                                tree.collapsed.insert(path.clone());
                            } else {
                                tree.collapsed.remove(path);
                            }
                        }
                        None => {}
                    },
                    _ => {}
                },
            },
        }
    }
}

/// Gives `attach` the real terminal for the duration of the session, then
/// reclaims it. `terminal.draw` right after would paint over a stale frame,
/// so the caller resets its preview tick to refresh immediately.
fn attach_to(terminal: &mut ratatui::DefaultTerminal, id: u64, name: String) -> Result<()> {
    let target = attach::Target { socket: argus_proto::paths::holder_socket(id), name, id: Some(id) };
    ratatui::restore();
    let opts = attach::Options { readonly: false, steal: false, replay: false, allow_clipboard_replay: false };
    let _ = attach::attach(&target, opts);
    *terminal = ratatui::init();
    Ok(())
}

// ---------------------------------------------------------------------------
// Grid mode
// ---------------------------------------------------------------------------

/// One `ScreenPreview` per visible tile, sized to whatever the grid layout
/// works out to for the current terminal size. The agent list itself comes
/// from the watch table, not this connection: `ScreenPreview` is always a
/// pull, since Watch deliberately never carries output bytes.
fn refresh_grid(conn: &mut Conn, table: &BTreeMap<u64, AgentInfo>, opts: &PsOptions, area: Rect) -> Result<Vec<Tile>> {
    let agents: Vec<AgentInfo> = table.values().filter(|a| opts.keeps(a)).cloned().collect();
    if agents.is_empty() {
        return Ok(vec![]);
    }
    let cols = grid_cols(agents.len());
    let rows = agents.len().div_ceil(cols);
    // Inside the border (2 cols) and title line (1 row) of each cell.
    let cell_cols = (area.width as usize / cols).saturating_sub(2).max(1) as u16;
    let cell_rows = (area.height as usize / rows).saturating_sub(3).max(1) as u16;

    let mut tiles = Vec::with_capacity(agents.len());
    for info in agents {
        let lines = match conn.request(&Request::ScreenPreview {
            target: info.id.to_string(),
            rows: cell_rows,
            cols: cell_cols,
        })? {
            Response::ScreenPreview { lines } => lines,
            _ => vec![],
        };
        tiles.push(Tile { info, lines });
    }
    Ok(tiles)
}

fn draw_grid(frame: &mut Frame, area: Rect, tiles: &[Tile], selected: usize, blink: bool) {
    if tiles.is_empty() {
        let block =
            Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" no running agents ");
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    }
    let cols = grid_cols(tiles.len());
    let rows = tiles.len().div_ceil(cols);
    let row_areas = Layout::vertical(row_constraints(rows)).split(area);

    for (r, row_area) in row_areas.iter().enumerate() {
        let start = r * cols;
        let n = (tiles.len() - start).min(cols);
        if n == 0 {
            continue;
        }
        let col_areas = Layout::horizontal(row_constraints(n)).split(*row_area);
        for (c, cell_area) in col_areas.iter().enumerate() {
            let idx = start + c;
            let tile = &tiles[idx];
            let title = Line::from(vec![
                Span::styled("● ", Style::default().fg(activity_color(&tile.info, blink))),
                Span::raw(format!("{} · {}", tile.info.name, tile.info.activity)),
            ]);
            let mut block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(title);
            if idx == selected {
                block = block.border_style(Style::default().fg(ACCENT));
            }
            let text = Text::from(tile.lines.iter().map(render_line).collect::<Vec<_>>());
            frame.render_widget(Paragraph::new(text).block(block), *cell_area);
        }
    }
}

// ---------------------------------------------------------------------------
// Tree mode
// ---------------------------------------------------------------------------

/// A group and its direct children, keyed by path segment; built fresh from
/// the watch table every frame Tree mode is on screen (cheap at the agent
/// counts this is for).
#[derive(Default)]
struct GroupNode {
    children: BTreeMap<String, GroupNode>,
    agents: Vec<AgentInfo>,
}

fn tree_rows(table: &BTreeMap<u64, AgentInfo>, opts: &PsOptions, collapsed: &HashSet<String>) -> Vec<Row> {
    let mut root = GroupNode::default();
    for info in table.values().filter(|a| opts.keeps(a)) {
        let mut parts: Vec<&str> = info.name.split('/').collect();
        parts.pop(); // The agent's own last segment is not a group.
        let mut node = &mut root;
        for part in parts {
            node = node.children.entry(part.to_string()).or_default();
        }
        node.agents.push(info.clone());
    }
    let mut rows = Vec::new();
    flatten_tree(&root, "", 0, collapsed, &mut rows);
    rows
}

fn flatten_tree(node: &GroupNode, prefix: &str, depth: usize, collapsed: &HashSet<String>, out: &mut Vec<Row>) {
    for (name, child) in &node.children {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let expanded = !collapsed.contains(&path);
        out.push(Row::Group { path: path.clone(), name: name.clone(), depth, count: count_agents(child), expanded });
        if expanded {
            flatten_tree(child, &path, depth + 1, collapsed, out);
        }
    }
    let mut agents: Vec<&AgentInfo> = node.agents.iter().collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    for info in agents {
        out.push(Row::Agent { info: info.clone(), depth });
    }
}

fn count_agents(node: &GroupNode) -> usize {
    node.agents.len() + node.children.values().map(count_agents).sum::<usize>()
}

/// The right-hand detail pane's preview box, at whatever size the current
/// terminal works out to — kept in sync with `draw_tree`'s own split so the
/// `ScreenPreview` request is sized for the box it will actually fill.
fn detail_preview_rect(area: Rect) -> Rect {
    let cols = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(area);
    let detail = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(cols[1]);
    detail[0]
}

fn refresh_tree_preview(conn: &mut Conn, rows: &[Row], selected: usize, area: Rect) -> Result<Vec<PreviewLine>> {
    let Some(Row::Agent { info, .. }) = rows.get(selected) else { return Ok(vec![]) };
    let cols = area.width.saturating_sub(2).max(1);
    let rows = area.height.saturating_sub(2).max(1);
    match conn.request(&Request::ScreenPreview { target: info.id.to_string(), rows, cols })? {
        Response::ScreenPreview { lines } => Ok(lines),
        _ => Ok(vec![]),
    }
}

fn draw_tree(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize, preview: &[PreviewLine], blink: bool) {
    let cols = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(area);
    draw_tree_list(frame, cols[0], rows, selected, blink);

    let detail =
        Layout::vertical([Constraint::Percentage(50), Constraint::Length(3), Constraint::Min(3)]).split(cols[1]);
    draw_detail_preview(frame, detail[0], rows, selected, preview);
    draw_detail_label(frame, detail[1], rows, selected, "title");
    draw_detail_label(frame, detail[2], rows, selected, "recap");
}

fn draw_tree_list(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize, blink: bool) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" agents ");
    if rows.is_empty() {
        frame.render_widget(Paragraph::new("no agents").block(block), area);
        return;
    }
    let items: Vec<ListItem> = rows.iter().map(|row| tree_row_item(row, blink)).collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn tree_row_item(row: &Row, blink: bool) -> ListItem<'static> {
    match row {
        Row::Group { name, depth, count, expanded, .. } => {
            let indent = "  ".repeat(*depth);
            let icon = if *expanded { "▾" } else { "▸" };
            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(format!("{icon} "), Style::default().fg(Color::DarkGray)),
                Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  ({count})"), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        }
        Row::Agent { info, depth } => {
            let indent = "  ".repeat(depth + 1);
            let leaf = info.name.rsplit('/').next().unwrap_or(&info.name).to_string();
            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled("● ", Style::default().fg(activity_color(info, blink))),
                Span::raw(format!("{leaf}  ")),
                Span::styled(info.activity.clone(), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        }
    }
}

fn draw_detail_preview(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize, preview: &[PreviewLine]) {
    let title = match rows.get(selected) {
        Some(Row::Agent { info, .. }) => format!(" {} · {} ", info.name, info.activity),
        Some(Row::Group { name, count, .. }) => format!(" {name} ({count}) — select an agent to preview its screen "),
        None => " no agents ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(title);
    let text = Text::from(preview.iter().map(render_line).collect::<Vec<_>>());
    frame.render_widget(Paragraph::new(text).block(block), area);
}

/// One of the self-reported labels (`title`, `recap`) an agent is taught to
/// maintain via `SELF_LABEL_INSTRUCTIONS` — a plain readback of
/// `AgentInfo.labels`, not a separate data source.
fn draw_detail_label(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize, key: &str) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(format!(" {key} "));
    let value = match rows.get(selected) {
        Some(Row::Agent { info, .. }) => info.labels.get(key).map(String::as_str).unwrap_or("none"),
        _ => "none",
    };
    let paragraph = Paragraph::new(value).style(Style::default().fg(Color::Gray)).block(block);
    let paragraph = if key == "recap" { paragraph.wrap(Wrap { trim: false }) } else { paragraph };
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Shared chrome
// ---------------------------------------------------------------------------

/// The muted band behind the top tab bar and bottom hint bar — chrome that
/// frames the content without boxing it in, the way a browser's tab strip or
/// a status bar reads as a bar without needing a drawn border around it.
const CHROME_BG: Color = Color::Indexed(236);

fn draw(frame: &mut Frame, mode: Mode, grid: &GridState, tree: &TreeState, rows: &[Row]) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    let blink = blink_on();
    draw_tabs(frame, chunks[0], mode);
    match mode {
        Mode::Grid => draw_grid(frame, chunks[1], &grid.tiles, grid.selected, blink),
        Mode::Tree => draw_tree(frame, chunks[1], rows, tree.selected, &tree.preview, blink),
    }
    draw_footer(frame, chunks[2], mode);
}

fn draw_tabs(frame: &mut Frame, area: Rect, mode: Mode) {
    let mut spans = vec![Span::styled(" argus ", Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD))];
    for m in Mode::ALL {
        spans.push(Span::raw(" "));
        let style = if m == mode {
            Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {} ", m.label()), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(CHROME_BG)), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, mode: Mode) {
    let hint = match mode {
        Mode::Grid => " \u{2190}/\u{2192}/\u{2191}/\u{2193} move   enter attach   [/]/tab switch   q quit",
        Mode::Tree => {
            " \u{2191}/\u{2193} move   \u{2192} expand   \u{2190} collapse   enter attach/toggle   [/]/tab switch   q quit"
        }
    };
    frame.render_widget(Paragraph::new(hint).style(Style::default().fg(Color::Gray).bg(CHROME_BG)), area);
}

/// Status-colored dot: gray once exited, otherwise colored by `activity` —
/// green (pulsing) while actively doing something, steady green once `done`,
/// steady yellow while it wants your input, red on `error`. Matched loosely
/// (`tool:<name>` falls through to the active/pulsing case) since new
/// activity strings are added on the driver side over time.
fn activity_color(info: &AgentInfo, blink: bool) -> Color {
    if !info.status.is_live() {
        return Color::DarkGray;
    }
    match info.activity.as_str() {
        "error" => Color::Red,
        "done" => Color::Green,
        "idle" | "blocked" => Color::Yellow,
        "quiet" | "unknown" => Color::DarkGray,
        // working / tool:<name> / busy: actively running, pulse to draw the
        // eye toward what's currently in motion.
        _ => {
            if blink {
                Color::Green
            } else {
                Color::DarkGray
            }
        }
    }
}

/// A ~600ms on/off pulse, derived from wall-clock time rather than kept as
/// loop state: the terminal's own `slow blink` SGR attribute is unreliable
/// (many emulators disable it), so the "actively running" dot is pulsed by
/// hand instead, redrawn every idle tick like the rest of the frame.
fn blink_on() -> bool {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    ms % 600 < 300
}

fn render_line(line: &PreviewLine) -> Line<'static> {
    Line::from(line.iter().map(render_span).collect::<Vec<_>>())
}

fn render_span(span: &PreviewSpan) -> Span<'static> {
    let mut modifiers = Modifier::empty();
    if span.bold {
        modifiers.insert(Modifier::BOLD);
    }
    if span.dim {
        modifiers.insert(Modifier::DIM);
    }
    if span.italic {
        modifiers.insert(Modifier::ITALIC);
    }
    if span.underline {
        modifiers.insert(Modifier::UNDERLINED);
    }
    if span.inverse {
        modifiers.insert(Modifier::REVERSED);
    }
    let style = Style::default().fg(render_color(span.fg)).bg(render_color(span.bg)).add_modifier(modifiers);
    Span::styled(span.text.clone(), style)
}

fn render_color(color: PreviewColor) -> Color {
    match color {
        PreviewColor::Default => Color::Reset,
        PreviewColor::Indexed(index) => Color::Indexed(index),
        PreviewColor::Rgb([r, g, b]) => Color::Rgb(r, g, b),
    }
}

fn row_constraints(n: usize) -> Vec<Constraint> {
    vec![Constraint::Ratio(1, n as u32); n]
}

/// Columns for a roughly-square grid of `n` tiles.
fn grid_cols(n: usize) -> usize {
    if n == 0 { 1 } else { (n as f64).sqrt().ceil() as usize }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::AgentStatus;

    fn agent(id: u64, name: &str) -> AgentInfo {
        AgentInfo {
            id,
            name: name.to_string(),
            kind: "bash".into(),
            command: vec!["bash".into()],
            cwd: "/".into(),
            created_at: 0,
            exited_at: None,
            holder_pid: None,
            agent_pid: None,
            status: AgentStatus::Running,
            exit_code: None,
            activity: "working".into(),
            activity_since: None,
            attached: 0,
            labels: BTreeMap::new(),
        }
    }

    fn table(agents: Vec<AgentInfo>) -> BTreeMap<u64, AgentInfo> {
        agents.into_iter().map(|a| (a.id, a)).collect()
    }

    fn opts() -> PsOptions {
        PsOptions { prefix: None, all: false, labels: vec![], json: false, watch: false }
    }

    #[test]
    fn nested_groups_flatten_depth_first_with_counts() {
        let table = table(vec![
            agent(1, "company/frontend/codex-1"),
            agent(2, "company/frontend/claude-1"),
            agent(3, "company/backend/codex-2"),
            agent(4, "solo"),
        ]);
        let rows = tree_rows(&table, &opts(), &HashSet::new());

        let labels: Vec<(&str, usize)> = rows
            .iter()
            .map(|r| match r {
                Row::Group { name, depth, .. } => (name.as_str(), *depth),
                Row::Agent { info, depth } => (info.name.rsplit('/').next().unwrap(), *depth),
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                ("company", 0),
                ("backend", 1),
                ("codex-2", 2),
                ("frontend", 1),
                ("claude-1", 2),
                ("codex-1", 2),
                ("solo", 0),
            ]
        );
        let Row::Group { count, .. } = &rows[0] else { panic!("expected a group") };
        assert_eq!(*count, 3);
    }

    #[test]
    fn collapsing_a_group_hides_its_descendants_but_keeps_the_row() {
        let table = table(vec![agent(1, "frontend/codex-1"), agent(2, "frontend/codex-2")]);
        let mut collapsed = HashSet::new();
        collapsed.insert("frontend".to_string());
        let rows = tree_rows(&table, &opts(), &collapsed);

        assert_eq!(rows.len(), 1);
        let Row::Group { expanded, count, .. } = &rows[0] else { panic!("expected a group") };
        assert!(!expanded);
        assert_eq!(*count, 2);
    }

    #[test]
    fn exited_agents_are_hidden_unless_all_is_set() {
        let mut exited = agent(1, "codex-1");
        exited.status = AgentStatus::Exited;
        let table = table(vec![exited, agent(2, "codex-2")]);

        let rows = tree_rows(&table, &opts(), &HashSet::new());
        assert_eq!(rows.len(), 1);

        let all = PsOptions { all: true, ..opts() };
        let rows = tree_rows(&table, &all, &HashSet::new());
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn grid_stays_roughly_square() {
        assert_eq!(grid_cols(0), 1);
        assert_eq!(grid_cols(1), 1);
        assert_eq!(grid_cols(2), 2);
        assert_eq!(grid_cols(4), 2);
        assert_eq!(grid_cols(5), 3);
        assert_eq!(grid_cols(9), 3);
        assert_eq!(grid_cols(10), 4);
    }
}
