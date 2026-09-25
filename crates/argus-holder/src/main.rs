//! argus-holder: owns one agent's PTY and is the agent's parent process.
//!
//! Started only by the manager:
//! - stdin carries one JSON [`HolderSpec`] and is then closed;
//! - stdout is a pipe on which we report [`HolderReady`] once serving;
//! - stderr is appended to the agent's `holder.log`.
//!
//! The holder forks once at startup so that it is adopted by init and never
//! becomes the manager's child: the manager can die or be upgraded freely.

mod conn;
mod ring;
mod server;
mod spawn;
mod stand_in;

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

use argus_proto::msg::{HolderReady, HolderSpec};
use nix::unistd::{ForkResult, fork, setsid};

fn main() {
    let mut ready = take_ready_pipe();

    let spec = match read_spec() {
        Ok(spec) => spec,
        Err(e) => fail(&mut ready, &format!("invalid holder spec: {e:#}")),
    };

    // SAFETY: still single-threaded; the parent only calls _exit.
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => unsafe { libc::_exit(0) },
        Ok(ForkResult::Child) => {}
        Err(e) => fail(&mut ready, &format!("fork: {e}")),
    }
    let _ = setsid();
    // SAFETY: setting a signal disposition to SIG_IGN has no preconditions.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };

    match server::Holder::start(spec) {
        Ok(holder) => {
            let msg = HolderReady::Ready { holder_pid: std::process::id(), agent_pid: holder.agent_pid() };
            let _ = writeln!(ready, "{}", serde_json::to_string(&msg).unwrap());
            drop(ready);
            holder.run()
        }
        Err(e) => fail(&mut ready, &format!("{e:#}")),
    }
}

fn read_spec() -> anyhow::Result<HolderSpec> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    Ok(serde_json::from_str(&input)?)
}

/// Moves the ready pipe off fd 1 and points stdin/stdout at /dev/null, so
/// nothing but the ready message ever reaches the manager through it.
fn take_ready_pipe() -> File {
    // SAFETY: plain fd juggling on fds this process owns.
    unsafe {
        let ready = libc::dup(1);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 1);
        }
        if ready < 0 {
            // No pipe to report on; keep going with a sink.
            return File::from_raw_fd(libc::dup(null));
        }
        libc::fcntl(ready, libc::F_SETFD, libc::FD_CLOEXEC);
        File::from_raw_fd(ready)
    }
}

fn fail(ready: &mut File, message: &str) -> ! {
    eprintln!("argus-holder: {message}");
    let msg = HolderReady::Failed { message: message.to_string() };
    let _ = writeln!(ready, "{}", serde_json::to_string(&msg).unwrap());
    std::process::exit(1)
}
