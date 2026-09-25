//! Locate an attaching terminal in tmux, and jump from a dashboard to it.
//! Pane IDs identify a pane only within one tmux server. A source pane may be
//! visible in multiple tmux clients, so a jump is refused unless exactly one
//! client currently displays the dashboard pane.

use std::collections::HashSet;
use std::io;
use std::os::fd::AsFd;
use std::process::Command;

use argus_proto::msg::TmuxLocation;

#[derive(Clone)]
pub struct JumpTarget {
    pub location: TmuxLocation,
    pub label: String,
}

fn valid_pane(pane: &str) -> bool {
    pane.strip_prefix('%').is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn from_environment() -> Option<TmuxLocation> {
    let socket = std::env::var("TMUX").ok()?.split(',').next()?.to_string();
    let pane = std::env::var("TMUX_PANE").ok()?;
    if !socket.starts_with('/') || !valid_pane(&pane) {
        return None;
    }
    Some(TmuxLocation { socket, pane })
}

fn command(socket: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("tmux").arg("-S").arg(socket).args(args).output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string())
}

/// Only report a location when the process's actual stdin TTY is this pane's
/// TTY. Inherited or stale TMUX environment variables do not prove location.
pub fn current_location() -> Option<TmuxLocation> {
    let location = from_environment()?;
    let tty = nix::unistd::ttyname(io::stdin().as_fd()).ok()?;
    let pane_tty = command(&location.socket, &["display-message", "-p", "-t", &location.pane, "#{pane_tty}"]).ok()?;
    (tty.to_string_lossy() == pane_tty).then_some(location)
}

/// Resolve live panes in the dashboard's tmux server. Other servers and
/// terminals outside tmux are not jumpable from this tmux client.
pub fn choices(locations: &[TmuxLocation]) -> Result<Vec<JumpTarget>, String> {
    let source = current_location().ok_or("this dashboard is not in a tmux pane")?;
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for location in locations.iter().filter(|l| l.socket == source.socket && valid_pane(&l.pane)) {
        if !seen.insert(location.pane.clone()) {
            continue;
        }
        let Ok(label) = command(
            &location.socket,
            &[
                "display-message",
                "-p",
                "-t",
                &location.pane,
                "#{session_name}:#{window_name}.#{pane_index}  (#{pane_id})",
            ],
        ) else {
            continue;
        };
        targets.push(JumpTarget { location: location.clone(), label });
    }
    Ok(targets)
}

fn unique_client(lines: &str, source_pane: &str) -> Result<String, String> {
    let clients: Vec<&str> = lines
        .lines()
        .filter_map(|line| {
            let (tty, pane) = line.split_once('\t')?;
            (pane == source_pane).then_some(tty)
        })
        .collect();
    match clients.as_slice() {
        [tty] => Ok((*tty).to_string()),
        [] => Err("no tmux client is displaying this dashboard pane".into()),
        _ => Err("multiple tmux clients display this dashboard pane; cannot identify yours".into()),
    }
}

pub fn jump(target: &JumpTarget) -> Result<(), String> {
    let source = current_location().ok_or("this dashboard is not in a tmux pane")?;
    if source.socket != target.location.socket {
        return Err("target is in another tmux server".into());
    }
    // Validate again: a pane can close between opening the chooser and Enter.
    command(&source.socket, &["display-message", "-p", "-t", &target.location.pane, "#{pane_id}"])?;
    let clients = command(&source.socket, &["list-clients", "-F", "#{client_tty}\t#{pane_id}"])?;
    let client = unique_client(&clients, &source.pane)?;
    command(&source.socket, &["switch-client", "-c", &client, "-t", &target.location.pane])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_client_may_display_the_source_pane() {
        assert_eq!(unique_client("/dev/ttys001\t%3\n/dev/ttys002\t%4", "%3").unwrap(), "/dev/ttys001");
        assert!(unique_client("/dev/ttys001\t%3\n/dev/ttys002\t%3", "%3").is_err());
        assert!(unique_client("/dev/ttys001\t%4", "%3").is_err());
    }
}
