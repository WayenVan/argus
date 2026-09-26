//! argus-hook: the command an agent runs for each hook event.
//!
//! Usage (written into the agent's hook configuration by argus):
//!     argus-hook <source>          e.g. `argus-hook claude`
//!
//! It reads the event JSON from stdin and forwards it unchanged to the
//! manager as a `Report`. A manager that answers reports may reply with text
//! for the agent, which goes to stdout, where the agent reads it as the hook's
//! answer (e.g. Claude's `{"decision":"block",…}` on `Stop`). It must never
//! slow down or break the agent, so every failure is silent: not started by
//! argus, manager down, bad input — just exit.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use argus_proto::MANAGER_PROTOCOL_VERSION;
use argus_proto::frame;
use argus_proto::msg::{Capability, Request, Response};
use argus_proto::text;

fn main() {
    // Only agents started by argus carry these; anything else is not ours.
    let Some(agent_id) = std::env::var("ARGUS_AGENT_ID").ok().and_then(|v| v.parse().ok()) else { return };
    let Ok(socket) = std::env::var("ARGUS_SOCKET") else { return };
    let source = std::env::args().nth(1).unwrap_or_default();

    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let Ok(mut event) = serde_json::from_slice(&input) else { return };
    // A huge pasted prompt must not push the event past the frame limit and
    // lose it; the manager keeps no more than this anyway.
    text::clip_event(&mut event);

    let Ok(mut stream) = UnixStream::connect(socket) else { return };
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    // Wait for the Hello reply so the manager is reading before we hang up.
    let hello = Request::Hello { version: MANAGER_PROTOCOL_VERSION, capabilities: Vec::new() };
    if frame::write_json(&mut stream, &hello).is_err() {
        return;
    }
    let Some(Response::Hello { capabilities, .. }) = read_response(&mut stream) else { return };
    let reply = capabilities.contains(&Capability::HookReply);
    if frame::write_json(&mut stream, &Request::Report { agent_id, source, event, reply }).is_err() || !reply {
        return;
    }
    if let Some(Response::HookReply { stdout: Some(text) }) = read_response(&mut stream) {
        print!("{text}");
    }
}

fn read_response(stream: &mut UnixStream) -> Option<Response> {
    let (_, payload) = frame::read_frame(stream).ok()??;
    serde_json::from_slice(&payload).ok()
}

/// Upper bound on how long a stuck manager can hold up one hook event.
const TIMEOUT: Duration = Duration::from_millis(500);
