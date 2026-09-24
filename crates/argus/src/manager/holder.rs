//! Talking to holders: launching them, following their events, and sending
//! them requests. Also the `by-name` symlinks that point at their sockets.

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use argus_proto::frame::{aio, ty};
use argus_proto::msg::{
    AgentStatus, ExitRecord, HOLDER_CAPABILITIES, HolderEvent, HolderReady, HolderRequest, HolderResponse, HolderSpec,
    RunRequest, SubscribeLevel,
};
use argus_proto::{HOLDER_PROTOCOL_VERSION, paths};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::log;

const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Launches `argus-holder` and waits for it to report the agent is running.
/// Returns `(holder_pid, agent_pid)`.
pub async fn start(holder_exe: &Path, id: u64, req: &RunRequest, command: Vec<String>) -> Result<(u32, u32)> {
    let dir = paths::agent_dir(id);
    paths::ensure_private_dir(&dir)?;
    let log_file = OpenOptions::new().create(true).append(true).open(dir.join("holder.log"))?;
    let spec = HolderSpec {
        id,
        command,
        cwd: req.cwd.clone(),
        env: req.env.clone(),
        rows: req.rows,
        cols: req.cols,
        socket: paths::holder_socket(id),
        state_dir: dir,
        manager_socket: paths::manager_socket(),
    };

    let mut child = tokio::process::Command::new(holder_exe)
        .args(["--id", &id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log_file)
        .spawn()
        .context("launching argus-holder")?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(&serde_json::to_vec(&spec)?).await?;
    drop(stdin);
    let stdout = child.stdout.take().expect("piped");
    // The holder forks and its parent exits at once; reap that parent.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    let mut lines = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(READY_TIMEOUT, lines.next_line())
        .await
        .context("argus-holder did not report ready in time")??
        .context("argus-holder exited without reporting")?;
    match serde_json::from_str(&line).context("bad ready message from argus-holder")? {
        HolderReady::Ready { holder_pid, agent_pid } => Ok((holder_pid, agent_pid)),
        HolderReady::Failed { message } => bail!(message),
    }
}

async fn connect(id: u64) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(paths::holder_socket(id)).await?;
    let hello = call(
        &mut stream,
        &HolderRequest::Hello { version: HOLDER_PROTOCOL_VERSION, capabilities: HOLDER_CAPABILITIES.to_vec() },
    )
    .await?;
    match hello {
        HolderResponse::Hello { version, .. } if version == HOLDER_PROTOCOL_VERSION => {}
        HolderResponse::Hello { version, .. } => {
            anyhow::bail!("holder speaks protocol v{version}, manager expects v{HOLDER_PROTOCOL_VERSION}")
        }
        other => anyhow::bail!("unexpected holder handshake reply: {other:?}"),
    }
    Ok(stream)
}

async fn call(stream: &mut UnixStream, req: &HolderRequest) -> Result<HolderResponse> {
    aio::write_json(stream, req).await?;
    loop {
        let Some((t, payload)) = aio::read_frame(stream).await? else { bail!("holder closed the connection") };
        if t == ty::CONTROL {
            return match serde_json::from_slice(&payload)? {
                HolderResponse::Error { message } => bail!(message),
                resp => Ok(resp),
            };
        }
    }
}

/// Passes the holder's events to `on_event` and returns the exit code once
/// the holder reports it, or `None` on EOF.
pub async fn follow(id: u64, on_event: impl Fn(HolderEvent)) -> Result<Option<i32>> {
    let mut stream = connect(id).await?;
    call(&mut stream, &HolderRequest::Subscribe { level: SubscribeLevel::Events, from_offset: None }).await?;
    while let Some((t, payload)) = aio::read_frame(&mut stream).await? {
        match t {
            ty::EXIT if payload.len() == 4 => return Ok(Some(i32::from_be_bytes(payload[..4].try_into().unwrap()))),
            // Events from a newer holder that this manager does not know are skipped.
            ty::CONTROL => {
                if let Ok(event) = serde_json::from_slice(&payload) {
                    on_event(event);
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Like [`follow`], but at [`SubscribeLevel::Output`]: also passes output
/// bytes to `on_data` as `(offset_of_first_byte, bytes)`, for the manager's
/// virtual-terminal tracking.
pub async fn follow_screen(
    id: u64,
    on_event: impl Fn(HolderEvent),
    mut on_data: impl FnMut(u64, &[u8]),
) -> Result<Option<i32>> {
    let mut stream = connect(id).await?;
    // `from_offset: Some(0)` backfills everything the ring buffer still has:
    // without it, a `Screens` tracker that subscribes after the agent's
    // first burst of output (plausible — it races the manager spawning this
    // task at all) would start from an empty, wrongly-not-in-alternate-
    // screen parser and never catch up.
    call(&mut stream, &HolderRequest::Subscribe { level: SubscribeLevel::Output, from_offset: Some(0) }).await?;
    while let Some((t, payload)) = aio::read_frame(&mut stream).await? {
        match t {
            ty::EXIT if payload.len() == 4 => return Ok(Some(i32::from_be_bytes(payload[..4].try_into().unwrap()))),
            ty::CONTROL => {
                if let Ok(event) = serde_json::from_slice(&payload) {
                    on_event(event);
                }
            }
            // Output subscribers get every DATA frame tagged with the offset
            // of its first byte (see argus-holder's `publish`).
            ty::DATA if payload.len() >= 8 => {
                let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
                on_data(offset, &payload[8..]);
            }
            ty::SKIPPED if payload.len() == 16 => {
                let to = u64::from_be_bytes(payload[8..16].try_into().unwrap());
                on_data(to, &[]); // Resyncs the tracked offset; the gap is lived with.
            }
            _ => {}
        }
    }
    Ok(None)
}

/// How many bytes of output the agent has produced so far.
pub async fn output_offset(id: u64) -> Result<u64> {
    match call(&mut connect(id).await?, &HolderRequest::Info).await? {
        HolderResponse::Info(info) => Ok(info.output_offset),
        other => bail!("unexpected reply to Info: {other:?}"),
    }
}

pub async fn signal(id: u64, signal: i32) -> Result<()> {
    call(&mut connect(id).await?, &HolderRequest::Signal { signal }).await?;
    Ok(())
}

pub async fn write(id: u64, text: String) -> Result<()> {
    call(&mut connect(id).await?, &HolderRequest::Write { text }).await?;
    Ok(())
}

/// Used when the holder is gone: its exit record says how the agent ended.
pub fn exit_from_disk(id: u64) -> (AgentStatus, Option<i32>, Option<u64>) {
    let path = paths::exit_record(&paths::agent_dir(id));
    match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<ExitRecord>(&b).ok()) {
        Some(rec) => (AgentStatus::Exited, Some(rec.code), Some(rec.exited_at)),
        None => (AgentStatus::Lost, None, None),
    }
}

/// Points `holders/by-name/<name>.sock` at the holder socket so attach works
/// by name without the manager. The holder never learns its name.
pub fn link_name(name: &str, id: u64) {
    let link = paths::name_socket(name);
    if let Some(dir) = link.parent()
        && let Err(e) = paths::ensure_private_dir(dir)
    {
        return log(&format!("creating {}: {e}", dir.display()));
    }
    let _ = fs::remove_file(&link);
    if let Err(e) = std::os::unix::fs::symlink(paths::holder_socket(id), &link) {
        log(&format!("linking {}: {e}", link.display()));
    }
}

pub fn unlink_name(name: &str) {
    let _ = fs::remove_file(paths::name_socket(name));
}
