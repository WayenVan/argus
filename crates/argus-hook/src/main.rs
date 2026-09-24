//! argus-hook: the command an agent runs for each hook event.
//!
//! Usage (written into the agent's hook configuration by argus):
//!     argus-hook <source>          e.g. `argus-hook claude`
//!
//! It reads the event JSON from stdin and forwards it unchanged to the
//! manager as a `Report`. It must never slow down or break the agent, so every
//! failure is silent: not started by argus, manager down, bad input — just exit.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use argus_proto::MANAGER_PROTOCOL_VERSION;
use argus_proto::frame;
use argus_proto::msg::MANAGER_CAPABILITIES;
use argus_proto::msg::Request;

fn main() {
    // Only agents started by argus carry these; anything else is not ours.
    let Some(agent_id) = std::env::var("ARGUS_AGENT_ID").ok().and_then(|v| v.parse().ok()) else { return };
    let Ok(socket) = std::env::var("ARGUS_SOCKET") else { return };
    let source = std::env::args().nth(1).unwrap_or_default();

    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let Ok(event) = serde_json::from_slice(&input) else { return };

    let Ok(mut stream) = UnixStream::connect(socket) else { return };
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    // Wait for the Hello reply so the manager is reading before we hang up.
    if frame::write_json(
        &mut stream,
        &Request::Hello { version: MANAGER_PROTOCOL_VERSION, capabilities: MANAGER_CAPABILITIES.to_vec() },
    )
    .is_err()
        || !matches!(frame::read_frame(&mut stream), Ok(Some(_)))
    {
        return;
    }
    let _ = frame::write_json(&mut stream, &Request::Report { agent_id, source, event });
    // Report is never answered; closing the socket ends the exchange.
}

/// Upper bound on how long a stuck manager can hold up one hook event.
const TIMEOUT: Duration = Duration::from_millis(500);
