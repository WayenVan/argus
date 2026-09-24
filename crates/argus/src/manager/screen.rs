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
use std::sync::{Arc, Mutex};

use argus_proto::msg::{HolderEvent, ScreenMode};

use super::holder;

struct State {
    parser: vt100::Parser,
    /// Running total of bytes fed into `parser`: the holder offset it has
    /// been brought up to date with.
    offset: u64,
}

pub struct ScreenReply {
    pub mode: ScreenMode,
    pub rows: u16,
    pub cols: u16,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

pub struct Screens {
    states: Mutex<HashMap<u64, State>>,
}

impl Screens {
    pub fn new() -> Arc<Screens> {
        Arc::new(Screens { states: Mutex::new(HashMap::new()) })
    }

    /// Subscribes to `id`'s output at `SubscribeLevel::Output` and keeps a
    /// virtual terminal in sync until the agent exits or the connection is
    /// lost. A separate holder connection from the lifecycle `follow` task,
    /// kept simple rather than threading screen state through it.
    pub fn track(self: &Arc<Self>, id: u64) {
        let screens = self.clone();
        tokio::spawn(async move {
            let on_event = |event: HolderEvent| screens.on_event(id, event);
            let on_data = |start: u64, bytes: &[u8]| screens.on_data(id, start, bytes);
            let _ = holder::follow_screen(id, on_event, on_data).await;
            screens.states.lock().unwrap().remove(&id);
        });
    }

    fn on_event(&self, id: u64, event: HolderEvent) {
        let HolderEvent::Resized { rows, cols } = event else { return };
        let mut states = self.states.lock().unwrap();
        match states.get_mut(&id) {
            Some(state) => state.parser.screen_mut().set_size(rows, cols),
            // The holder pushes the current size right after a subscribe
            // succeeds, so this is always the first event for a fresh state.
            None => {
                states.insert(id, State { parser: vt100::Parser::new(rows, cols, 0), offset: 0 });
            }
        }
    }

    fn on_data(&self, id: u64, start: u64, bytes: &[u8]) {
        let mut states = self.states.lock().unwrap();
        let Some(state) = states.get_mut(&id) else { return }; // No size yet.
        state.parser.process(bytes);
        state.offset = start + bytes.len() as u64;
    }

    /// How `id`'s screen should be restored. `since_offset` lets a caller
    /// that already has the screen at that offset skip the bytes.
    pub fn get(&self, id: u64, since_offset: Option<u64>) -> ScreenReply {
        let states = self.states.lock().unwrap();
        let Some(state) = states.get(&id) else {
            return ScreenReply { mode: ScreenMode::Unavailable, rows: 0, cols: 0, offset: 0, bytes: vec![] };
        };
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
        ScreenReply { mode: ScreenMode::Snapshot, rows, cols, offset: state.offset, bytes }
    }
}
