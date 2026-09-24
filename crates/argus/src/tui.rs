//! `argus tui`: full-screen agent dashboards. `argus grid` is its first mode
//! — a live thumbnail grid of every agent's screen.
//!
//! One process takes over the whole terminal (like `htop`), the same as
//! `attach` does — nothing here nests a terminal inside another; it paints
//! standard widgets into whichever real terminal (or tmux pane) is running
//! it. The agent list is pushed by `Watch`, applied as it arrives; only the
//! screen previews are still a pull, one `ScreenPreview` request per visible
//! tile on its own tick, since `Watch` deliberately never carries output.

use std::collections::BTreeMap;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use anyhow::Result;
use argus_proto::msg::{AgentInfo, Capability, PreviewColor, PreviewLine, PreviewSpan, Request, Response};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::attach;
use crate::client::{Conn, PsOptions};
use crate::stream;

/// How often visible tiles get a fresh `ScreenPreview`. Unlike the agent
/// list (pushed by Watch, applied as it arrives), preview bytes are always a
/// pull — Watch deliberately never carries output.
const PREVIEW_TICK: Duration = Duration::from_millis(500);
/// Upper bound on how long one loop iteration blocks on keyboard input, so a
/// pending watch event never waits behind a slow poll to be drawn.
const INPUT_POLL: Duration = Duration::from_millis(100);

pub fn run(prefix: Option<String>, labels: Vec<(String, String)>) -> Result<()> {
    let opts = PsOptions { prefix, all: false, labels, json: false, watch: false };
    let mut conn = Conn::connect()?;
    if !conn.supports(Capability::StyledPreview) {
        anyhow::bail!("the running manager does not support styled previews; restart it with `argus manager restart`");
    }
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut conn, &opts);
    ratatui::restore();
    result
}

struct Tile {
    info: AgentInfo,
    lines: Vec<PreviewLine>,
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, conn: &mut Conn, opts: &PsOptions) -> Result<()> {
    let mut selected = 0usize;
    let mut tiles: Vec<Tile> = Vec::new();
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

        if last_tick.elapsed() >= PREVIEW_TICK {
            let area = terminal.size()?;
            tiles = refresh(conn, &table, opts, area.into()).unwrap_or_default();
            selected = selected.min(tiles.len().saturating_sub(1));
            last_tick = Instant::now();
        }
        terminal.draw(|frame| draw(frame, &tiles, selected))?;

        let timeout = INPUT_POLL.min(PREVIEW_TICK.saturating_sub(last_tick.elapsed()));
        if !event::poll(timeout)? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let cols = grid_cols(tiles.len());
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Left | KeyCode::Char('h') => selected = selected.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') if !tiles.is_empty() => {
                selected = (selected + 1).min(tiles.len() - 1);
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(cols),
            KeyCode::Down | KeyCode::Char('j') if !tiles.is_empty() => {
                selected = (selected + cols).min(tiles.len() - 1);
            }
            KeyCode::Enter => {
                if let Some(tile) = tiles.get(selected) {
                    let target = attach::Target {
                        socket: argus_proto::paths::holder_socket(tile.info.id),
                        name: tile.info.name.clone(),
                        id: Some(tile.info.id),
                    };
                    // Give the real terminal back to `attach` for the
                    // duration of the session, then reclaim it.
                    ratatui::restore();
                    let opts =
                        attach::Options { readonly: false, steal: false, replay: false, allow_clipboard_replay: false };
                    let _ = attach::attach(&target, opts);
                    *terminal = ratatui::init();
                    last_tick = Instant::now() - PREVIEW_TICK; // Refresh right away.
                }
            }
            _ => {}
        }
    }
}

/// One `ScreenPreview` per visible tile, sized to whatever the grid layout
/// works out to for the current terminal size. The agent list itself comes
/// from the watch table, not this connection: `ScreenPreview` is always a
/// pull, since Watch deliberately never carries output bytes.
fn refresh(conn: &mut Conn, table: &BTreeMap<u64, AgentInfo>, opts: &PsOptions, area: Rect) -> Result<Vec<Tile>> {
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

fn draw(frame: &mut Frame, tiles: &[Tile], selected: usize) {
    let area = frame.area();
    if tiles.is_empty() {
        let block = Block::default().borders(Borders::ALL).title(" argus grid — no running agents ");
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    }
    let cols = grid_cols(tiles.len());
    let rows = tiles.len().div_ceil(cols);
    let row_areas = Layout::default().direction(Direction::Vertical).constraints(row_constraints(rows)).split(area);

    for (r, row_area) in row_areas.iter().enumerate() {
        let start = r * cols;
        let n = (tiles.len() - start).min(cols);
        if n == 0 {
            continue;
        }
        let col_areas =
            Layout::default().direction(Direction::Horizontal).constraints(row_constraints(n)).split(*row_area);
        for (c, cell_area) in col_areas.iter().enumerate() {
            let idx = start + c;
            let tile = &tiles[idx];
            let title = format!(" {} · {} ", tile.info.name, tile.info.activity);
            let mut block = Block::default().borders(Borders::ALL).title(title);
            if idx == selected {
                block = block.border_style(Style::default().fg(Color::Cyan));
            }
            let text = Text::from(tile.lines.iter().map(render_line).collect::<Vec<_>>());
            frame.render_widget(Paragraph::new(text).block(block), *cell_area);
        }
    }
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
