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
use argus_proto::msg::{
    Activity, AgentInfo, Capability, PreviewColor, PreviewLine, PreviewSpan, Request, Response, RunRequest, now_secs,
};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::attach;
use crate::client::{self, Conn, PsOptions};
use crate::errors::CodedError;
use crate::theme::theme;
use crate::{Cli, Command, query, stream, term, tmux};

/// How often the visible screen preview(s) get refreshed. Unlike the agent
/// list (pushed by Watch, applied as it arrives), preview bytes are always a
/// pull — Watch deliberately never carries output.
const PREVIEW_TICK: Duration = Duration::from_millis(500);
/// Upper bound on how long one loop iteration blocks on keyboard input, so a
/// pending watch event never waits behind a slow poll to be drawn.
const INPUT_POLL: Duration = Duration::from_millis(100);
/// How long a one-shot status line (rename/kill/copy result) stays on screen.
const STATUS_TTL: Duration = Duration::from_secs(3);

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
    // Focus reports let the tree dim its selection while the window is in
    // the background; terminals that lack them just never send any.
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableFocusChange)?;
    let result = event_loop(&mut terminal, &mut conn, &opts, mode);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableFocusChange);
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

/// The Tree mode panes, in `Tab` order. Keys act on the focused one; the
/// agent list keeps its selection while another pane has focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Pane {
    #[default]
    Agents,
    Preview,
    Title,
    Recap,
}

impl Pane {
    const ALL: [Pane; 4] = [Pane::Agents, Pane::Preview, Pane::Title, Pane::Recap];

    fn next(self) -> Pane {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Pane {
        Self::ALL[(self as usize + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The pane `H`/`J`/`K`/`L` move to from this one: the agent list on the
    /// left, the other three stacked on the right. Moving right returns to
    /// `right`, the right-hand pane last focused.
    fn toward(self, key: char, right: Pane) -> Pane {
        match (key, self) {
            ('H', _) => Pane::Agents,
            ('L', Pane::Agents) => right,
            ('K', Pane::Title) => Pane::Preview,
            ('K', Pane::Recap) | ('J', Pane::Preview) => Pane::Title,
            ('J', Pane::Title) => Pane::Recap,
            _ => self,
        }
    }

    /// Whether `e` can spread it over the whole content area.
    fn zoomable(self) -> bool {
        matches!(self, Pane::Agents | Pane::Preview)
    }
}

#[derive(Default)]
struct TreeState {
    /// Group paths (e.g. `"company/frontend"`) currently collapsed.
    collapsed: HashSet<String>,
    selected: usize,
    /// The selected agent's whole current screen, cropped to the preview's
    /// width; the pane shows as much of its bottom as fits.
    preview: Vec<PreviewLine>,
    /// Whose screen `preview` is, so moving to another agent starts at its
    /// bottom again.
    preview_of: Option<u64>,
    /// Lines the preview is scrolled up from the bottom of the screen.
    preview_scroll: usize,
    focus: Pane,
    /// The right-hand pane last focused, where `L` returns; the preview
    /// until another is.
    right: Option<Pane>,
    /// The focused pane fills the whole content area.
    zoomed: bool,
    /// The terminal window lost focus: the selection is drawn faintly so it
    /// does not drown out the activity colors of the row under it.
    blurred: bool,
}

/// One flattened, orderable line of the tree: either a group heading or a
/// leaf agent. Groups always sort before agents at the same depth.
enum Row {
    Group { path: String, name: String, depth: usize, count: usize, expanded: bool },
    Agent { info: AgentInfo, depth: usize },
}

/// A modal covering the whole loop's key handling until it resolves. Shared
/// by Grid and Tree since both act on "whatever agent is selected right now".
#[derive(Default)]
enum Overlay {
    #[default]
    None,
    Edit {
        id: u64,
        kind: EditKind,
        input: String,
        cursor: usize,
    },
    KillConfirm {
        id: u64,
        name: String,
    },
    Jump {
        targets: Vec<tmux::JumpTarget>,
        selected: usize,
    },
    Details {
        id: u64,
        scroll: u16,
    },
    /// The input line is whatever would follow `argus run` on a command
    /// line; submitting parses it with the same clap definition `argus run`
    /// itself uses (see `submit_new_agent`), so any flag it accepts here.
    /// `error` holds a local (pre-send) parse problem, shown inline instead
    /// of closing the overlay, so a typo doesn't lose what was typed.
    NewAgent {
        input: String,
        cursor: usize,
        error: Option<String>,
    },
}

/// What a text-edit overlay's input means once submitted: `Rename` replaces
/// the agent's whole path-shaped name; `Move` replaces just the group,
/// keeping the leaf (an empty input ungroups it), the same as `argus mv`.
#[derive(Clone, Copy)]
enum EditKind {
    Rename,
    Move,
}

impl EditKind {
    fn title(self) -> &'static str {
        match self {
            EditKind::Rename => " rename (enter confirm · esc cancel) ",
            EditKind::Move => " move to group (enter confirm · esc cancel) ",
        }
    }
}

/// The agent under the cursor in whichever mode is on screen, or `None` when
/// nothing is selected or a Tree group heading is (r/x/c are agent-only).
fn selected_agent<'a>(mode: Mode, grid: &'a GridState, rows: &'a [Row], tree_selected: usize) -> Option<&'a AgentInfo> {
    match mode {
        Mode::Grid => grid.tiles.get(grid.selected).map(|t| &t.info),
        Mode::Tree => match rows.get(tree_selected) {
            Some(Row::Agent { info, .. }) => Some(info),
            _ => None,
        },
    }
}

fn attachment_hint(info: &AgentInfo) -> String {
    if info.attached == 0 {
        String::new()
    } else {
        format!(" · attached {} · tmux {}", info.attached, info.tmux_locations.len())
    }
}

/// Count panes in this dashboard's tmux server. The actual pane is checked
/// again when `o` is pressed, since it can disappear between watch updates.
fn jumpable_count(info: &AgentInfo, socket: Option<&str>) -> usize {
    let Some(socket) = socket else { return 0 };
    let mut panes = HashSet::new();
    for location in &info.tmux_locations {
        if location.socket == socket {
            panes.insert(location.pane.as_str());
        }
    }
    panes.len()
}

fn jump_marker(info: &AgentInfo, socket: Option<&str>) -> Option<Span<'static>> {
    let count = jumpable_count(info, socket);
    (count > 0).then(|| {
        let label = if count == 1 { " ↗".to_string() } else { format!(" ↗{count}") };
        Span::styled(label, Style::default().fg(theme().lavender))
    })
}

/// Byte offset of the `char_idx`-th character, for editing a `String` by
/// character position (input is short, so a linear scan is fine).
fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

/// The group the cursor is "inside" right now, to prefill `a`'s `--in`: the
/// selected group's own path, or the selected agent's group. `None` in Grid
/// (it has no notion of a current group) or when nothing is selected.
fn current_group(mode: Mode, rows: &[Row], tree_selected: usize) -> Option<String> {
    match mode {
        Mode::Grid => None,
        Mode::Tree => match rows.get(tree_selected)? {
            Row::Group { path, .. } => Some(path.clone()),
            Row::Agent { info, .. } => info.name.rsplit_once('/').map(|(g, _)| g.to_string()),
        },
    }
}

/// Splits a typed command line into argv the way a shell would: single quotes
/// are literal, double quotes allow `\"`/`\\`/`\$` escapes, and a bare `\`
/// escapes the next character outside quotes. Just enough to let `-l
/// msg="hello world"` or similar work; not a full shell grammar.
fn shell_split(input: &str) -> Result<Vec<String>, String> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut have_token = false;
    let mut quote = Quote::None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Quote::None => match c {
                ' ' | '\t' => {
                    if have_token {
                        tokens.push(std::mem::take(&mut cur));
                        have_token = false;
                    }
                }
                '\'' => {
                    quote = Quote::Single;
                    have_token = true;
                }
                '"' => {
                    quote = Quote::Double;
                    have_token = true;
                }
                '\\' => match chars.next() {
                    Some(next) => {
                        cur.push(next);
                        have_token = true;
                    }
                    None => return Err("trailing backslash".to_string()),
                },
                _ => {
                    cur.push(c);
                    have_token = true;
                }
            },
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    cur.push(c);
                }
            }
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '\\' if matches!(chars.peek(), Some('"' | '\\' | '$')) => {
                    cur.push(chars.next().expect("peeked"));
                }
                _ => cur.push(c),
            },
        }
    }
    if quote != Quote::None {
        return Err("unterminated quote".to_string());
    }
    if have_token {
        tokens.push(cur);
    }
    Ok(tokens)
}

/// The first line of a (possibly multi-line) clap error, compact enough for
/// the overlay's one-line error slot.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim_end().to_string()
}

/// Parses `input` the same way `argus run <input>` would, and — if that
/// succeeds — creates the agent detached (never attaches from inside the
/// TUI; `Enter` on the resulting tile does that already). `Err` means the
/// problem never reached the manager (bad shell quoting, a bad flag, a
/// nonexistent `--cwd`) and the overlay should stay open so it can be fixed;
/// `Ok` covers both success and a manager-side rejection (e.g. name taken),
/// since either way the overlay is done and the result belongs in the
/// status line.
fn submit_new_agent(conn: &mut Conn, input: &str, area: Rect) -> Result<String, String> {
    let tokens = shell_split(input)?;
    if tokens.is_empty() {
        return Err("enter a command, e.g. `claude`".to_string());
    }
    let mut argv = vec!["argus".to_string(), "run".to_string()];
    argv.extend(tokens);
    let cli = Cli::try_parse_from(argv).map_err(|e| first_line(&e.to_string()))?;
    let Command::Run { name, group, cwd, label, kind, program, args, .. } = cli.command else {
        return Err("internal error: expected a run command".to_string());
    };
    let cwd = match cwd {
        Some(dir) => std::fs::canonicalize(&dir).map_err(|_| format!("no such directory: {}", dir.display()))?,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let group = group.or_else(|| std::env::var("ARGUS_GROUP").ok()).filter(|g| !g.is_empty());
    let mut command = vec![program];
    command.extend(args);
    let req = RunRequest {
        command,
        name,
        group,
        cwd: cwd.to_string_lossy().into_owned(),
        env: std::env::vars().collect(),
        rows: area.height,
        cols: area.width,
        labels: label.into_iter().collect(),
        kind,
        colors: term::profile().colors.clone(),
    };
    Ok(match conn.request(&Request::Run(req)) {
        Ok(Response::Agent { agent, warnings }) => match warnings.first() {
            Some(w) => format!("created {} ({w})", agent.name),
            None => format!("created {}", agent.name),
        },
        Ok(_) => "created".to_string(),
        Err(e) => format!("create failed: {e}"),
    })
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    conn: &mut Conn,
    opts: &PsOptions,
    mut mode: Mode,
) -> Result<()> {
    let mut grid = GridState::default();
    let mut tree = TreeState::default();
    let mut overlay = Overlay::None;
    let mut status: Option<(String, Instant)> = None;
    let jump_socket = tmux::current_location().map(|location| location.socket);
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
            let selected_id = match rows.get(tree.selected) {
                Some(Row::Agent { info, .. }) => Some(info.id),
                _ => None,
            };
            if selected_id != tree.preview_of {
                tree.preview_of = selected_id;
                tree.preview.clear();
                tree.preview_scroll = 0;
                last_tick = Instant::now() - PREVIEW_TICK; // Show the new agent's screen right away.
            }
        }

        let area: Rect = terminal.size()?.into();
        let content_area = dashboard_content_rect(area);
        if last_tick.elapsed() >= PREVIEW_TICK {
            let refreshed = match mode {
                Mode::Grid => refresh_grid(conn, &table, opts, content_area).map(|tiles| grid.tiles = tiles),
                Mode::Tree if tree.zoomed && tree.focus == Pane::Agents => Ok(()),
                Mode::Tree => refresh_tree_preview(conn, &rows, tree.selected, tree_preview_rect(content_area, &tree))
                    .map(|preview| tree.preview = preview),
            };
            if let Err(e) = refreshed {
                grid.tiles.clear();
                tree.preview.clear();
                // Not a refusal: the connection itself is gone (a manager
                // restart). The watch reconnects on its own; this one has to
                // be replaced here, or every later request fails with it.
                if e.downcast_ref::<CodedError>().is_none()
                    && let Ok(Some(fresh)) = Conn::open(false)
                {
                    *conn = fresh;
                }
            }
            grid.selected = grid.selected.min(grid.tiles.len().saturating_sub(1));
            last_tick = Instant::now();
        }

        let status_line = status.as_ref().filter(|(_, at)| at.elapsed() < STATUS_TTL).map(|(msg, _)| msg.as_str());
        let stale = conn.stale_manager().is_some();
        terminal.draw(|frame| {
            draw(frame, mode, &grid, &tree, &rows, &table, &overlay, status_line, jump_socket.as_deref(), stale)
        })?;

        let timeout = INPUT_POLL.min(PREVIEW_TICK.saturating_sub(last_tick.elapsed()));
        if !event::poll(timeout)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::FocusGained => {
                tree.blurred = false;
                continue;
            }
            Event::FocusLost => {
                tree.blurred = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if !matches!(overlay, Overlay::None) {
            handle_overlay_key(conn, &mut overlay, &mut status, key.code, area, &table);
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('H' | 'J' | 'K' | 'L') if mode == Mode::Tree => {
                let focus = match key.code {
                    KeyCode::Tab => tree.focus.next(),
                    KeyCode::BackTab => tree.focus.prev(),
                    KeyCode::Char(c) => tree.focus.toward(c, tree.right.unwrap_or(Pane::Preview)),
                    _ => tree.focus,
                };
                if focus != tree.focus {
                    if tree.zoomed {
                        tree.zoomed = false;
                        last_tick = Instant::now() - PREVIEW_TICK; // Back to the split's preview size.
                    }
                    tree.focus = focus;
                    if focus != Pane::Agents {
                        tree.right = Some(focus);
                    }
                }
            }
            KeyCode::Char(']') => {
                mode = mode.next();
                last_tick = Instant::now() - PREVIEW_TICK; // Refresh right away.
            }
            KeyCode::Char('[') => {
                mode = mode.prev();
                last_tick = Instant::now() - PREVIEW_TICK;
            }
            KeyCode::Char('1') => mode = Mode::Grid,
            KeyCode::Char('2') => mode = Mode::Tree,
            KeyCode::Char('i') => {
                if let Some(info) = selected_agent(mode, &grid, &rows, tree.selected) {
                    overlay = Overlay::Details { id: info.id, scroll: 0 };
                }
            }
            KeyCode::Char('r') => {
                if let Some(info) = selected_agent(mode, &grid, &rows, tree.selected) {
                    let input = info.name.clone();
                    let cursor = input.chars().count();
                    overlay = Overlay::Edit { id: info.id, kind: EditKind::Rename, input, cursor };
                }
            }
            KeyCode::Char('m') => {
                if let Some(info) = selected_agent(mode, &grid, &rows, tree.selected) {
                    let input = info.name.rsplit_once('/').map(|(g, _)| g.to_string()).unwrap_or_default();
                    let cursor = input.chars().count();
                    overlay = Overlay::Edit { id: info.id, kind: EditKind::Move, input, cursor };
                }
            }
            KeyCode::Char('a') => {
                let input = match current_group(mode, &rows, tree.selected) {
                    Some(group) => format!("--in {group} "),
                    None => String::new(),
                };
                let cursor = input.chars().count();
                overlay = Overlay::NewAgent { input, cursor, error: None };
            }
            KeyCode::Char('x') => {
                if let Some(info) = selected_agent(mode, &grid, &rows, tree.selected) {
                    overlay = Overlay::KillConfirm { id: info.id, name: info.name.clone() };
                }
            }
            KeyCode::Char('c') => {
                if let Some(info) = selected_agent(mode, &grid, &rows, tree.selected) {
                    let focus = if mode == Mode::Tree { tree.focus } else { Pane::Agents };
                    status = Some((copy_pane(conn, info, focus), Instant::now()));
                }
            }
            KeyCode::Char('o') => {
                if let Some(info) = selected_agent(mode, &grid, &rows, tree.selected) {
                    match tmux::choices(&info.tmux_locations) {
                        Ok(targets) if targets.len() == 1 => {
                            let message = match tmux::jump(&targets[0]) {
                                Ok(()) => format!("switched to {}", targets[0].label),
                                Err(e) => format!("jump failed: {e}"),
                            };
                            status = Some((message, Instant::now()));
                        }
                        Ok(targets) if !targets.is_empty() => overlay = Overlay::Jump { targets, selected: 0 },
                        Ok(_) => status = Some(("no reachable tmux pane for this agent".into(), Instant::now())),
                        Err(e) => status = Some((e, Instant::now())),
                    }
                }
            }
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
                                status = Some((attach_to(terminal, id, name)?, Instant::now()));
                                last_tick = Instant::now() - PREVIEW_TICK;
                            }
                        }
                        _ => {}
                    }
                }
                Mode::Tree => match (tree.focus, code) {
                    (focus, KeyCode::Char('e')) if focus.zoomable() => {
                        tree.zoomed = !tree.zoomed;
                        last_tick = Instant::now() - PREVIEW_TICK; // A preview sized for the new layout.
                    }
                    (Pane::Preview, KeyCode::Up | KeyCode::Char('k')) => {
                        tree.preview_scroll = (tree.preview_scroll + 1).min(preview_scroll_limit(&tree, content_area));
                    }
                    (Pane::Preview, KeyCode::Down | KeyCode::Char('j')) => {
                        tree.preview_scroll = tree.preview_scroll.saturating_sub(1);
                    }
                    (Pane::Preview, KeyCode::PageUp) => {
                        let page = preview_page_height(&tree, content_area);
                        tree.preview_scroll =
                            (tree.preview_scroll + page).min(preview_scroll_limit(&tree, content_area));
                    }
                    (Pane::Preview, KeyCode::PageDown) => {
                        tree.preview_scroll =
                            tree.preview_scroll.saturating_sub(preview_page_height(&tree, content_area));
                    }
                    (Pane::Agents, code) => match code {
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
                                status = Some((attach_to(terminal, id, name)?, Instant::now()));
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
                    (_, KeyCode::Enter) => {
                        if let Some(Row::Agent { info, .. }) = rows.get(tree.selected) {
                            let (id, name) = (info.id, info.name.clone());
                            status = Some((attach_to(terminal, id, name)?, Instant::now()));
                            last_tick = Instant::now() - PREVIEW_TICK;
                        }
                    }
                    _ => {}
                },
            },
        }
    }
}

/// Applies one keypress to an open Edit/KillConfirm overlay, issuing the
/// request and leaving a status line behind once it resolves. Takes
/// `overlay` by value (via `mem::take`) rather than matching through the
/// `&mut` so the request call and the reassignment aren't fighting over the
/// same borrow.
fn handle_overlay_key(
    conn: &mut Conn,
    overlay: &mut Overlay,
    status: &mut Option<(String, Instant)>,
    code: KeyCode,
    area: Rect,
    table: &BTreeMap<u64, AgentInfo>,
) {
    *overlay = match std::mem::take(overlay) {
        Overlay::Edit { id, kind, mut input, mut cursor } => match code {
            KeyCode::Esc => Overlay::None,
            KeyCode::Enter => {
                let msg = match kind {
                    EditKind::Rename => rename_status(conn, id, &input),
                    EditKind::Move => move_status(conn, id, &input),
                };
                *status = Some((msg, Instant::now()));
                Overlay::None
            }
            _ => {
                edit_text(&mut input, &mut cursor, code);
                Overlay::Edit { id, kind, input, cursor }
            }
        },
        Overlay::KillConfirm { id, name } => match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                *status = Some((kill_status(conn, id, &name), Instant::now()));
                Overlay::None
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Overlay::None,
            _ => Overlay::KillConfirm { id, name },
        },
        Overlay::Jump { targets, mut selected } => match code {
            KeyCode::Esc => Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
                Overlay::Jump { targets, selected }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(targets.len().saturating_sub(1));
                Overlay::Jump { targets, selected }
            }
            KeyCode::Enter => {
                if let Some(target) = targets.get(selected) {
                    let message = match tmux::jump(target) {
                        Ok(()) => format!("switched to {}", target.label),
                        Err(e) => format!("jump failed: {e}"),
                    };
                    *status = Some((message, Instant::now()));
                }
                Overlay::None
            }
            _ => Overlay::Jump { targets, selected },
        },
        Overlay::Details { id, mut scroll } => match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i') => Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => {
                scroll = scroll.saturating_sub(1);
                Overlay::Details { id, scroll }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = table.get(&id).map_or(0, |info| details_scroll_limit(info, area));
                scroll = scroll.saturating_add(1).min(max);
                Overlay::Details { id, scroll }
            }
            KeyCode::PageUp => Overlay::Details { id, scroll: scroll.saturating_sub(details_page_height(area)) },
            KeyCode::PageDown => {
                let max = table.get(&id).map_or(0, |info| details_scroll_limit(info, area));
                Overlay::Details { id, scroll: scroll.saturating_add(details_page_height(area)).min(max) }
            }
            KeyCode::Home => Overlay::Details { id, scroll: 0 },
            KeyCode::End => {
                let scroll = table.get(&id).map_or(0, |info| details_scroll_limit(info, area));
                Overlay::Details { id, scroll }
            }
            _ => Overlay::Details { id, scroll },
        },
        Overlay::NewAgent { mut input, mut cursor, .. } => match code {
            KeyCode::Esc => Overlay::None,
            KeyCode::Enter => match submit_new_agent(conn, &input, area) {
                Ok(msg) => {
                    *status = Some((msg, Instant::now()));
                    Overlay::None
                }
                Err(e) => Overlay::NewAgent { input, cursor, error: Some(e) },
            },
            _ => {
                // Any edit clears a stale error rather than leaving it
                // pinned under text the user has since changed.
                edit_text(&mut input, &mut cursor, code);
                Overlay::NewAgent { input, cursor, error: None }
            }
        },
        Overlay::None => Overlay::None,
    };
}

/// Applies one line-editing keypress (typing, backspace, delete, arrows,
/// home/end) to `input`/`cursor`. Unrecognized keys are a no-op, so callers
/// can route everything they don't handle themselves straight through.
fn edit_text(input: &mut String, cursor: &mut usize, code: KeyCode) {
    match code {
        KeyCode::Backspace if *cursor > 0 => {
            let end = byte_index(input, *cursor);
            let start = byte_index(input, *cursor - 1);
            input.replace_range(start..end, "");
            *cursor -= 1;
        }
        KeyCode::Delete if *cursor < input.chars().count() => {
            let start = byte_index(input, *cursor);
            let end = byte_index(input, *cursor + 1);
            input.replace_range(start..end, "");
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(input.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = input.chars().count(),
        KeyCode::Char(c) => {
            let at = byte_index(input, *cursor);
            input.insert(at, c);
            *cursor += 1;
        }
        _ => {}
    }
}

fn rename_status(conn: &mut Conn, id: u64, name: &str) -> String {
    match conn.request(&Request::Rename { target: id.to_string(), name: name.to_string() }) {
        Ok(Response::Agent { agent, .. }) => format!("renamed to {}", agent.name),
        Ok(_) => "renamed".to_string(),
        Err(e) => format!("rename failed: {e}"),
    }
}

/// A trailing `/` tells the manager's `Rename` handler to keep the agent's
/// leaf and replace only its group — the same trick `argus mv` uses.
fn move_status(conn: &mut Conn, id: u64, group: &str) -> String {
    let name = format!("{}/", group.trim_matches('/'));
    match conn.request(&Request::Rename { target: id.to_string(), name }) {
        Ok(Response::Agent { agent, .. }) => format!("moved to {}", agent.name),
        Ok(_) => "moved".to_string(),
        Err(e) => format!("move failed: {e}"),
    }
}

fn kill_status(conn: &mut Conn, id: u64, name: &str) -> String {
    match conn.request(&Request::Kill { target: id.to_string(), signal: None }) {
        Ok(Response::Killed { .. }) => format!("killed {name}"),
        Ok(_) => format!("killed {name}"),
        Err(e) => format!("kill failed: {e}"),
    }
}

/// Gives `attach` the real terminal for the duration of the session, then
/// reclaims it. The alternate screen is kept for an agent on its own, so the
/// normal screen never flashes by; an agent on the normal screen is shown
/// there. Either way it is re-entered after. `terminal.draw` right after would paint over a stale frame, so the
/// caller resets its preview tick to refresh immediately. Returns how the
/// session ended for the footer: printing it would land on the normal screen
/// and pile up there until the TUI exits.
fn attach_to(terminal: &mut ratatui::DefaultTerminal, id: u64, name: String) -> Result<String> {
    let target = attach::Target { socket: argus_proto::paths::holder_socket(id), name, id: Some(id) };
    let opts = attach::Options {
        readonly: false,
        steal: false,
        replay: false,
        allow_clipboard_replay: false,
        shared_screen: true,
        alt_screen: false,
    };
    let ending = attach::session(&target, opts).unwrap_or_else(|e| format!("attach failed: {e}"));
    // The session's reset turned focus reports off; the user just came back
    // here, so this window has focus.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableFocusChange
    )?;
    terminal.clear()?;
    Ok(ending)
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
    let areas = grid_tile_rects(area, agents.len());
    let mut tiles = Vec::with_capacity(agents.len());
    for (info, cell) in agents.into_iter().zip(areas) {
        let lines = match conn.request(&Request::ScreenPreview {
            target: info.id.to_string(),
            rows: cell.height.saturating_sub(2),
            cols: cell.width.saturating_sub(2),
        })? {
            Response::ScreenPreview { lines } => lines,
            _ => vec![],
        };
        tiles.push(Tile { info, lines });
    }
    Ok(tiles)
}

fn draw_grid(frame: &mut Frame, area: Rect, tiles: &[Tile], selected: usize, blink: bool, jump_socket: Option<&str>) {
    if tiles.is_empty() {
        let block = panel(" no running agents ");
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    }
    for (idx, cell_area) in grid_tile_rects(area, tiles.len()).into_iter().enumerate() {
        let tile = &tiles[idx];
        let mut spans = vec![
            Span::styled(activity_symbol(&tile.info), Style::default().fg(activity_color(&tile.info, blink))),
            Span::raw(tile.info.name.clone()),
        ];
        if let Some(marker) = jump_marker(&tile.info, jump_socket) {
            spans.push(marker);
        }
        let activity = format!(" · {}", tile.info.activity);
        spans.push(if is_blocked(&tile.info) {
            Span::styled(activity, activity_text_style(&tile.info))
        } else {
            Span::raw(activity)
        });
        let title = Line::from(spans);
        let border_color = if idx == selected { theme().mauve } else { theme().surface2 };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(title);
        let text = preview_text(&tile.lines, cell_area.height.saturating_sub(2), 0);
        frame.render_widget(Paragraph::new(text).block(block), cell_area);
    }
}

fn grid_tile_rects(area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let cols = grid_cols(count);
    let rows = count.div_ceil(cols);
    Layout::vertical(row_constraints(rows))
        .split(area)
        .iter()
        .enumerate()
        .flat_map(|(row, row_area)| {
            let n = (count - row * cols).min(cols);
            Layout::horizontal(row_constraints(n)).split(*row_area).iter().copied().collect::<Vec<_>>()
        })
        .collect()
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

/// The preview box, at whatever size the current terminal works out to —
/// kept in sync with `draw_tree`'s own layout so the `ScreenPreview` request
/// is cropped to the width it will actually fill.
fn tree_preview_rect(area: Rect, tree: &TreeState) -> Rect {
    if tree.zoomed && tree.focus == Pane::Preview {
        return area;
    }
    let cols = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(area);
    let detail = tree_detail_rects(cols[1]);
    detail[0]
}

/// Lines of screen the preview box shows at once.
fn preview_page_height(tree: &TreeState, area: Rect) -> usize {
    usize::from(tree_preview_rect(area, tree).height.saturating_sub(2))
}

/// How far up the preview scrolls: to the top of the agent's screen.
fn preview_scroll_limit(tree: &TreeState, area: Rect) -> usize {
    tree.preview.len().saturating_sub(preview_page_height(tree, area))
}

/// What `c` copies from the focused pane: the agent's name from the list,
/// its whole current screen (uncropped) from the preview, or a label.
fn copy_pane(conn: &mut Conn, info: &AgentInfo, focus: Pane) -> String {
    let (what, text) = match focus {
        Pane::Agents => ("name", Some(info.name.clone())),
        Pane::Preview => match query::screen_text(conn, info.id) {
            Ok(text) if !text.is_empty() => ("screen", Some(text)),
            Ok(_) => ("screen", None),
            Err(e) => return format!("copy failed: {e}"),
        },
        Pane::Title => ("title", info.labels.get("title").cloned()),
        Pane::Recap => ("recap", info.labels.get("recap").cloned()),
    };
    let Some(text) = text.filter(|t| !t.is_empty()) else { return format!("{} has no {what} to copy", info.name) };
    match term::copy_to_clipboard(&text) {
        Ok(()) if focus == Pane::Agents => format!("copied {text}"),
        Ok(()) => format!("copied the {what} of {}", info.name),
        Err(e) => format!("copy failed: {e}"),
    }
}

fn tree_detail_rects(area: Rect) -> [Rect; 3] {
    let parts = Layout::vertical([Constraint::Percentage(50), Constraint::Length(3), Constraint::Min(3)]).split(area);
    [parts[0], parts[1], parts[2]]
}

fn refresh_tree_preview(conn: &mut Conn, rows: &[Row], selected: usize, area: Rect) -> Result<Vec<PreviewLine>> {
    let Some(Row::Agent { info, .. }) = rows.get(selected) else { return Ok(vec![]) };
    // Every row, so the pane can scroll through the whole screen.
    let cols = area.width.saturating_sub(2);
    match conn.request(&Request::ScreenPreview { target: info.id.to_string(), rows: u16::MAX, cols })? {
        Response::ScreenPreview { lines } => Ok(lines),
        _ => Ok(vec![]),
    }
}

fn draw_tree(frame: &mut Frame, area: Rect, rows: &[Row], tree: &TreeState, blink: bool, jump_socket: Option<&str>) {
    match (tree.zoomed, tree.focus) {
        (true, Pane::Agents) => return draw_tree_list(frame, area, rows, tree, blink, jump_socket),
        (true, Pane::Preview) => return draw_detail_preview(frame, area, rows, tree),
        _ => {}
    }
    let cols = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(area);
    draw_tree_list(frame, cols[0], rows, tree, blink, jump_socket);

    let detail = tree_detail_rects(cols[1]);
    draw_detail_preview(frame, detail[0], rows, tree);
    draw_detail_label(frame, detail[1], rows, tree, Pane::Title);
    draw_detail_label(frame, detail[2], rows, tree, Pane::Recap);
}

fn draw_tree_list(
    frame: &mut Frame,
    area: Rect,
    rows: &[Row],
    tree: &TreeState,
    blink: bool,
    jump_socket: Option<&str>,
) {
    let TreeState { selected, blurred, zoomed, focus, .. } = *tree;
    let title = if zoomed { " agents · zoomed (e restore) " } else { " agents " };
    let block = focusable_panel(title, focus == Pane::Agents);
    if rows.is_empty() {
        frame.render_widget(Paragraph::new("no agents").block(block), area);
        return;
    }
    let highlight = if blurred { blurred_selection() } else { selection() };
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| tree_row_item(row, blink, jump_socket, (idx == selected).then_some(highlight)))
        .collect();
    // The selection is styled per row by `tree_row_item`; the state only
    // keeps it scrolled into view.
    let list = List::new(items).block(block).style(Style::default().fg(theme().text));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// `highlight` marks the selected row. On an agent the activity dot keeps its
/// own color and pulse on top of the highlight background.
/// A plain framed pane: a quiet border that leaves the accent to whatever
/// has focus.
fn panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme().surface2))
        .title_style(Style::default().fg(theme().text))
        .title(title)
}

/// A pane that `Tab` can focus: the focused one's border takes the accent.
fn focusable_panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let block = panel(title);
    if focused { block.border_style(Style::default().fg(theme().mauve)) } else { block }
}

/// The selected row of a list: a raised surface, not the accent, so the
/// colored dots and markers on it stay readable.
fn selection() -> Style {
    Style::default().bg(theme().surface1).fg(theme().text).add_modifier(Modifier::BOLD)
}

/// The selection while the terminal window is in the background: only a
/// faint surface, every span keeps its own color.
fn blurred_selection() -> Style {
    Style::default().bg(theme().surface0)
}

fn tree_row_item(row: &Row, blink: bool, jump_socket: Option<&str>, highlight: Option<Style>) -> ListItem<'static> {
    match row {
        Row::Group { name, depth, count, expanded, .. } => {
            let indent = "  ".repeat(*depth);
            let icon = if *expanded { "⌄" } else { "❯" };
            let icon_style = Style::default().fg(theme().overlay0).add_modifier(Modifier::BOLD);
            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(format!("{icon} "), icon_style),
                Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  ({count})"), Style::default().fg(theme().overlay0)),
            ]);
            match highlight {
                Some(style) => ListItem::new(line.patch_style(style)).style(style),
                None => ListItem::new(line),
            }
        }
        Row::Agent { info, depth } => {
            let indent = "  ".repeat(depth + 1);
            let leaf = info.name.rsplit('/').next().unwrap_or(&info.name).to_string();
            let mut spans = vec![
                Span::raw(indent),
                Span::styled(activity_symbol(info), Style::default().fg(activity_color(info, blink))),
                Span::raw(leaf),
            ];
            if let Some(marker) = jump_marker(info, jump_socket) {
                spans.push(marker);
            }
            spans.push(Span::styled(format!("  {}", info.activity), activity_text_style(info)));
            if let Some(style) = highlight {
                // Skip the indent and the dot; the item style below still
                // gives both the background.
                for span in &mut spans[2..] {
                    span.style = span.style.patch(style);
                }
                if is_blocked(info) {
                    // Keep the warning color while retaining the selection background.
                    spans.last_mut().unwrap().style = style.patch(activity_text_style(info));
                }
                return ListItem::new(Line::from(spans)).style(style);
            }
            ListItem::new(Line::from(spans))
        }
    }
}

fn draw_detail_preview(frame: &mut Frame, area: Rect, rows: &[Row], tree: &TreeState) {
    let height = area.height.saturating_sub(2);
    let scroll = tree.preview_scroll.min(tree.preview.len().saturating_sub(usize::from(height)));
    let mut title = match rows.get(tree.selected) {
        Some(Row::Agent { info, .. }) => format!(" {} · {}{} ", info.name, info.activity, attachment_hint(info)),
        Some(Row::Group { name, count, .. }) => format!(" {name} ({count}) — select an agent to preview its screen "),
        None => " no agents ".to_string(),
    };
    if scroll > 0 {
        title.push_str(&format!("· \u{2191}{scroll} "));
    }
    if tree.zoomed {
        title.push_str("· zoomed (e restore) ");
    }
    let block = focusable_panel(title, tree.focus == Pane::Preview);
    let text = preview_text(&tree.preview, height, scroll);
    frame.render_widget(Paragraph::new(text).block(block), area);
}

/// The `height` lines of `preview` that end `scroll` lines above its bottom,
/// padded above when the screen is shorter than the box.
fn preview_text(preview: &[PreviewLine], height: u16, scroll: usize) -> Text<'static> {
    let end = preview.len().saturating_sub(scroll);
    let start = end.saturating_sub(usize::from(height));
    let pad = usize::from(height).saturating_sub(end - start);
    Text::from(
        std::iter::repeat_with(|| Line::raw(""))
            .take(pad)
            .chain(preview[start..end].iter().map(render_line))
            .collect::<Vec<_>>(),
    )
}

/// One of the self-reported labels (`title`, `recap`) an agent is taught to
/// maintain via `SELF_LABEL_INSTRUCTIONS` — a plain readback of
/// `AgentInfo.labels`, not a separate data source.
fn draw_detail_label(frame: &mut Frame, area: Rect, rows: &[Row], tree: &TreeState, pane: Pane) {
    let key = if pane == Pane::Title { "title" } else { "recap" };
    let block = focusable_panel(format!(" {key} "), tree.focus == pane);
    let value = match rows.get(tree.selected) {
        Some(Row::Agent { info, .. }) => info.labels.get(key).map(String::as_str).unwrap_or("none"),
        _ => "none",
    };
    let paragraph = Paragraph::new(value).style(Style::default().fg(theme().subtext0)).block(block);
    let paragraph = if key == "recap" { paragraph.wrap(Wrap { trim: false }) } else { paragraph };
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Shared chrome
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame,
    mode: Mode,
    grid: &GridState,
    tree: &TreeState,
    rows: &[Row],
    table: &BTreeMap<u64, AgentInfo>,
    overlay: &Overlay,
    status: Option<&str>,
    jump_socket: Option<&str>,
    stale_manager: bool,
) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    let blink = blink_on();
    draw_tabs(frame, chunks[0], mode, stale_manager);
    match mode {
        Mode::Grid => draw_grid(frame, chunks[1], &grid.tiles, grid.selected, blink, jump_socket),
        Mode::Tree => draw_tree(frame, chunks[1], rows, tree, blink, jump_socket),
    }
    draw_footer(frame, chunks[2], mode, tree, status);
    draw_overlay(frame, area, overlay, table);
}

fn dashboard_content_rect(area: Rect) -> Rect {
    Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area)[1]
}

/// `stale_manager`: the manager is another build (see `Conn::stale_manager`),
/// flagged on the right for as long as that lasts.
fn draw_tabs(frame: &mut Frame, area: Rect, mode: Mode, stale_manager: bool) {
    let mut spans = vec![Span::styled(" argus ", Style::default().fg(theme().mauve).add_modifier(Modifier::BOLD))];
    for m in Mode::ALL {
        spans.push(Span::raw(" "));
        let style = if m == mode {
            Style::default().fg(theme().crust).bg(theme().mauve).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().subtext0)
        };
        spans.push(Span::styled(format!(" {} ", m.label()), style));
    }
    let bar = Style::default().bg(theme().mantle);
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
    if stale_manager {
        let notice =
            Line::styled("manager is a different build · argus manager restart ", Style::default().fg(theme().yellow));
        frame.render_widget(Paragraph::new(notice).alignment(Alignment::Right).style(bar), area);
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, mode: Mode, tree: &TreeState, status: Option<&str>) {
    // A fresh status line (rename/kill/copy result) briefly takes over the
    // footer instead of the hint, so the user notices it without a popup.
    if let Some(msg) = status {
        frame.render_widget(
            Paragraph::new(format!(" {msg}")).style(Style::default().fg(theme().crust).bg(theme().mauve)),
            area,
        );
        return;
    }
    const COMMON: &str = "i details   o jump   a new   r rename   m move group   x kill";
    const SWITCH: &str = "[/] grid\u{2194}tree   q quit";
    let zoom = if tree.zoomed { "e restore" } else { "e zoom" };
    let hint = match (mode, tree.focus) {
        (Mode::Grid, _) => {
            format!(" \u{2190}/\u{2192}/\u{2191}/\u{2193} move   enter attach   {COMMON}   c copy name   {SWITCH}")
        }
        (Mode::Tree, Pane::Agents) => format!(
            " \u{2191}/\u{2193} move   \u{2192} expand   \u{2190} collapse   {zoom}   enter attach/toggle   tab/HJKL pane   {COMMON}   c copy name   {SWITCH}"
        ),
        (Mode::Tree, Pane::Preview) => format!(
            " \u{2191}/\u{2193} scroll   PgUp/PgDn page   {zoom}   enter attach   tab/HJKL pane   c copy screen   {COMMON}   {SWITCH}"
        ),
        (Mode::Tree, pane) => {
            let key = if pane == Pane::Title { "title" } else { "recap" };
            format!(" c copy {key}   enter attach   tab/HJKL pane   {COMMON}   {SWITCH}")
        }
    };
    frame.render_widget(Paragraph::new(hint).style(Style::default().fg(theme().subtext0).bg(theme().mantle)), area);
}

/// The Rename input box or the Kill confirm popup, floating centered over
/// whatever `draw` already painted. No-op for `Overlay::None`.
fn draw_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay, table: &BTreeMap<u64, AgentInfo>) {
    match overlay {
        Overlay::None => {}
        Overlay::Edit { kind, input, cursor, .. } => {
            let rect = centered_rect(area, 46, 3);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme().mauve))
                .title(kind.title());
            frame.render_widget(Paragraph::new(cursor_line(input, *cursor)).block(block), rect);
        }
        Overlay::KillConfirm { name, .. } => {
            let rect = centered_rect(area, 46, 3);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme().red))
                .title(" kill? ");
            let text = format!("kill {name}?  y/enter confirm · n/esc cancel");
            frame.render_widget(Paragraph::new(text).style(Style::default().fg(theme().red)).block(block), rect);
        }
        Overlay::Jump { targets, selected } => {
            let height = (targets.len() as u16 + 2).min(area.height.saturating_sub(2));
            let rect = centered_rect(area, 60, height);
            frame.render_widget(Clear, rect);
            let items: Vec<ListItem> = targets.iter().map(|target| ListItem::new(target.label.clone())).collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" jump to tmux pane "),
                )
                .highlight_style(selection());
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, rect, &mut state);
        }
        Overlay::Details { id, scroll } => {
            let rect = details_rect(area);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme().mauve))
                .title(" agent details ")
                .title_bottom(" ↑/↓ scroll · PgUp/PgDn · K/Esc close ")
                .style(Style::default().fg(theme().text).bg(theme().mantle));
            let lines = table.get(id).map(details_lines).unwrap_or_else(|| {
                vec![Line::styled("  This agent is no longer available.", Style::default().fg(theme().subtext0))]
            });
            frame.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((*scroll, 0)),
                rect,
            );
        }
        Overlay::NewAgent { input, cursor, error } => {
            let mut lines = vec![
                cursor_line(input, *cursor),
                Line::styled(
                    "--in G  --name N  --kind K  -l K=V  PROGRAM [-- ARGS]  (detached)",
                    Style::default().fg(theme().overlay0),
                ),
            ];
            if let Some(err) = error {
                lines.push(Line::styled(err.clone(), Style::default().fg(theme().red)));
            }
            let height = lines.len() as u16 + 2;
            let rect = centered_rect(area, 64, height);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if error.is_some() { theme().red } else { theme().mauve }))
                .title(" new agent (enter run · esc cancel) ");
            frame.render_widget(Paragraph::new(lines).block(block), rect);
        }
    }
}

fn details_rect(area: Rect) -> Rect {
    centered_rect(area, area.width.saturating_sub(4).min(82), area.height.saturating_sub(2).min(24))
}

fn details_page_height(area: Rect) -> u16 {
    details_rect(area).height.saturating_sub(2).max(1)
}

fn details_scroll_limit(info: &AgentInfo, area: Rect) -> u16 {
    let width = usize::from(details_rect(area).width.saturating_sub(2).max(1));
    let height = usize::from(details_page_height(area));
    // Wrapped lines can break before the edge at a word boundary, so allow
    // one extra display row for each line that needs wrapping.
    let displayed: usize = details_lines(info)
        .iter()
        .map(|line| {
            let columns = line.width().max(1);
            columns.div_ceil(width) + usize::from(columns > width)
        })
        .sum();
    displayed.saturating_sub(height).min(usize::from(u16::MAX)) as u16
}

fn detail_field(lines: &mut Vec<Line<'static>>, label: &'static str, value: impl Into<String>) {
    lines.push(Line::from(vec![
        Span::styled(format!("  {label:<15}"), Style::default().fg(theme().subtext0)),
        Span::styled(value.into(), Style::default().fg(theme().text)),
    ]));
}

fn detail_section(lines: &mut Vec<Line<'static>>, title: &'static str) {
    lines.push(Line::raw(""));
    lines.push(Line::styled(format!("  {title}"), Style::default().fg(theme().lavender).add_modifier(Modifier::BOLD)));
}

fn details_lines(info: &AgentInfo) -> Vec<Line<'static>> {
    let now = now_secs();
    let ago = |t: u64| format!("{} ago", client::age(now.saturating_sub(t)));
    let status = match info.exit_code {
        Some(code) => format!("{} (code {code})", info.status.as_str()),
        None => info.status.as_str().to_string(),
    };
    let activity = match info.activity_since {
        Some(since) => format!("{} (since {})", info.activity, ago(since)),
        None => info.activity.to_string(),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("  {}", info.name), Style::default().fg(theme().mauve).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  #{}", info.id), Style::default().fg(theme().overlay0)),
        ]),
        Line::from(vec![
            Span::styled("  ● ", Style::default().fg(activity_color(info, true))),
            Span::styled(
                info.availability().to_string(),
                Style::default().fg(activity_color(info, true)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ·  {activity}"), Style::default().fg(theme().subtext0)),
        ]),
    ];
    detail_section(&mut lines, "IDENTITY");
    detail_field(&mut lines, "id", info.id.to_string());
    detail_field(&mut lines, "name", info.name.clone());
    detail_field(&mut lines, "group", info.name.rsplit_once('/').map_or("-", |(group, _)| group));
    detail_field(&mut lines, "kind", info.kind.clone());
    detail_section(&mut lines, "STATE");
    detail_field(&mut lines, "status", status);
    detail_field(&mut lines, "activity", activity);
    detail_field(&mut lines, "availability", info.availability().to_string());
    detail_field(&mut lines, "turns", info.turns.to_string());
    detail_field(&mut lines, "created", ago(info.created_at));
    if let Some(exited) = info.exited_at {
        detail_field(&mut lines, "exited", ago(exited));
    }
    detail_section(&mut lines, "PROCESS");
    detail_field(&mut lines, "cwd", info.cwd.clone());
    detail_field(&mut lines, "command", info.command.join(" "));
    if info.status.is_live() {
        detail_field(&mut lines, "attached", info.attached.to_string());
    }
    if let (Some(holder), Some(agent)) = (info.holder_pid, info.agent_pid) {
        detail_field(&mut lines, "pids", format!("holder {holder}, agent {agent}"));
    }
    if !info.labels.is_empty() {
        detail_section(&mut lines, "LABELS");
        for (key, value) in &info.labels {
            detail_field(&mut lines, "label", format!("{key}={value}"));
        }
    }
    lines
}

/// A single line of editable text with a reversed-video block marking the
/// cursor position, shared by every text-edit overlay.
fn cursor_line(input: &str, cursor: usize) -> Line<'static> {
    let before = input.chars().take(cursor).collect::<String>();
    let at = input.chars().nth(cursor);
    let after = input.chars().skip(cursor + 1).collect::<String>();
    Line::from(vec![
        Span::raw(before),
        Span::styled(
            at.map(String::from).unwrap_or_else(|| " ".into()),
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(after),
    ])
}

/// A `width`x`height` box centered in `area`, clamped so it never exceeds it.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect { x, y, width, height }
}

fn is_blocked(info: &AgentInfo) -> bool {
    info.status.is_live() && info.activity == Activity::Blocked
}

/// A blocked agent needs a distinct shape as well as color: idle shares its
/// yellow, but only blocked requires approval.
fn activity_symbol(info: &AgentInfo) -> &'static str {
    if is_blocked(info) { "◉ " } else { "● " }
}

fn activity_text_style(info: &AgentInfo) -> Style {
    if is_blocked(info) {
        Style::default().fg(theme().yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme().overlay0)
    }
}

/// Status-colored dot: gray once exited, otherwise colored by `activity` —
/// green (pulsing) while actively doing something, steady green once `done`,
/// steady yellow while it wants your input, red on `error`.
fn activity_color(info: &AgentInfo, blink: bool) -> Color {
    let t = theme();
    if !info.status.is_live() {
        return t.surface2;
    }
    match info.activity {
        Activity::Error => t.red,
        Activity::Done => t.green,
        Activity::Idle | Activity::Blocked => t.yellow,
        Activity::Quiet | Activity::Unknown => t.overlay0,
        // Actively running: pulse to draw the eye toward what's in motion.
        Activity::Working | Activity::Tool(_) | Activity::Busy => {
            if blink {
                t.green
            } else {
                t.overlay0
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

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        assert_eq!(shell_split("claude --model opus").unwrap(), vec!["claude", "--model", "opus"]);
        assert_eq!(shell_split(r#"-l msg="hello world" claude"#).unwrap(), vec!["-l", "msg=hello world", "claude"]);
        assert_eq!(shell_split("'a b' c").unwrap(), vec!["a b", "c"]);
        assert_eq!(shell_split(r"a\ b c").unwrap(), vec!["a b", "c"]);
        assert_eq!(shell_split("  ").unwrap(), Vec::<String>::new());
        assert!(shell_split("'unterminated").is_err());
        assert!(shell_split(r"trailing\").is_err());
    }

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
            turns: 0,
            attached: 0,
            tmux_locations: Vec::new(),
            pending_interactions: Vec::new(),
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

    #[test]
    fn preview_content_uses_dashboard_height_and_sits_at_bottom() {
        let screen = Rect::new(0, 0, 80, 24);
        let content = dashboard_content_rect(screen);
        assert_eq!(content.height, 22);
        let tiles = grid_tile_rects(content, 5);
        assert_eq!(tiles.len(), 5);
        assert!(tiles.iter().all(|rect| rect.bottom() <= content.bottom()));

        let line = vec![PreviewSpan {
            text: "latest".into(),
            fg: PreviewColor::Default,
            bg: PreviewColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }];
        let text = preview_text(&[line], 3, 0);
        assert!(text.lines[0].spans.is_empty());
        assert!(text.lines[1].spans.is_empty());
        assert_eq!(text.lines[2].spans[0].content, "latest");
    }

    fn text_line(text: &str) -> PreviewLine {
        vec![PreviewSpan {
            text: text.into(),
            fg: PreviewColor::Default,
            bg: PreviewColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }]
    }

    #[test]
    fn tab_cycles_the_tree_panes_both_ways() {
        assert_eq!(Pane::Agents.next(), Pane::Preview);
        assert_eq!(Pane::Recap.next(), Pane::Agents);
        assert_eq!(Pane::Agents.prev(), Pane::Recap);
        assert!(Pane::Preview.zoomable() && !Pane::Title.zoomable());
    }

    #[test]
    fn capital_hjkl_moves_between_panes_by_position() {
        let right = Pane::Preview;
        assert_eq!(Pane::Agents.toward('L', right), Pane::Preview);
        assert_eq!(Pane::Agents.toward('L', Pane::Recap), Pane::Recap, "back to the last right-hand pane");
        assert_eq!(Pane::Recap.toward('H', right), Pane::Agents);
        assert_eq!(Pane::Preview.toward('J', right), Pane::Title);
        assert_eq!(Pane::Title.toward('J', right), Pane::Recap);
        assert_eq!(Pane::Recap.toward('K', right), Pane::Title);
        assert_eq!(Pane::Title.toward('K', right), Pane::Preview);
        for (pane, key) in [(Pane::Preview, 'K'), (Pane::Recap, 'J'), (Pane::Agents, 'J'), (Pane::Title, 'L')] {
            assert_eq!(pane.toward(key, right), pane, "{pane:?} {key}");
        }
    }

    #[test]
    fn preview_scrolls_up_from_the_bottom_of_the_screen() {
        let lines: Vec<PreviewLine> = (0..5).map(|i| text_line(&format!("l{i}"))).collect();
        let shown = |scroll| -> Vec<String> {
            preview_text(&lines, 3, scroll).lines.iter().map(|l| l.spans[0].content.to_string()).collect()
        };
        assert_eq!(shown(0), ["l2", "l3", "l4"]);
        assert_eq!(shown(2), ["l0", "l1", "l2"]);

        let content = Rect::new(0, 0, 100, 12);
        let tree = TreeState { preview: lines.clone(), focus: Pane::Preview, zoomed: true, ..TreeState::default() };
        assert_eq!(tree_preview_rect(content, &tree), content);
        assert_eq!(preview_scroll_limit(&tree, content), 0, "a zoomed box that fits the screen does not scroll");
        let split = TreeState { zoomed: false, ..tree };
        assert_eq!(preview_page_height(&split, content), 4);
        assert_eq!(preview_scroll_limit(&split, content), 1);
    }

    #[test]
    fn the_focused_pane_has_an_accent_border() {
        let rows = [Row::Agent { info: agent(1, "a"), depth: 0 }];
        let tree = TreeState { focus: Pane::Title, ..TreeState::default() };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| draw_tree(frame, frame.area(), &rows, &tree, true, None)).unwrap();
        let buf = terminal.backend().buffer();
        let title = tree_detail_rects(
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(Rect::new(0, 0, 100, 24))[1],
        )[1];
        assert_eq!(buf[(title.x, title.y)].fg, theme().mauve);
        assert_eq!(buf[(0, 0)].fg, theme().surface2, "the agent list is not focused");
    }

    #[test]
    fn selected_agent_row_keeps_its_activity_dot_color() {
        let mut info = agent(1, "a");
        info.activity = "done".into();
        let row = Row::Agent { info, depth: 0 };
        let rows = std::slice::from_ref(&row);
        for blurred in [false, true] {
            let tree = TreeState { blurred, zoomed: true, ..TreeState::default() };
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 3)).unwrap();
            terminal.draw(|frame| draw_tree_list(frame, frame.area(), rows, &tree, true, None)).unwrap();
            let buf = terminal.backend().buffer();
            // Border, two-space indent, then the dot.
            let dot = &buf[(3, 1)];
            assert_eq!(dot.symbol(), "●");
            let expected_bg = if blurred { theme().surface0 } else { theme().surface1 };
            assert_eq!((dot.fg, dot.bg), (theme().green, expected_bg));
            assert_eq!(buf[(1, 1)].bg, expected_bg, "band starts at the row's edge");
            assert_eq!(buf[(18, 1)].bg, expected_bg, "band fills the row");
        }
    }

    #[test]
    fn blocked_agent_has_a_distinct_symbol_and_warning_text_when_selected() {
        let mut info = agent(1, "a");
        info.activity = "blocked".into();
        assert_eq!(activity_symbol(&info), "◉ ");
        assert_eq!(activity_text_style(&info).fg, Some(theme().yellow));

        let rows = [Row::Agent { info, depth: 0 }];
        let tree = TreeState { zoomed: true, ..TreeState::default() };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 3)).unwrap();
        terminal.draw(|frame| draw_tree_list(frame, frame.area(), &rows, &tree, true, None)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(3, 1)].symbol(), "◉");
        assert_eq!(buf[(3, 1)].fg, theme().yellow);
        assert_eq!(buf[(8, 1)].symbol(), "b");
        assert_eq!(buf[(8, 1)].fg, theme().yellow);
        assert_eq!(buf[(8, 1)].bg, theme().surface1);
        assert!(buf[(8, 1)].modifier.contains(Modifier::BOLD));

        let mut idle = agent(2, "b");
        idle.activity = "idle".into();
        assert_eq!(activity_symbol(&idle), "● ");
        assert_eq!(activity_text_style(&idle).fg, Some(theme().overlay0));
    }

    #[test]
    fn stale_manager_notice_shares_the_tab_bar() {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 1)).unwrap();
        terminal.draw(|frame| draw_tabs(frame, frame.area(), Mode::Tree, true)).unwrap();
        let line: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(line.starts_with(" argus "), "{line:?}");
        assert!(line.contains(" Tree "), "{line:?}");
        assert!(line.trim_end().ends_with("argus manager restart"), "{line:?}");
    }

    #[test]
    fn details_include_inspect_fields_and_labels() {
        let mut info = agent(156, "team/codex-1");
        info.kind = "codex".into();
        info.command = vec!["codex".into(), "--sandbox".into(), "workspace-write".into()];
        info.cwd = "/work/project".into();
        info.turns = 2;
        info.attached = 1;
        info.holder_pid = Some(123);
        info.agent_pid = Some(124);
        info.labels.insert("title".into(), "TUI details".into());
        let text = details_lines(&info)
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "156",
            "team/codex-1",
            "team",
            "codex",
            "running",
            "working",
            "active",
            "2",
            "/work/project",
            "codex --sandbox workspace-write",
            "1",
            "holder 123, agent 124",
            "title=TUI details",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
    }

    #[test]
    fn details_popup_renders_on_a_small_terminal() {
        let mut info = agent(156, "codex-1");
        info.labels.insert("recap".into(), "A long recap that should remain accessible when the popup is short".into());
        let agents = table(vec![info.clone()]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(42, 12)).unwrap();
        let overlay = Overlay::Details { id: info.id, scroll: 0 };
        terminal.draw(|frame| draw_overlay(frame, frame.area(), &overlay, &agents)).unwrap();
        let first: String = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
        assert!(first.contains("agent details"), "{first:?}");
        assert!(first.contains("codex-1"), "{first:?}");

        let overlay = Overlay::Details { id: info.id, scroll: details_scroll_limit(&info, Rect::new(0, 0, 42, 12)) };
        terminal.draw(|frame| draw_overlay(frame, frame.area(), &overlay, &agents)).unwrap();
        let last: String = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
        assert!(last.contains("recap="), "{last:?}");
    }
}
