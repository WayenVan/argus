//! `argus logs`: an agent's recent output, or its current screen.

use std::io::{self, IsTerminal, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};
use argus_proto::frame::{self, ty};
use argus_proto::msg::{
    AgentInfo, HOLDER_CAPABILITIES, HolderRequest, HolderResponse, Request, Response, SubscribeLevel,
};
use argus_proto::{HOLDER_PROTOCOL_VERSION, paths};

use crate::attach;
use crate::client::Conn;

/// Ends passive log rendering at a clean shell boundary. The allowlist means
/// only SGR state can remain. This is never written into redirected data.
const LOG_TERMINAL_END: &[u8] = b"\x1b[0m\r\n";

/// Prints recent output. Running agents are read from their holder (at most
/// the 1 MiB ring buffer); exited agents from the `output.log` it left.
pub fn logs(target: String, bytes: Option<u64>, follow: bool, raw: bool, screen: bool) -> Result<()> {
    if screen {
        return logs_screen(&target);
    }
    let agent = Conn::connect()?.find(&target)?;
    // A connect failure means it exited just now; its log is on disk.
    if agent.status.is_live()
        && let Ok(stream) = UnixStream::connect(paths::holder_socket(agent.id))
    {
        return logs_live(stream, &agent, bytes, follow, raw);
    }
    let path = paths::output_log(&paths::agent_dir(agent.id));
    let data = std::fs::read(&path).with_context(|| format!("no output recorded for {}", agent.name))?;
    let start = bytes.map_or(0, |n| data.len().saturating_sub(n as usize));
    let output = if raw { data[start..].to_vec() } else { argus_proto::ansi::filter_logs(&data[start..]) };
    let restore_terminal = !raw && io::stdout().is_terminal();
    let mut out = io::stdout().lock();
    out.write_all(&output)?;
    if restore_terminal {
        out.write_all(LOG_TERMINAL_END)?;
    }
    out.flush()?;
    Ok(())
}

/// The manager's virtual-terminal rendering of the agent's current screen: a
/// point-in-time snapshot of a moment, not a stream, so it takes its own path
/// through the manager rather than the holder ring buffer `logs` otherwise
/// reads. The bytes are entirely manager-synthesized (the same escape-code
/// generator `attach` restore uses), never a verbatim copy of agent output,
/// so this stays safe to print without the `LogFilter` allowlist.
fn logs_screen(target: &str) -> Result<()> {
    let mut conn = Conn::connect()?;
    let Response::ScreenDump { bytes, .. } = conn.request(&Request::ScreenDump { target: target.to_string() })? else {
        bail!("unexpected reply to ScreenDump");
    };
    let restore_terminal = io::stdout().is_terminal();
    let mut out = io::stdout().lock();
    out.write_all(&bytes)?;
    if restore_terminal {
        out.write_all(LOG_TERMINAL_END)?;
    }
    out.flush()?;
    Ok(())
}

fn logs_live(mut stream: UnixStream, agent: &AgentInfo, bytes: Option<u64>, follow: bool, raw: bool) -> Result<()> {
    let restore_terminal = !raw && io::stdout().is_terminal();
    let hello = attach::call(
        &mut stream,
        &HolderRequest::Hello { version: HOLDER_PROTOCOL_VERSION, capabilities: HOLDER_CAPABILITIES.to_vec() },
    )?;
    attach::validate_holder_hello(hello)?;
    let HolderResponse::Info(info) = attach::call(&mut stream, &HolderRequest::Info)? else {
        bail!("unexpected reply to Info");
    };
    let end = info.output_offset;
    let from = bytes.map_or(0, |n| end.saturating_sub(n));
    if !follow && from >= end {
        return Ok(());
    }
    let subscribe = HolderRequest::Subscribe { level: SubscribeLevel::Output, from_offset: Some(from) };
    attach::call(&mut stream, &subscribe)?;

    let mut out = io::stdout().lock();
    let mut log_filter = (!raw).then(argus_proto::ansi::LogFilter::default);
    while let Some((t, payload)) = frame::read_frame(&mut stream)? {
        match t {
            ty::DATA if payload.len() >= 8 => {
                let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
                let data = &payload[8..];
                if follow {
                    write_log_bytes(&mut out, &mut log_filter, data)?;
                    out.flush()?;
                    continue;
                }
                // Without --follow, stop at the end offset seen when we started.
                let keep = end.saturating_sub(offset).min(data.len() as u64) as usize;
                write_log_bytes(&mut out, &mut log_filter, &data[..keep])?;
                if offset + data.len() as u64 >= end {
                    break;
                }
            }
            ty::SKIPPED => eprintln!("[argus] output skipped: this terminal fell behind"),
            ty::EXIT if payload.len() == 4 => {
                out.flush()?;
                let code = i32::from_be_bytes(payload[..4].try_into().unwrap());
                eprintln!("[{} exited with code {code}]", agent.name);
                break;
            }
            _ => {}
        }
    }
    if let Some(filter) = &mut log_filter {
        let mut tail = Vec::new();
        filter.finish(&mut tail);
        out.write_all(&tail)?;
    }
    if restore_terminal {
        out.write_all(LOG_TERMINAL_END)?;
    }
    out.flush()?;
    Ok(())
}

fn write_log_bytes(
    out: &mut impl Write,
    filter: &mut Option<argus_proto::ansi::LogFilter>,
    bytes: &[u8],
) -> io::Result<()> {
    if let Some(filter) = filter {
        let mut safe = Vec::with_capacity(bytes.len());
        filter.write_filtered(bytes, &mut safe);
        out.write_all(&safe)
    } else {
        out.write_all(bytes)
    }
}
