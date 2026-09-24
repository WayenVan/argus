//! Starting the agent on a fresh PTY.

use std::ffi::{CString, c_char};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use argus_proto::msg::HolderSpec;
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid, fork, pipe, read};

pub struct Spawned {
    pub master: OwnedFd,
    pub pid: Pid,
}

/// Failure stages reported by the child over the exec-error pipe.
const STAGE_SETSID: u8 = 1;
const STAGE_CTTY: u8 = 2;
const STAGE_DUP: u8 = 3;
const STAGE_CHDIR: u8 = 4;
const STAGE_EXEC: u8 = 5;

pub fn spawn_agent(spec: &HolderSpec) -> Result<Spawned> {
    let Some(program) = spec.command.first() else { bail!("empty command") };
    let path = resolve_program(program, &spec.cwd, &spec.env)?;

    // Everything the child needs is built before fork: after fork the child
    // may only make async-signal-safe calls.
    let c_path = cstring(path.as_os_str().as_encoded_bytes())?;
    let c_cwd = cstring(spec.cwd.as_bytes())?;
    let c_argv = spec.command.iter().map(|a| cstring(a.as_bytes())).collect::<Result<Vec<_>>>()?;
    let c_env = agent_env(spec).iter().map(|kv| cstring(kv.as_bytes())).collect::<Result<Vec<_>>>()?;
    let argv = null_terminated(&c_argv);
    let envp = null_terminated(&c_env);

    let ws = Winsize { ws_row: spec.rows.max(1), ws_col: spec.cols.max(1), ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).context("openpty")?;
    set_cloexec(pty.master.as_raw_fd());
    let (err_r, err_w) = cloexec_pipe()?;

    // SAFETY: the holder is single-threaded; the child branch below only
    // performs async-signal-safe syscalls before execve or _exit.
    match unsafe { fork() }.context("fork")? {
        ForkResult::Child => unsafe {
            child_exec(pty.slave.as_raw_fd(), err_w.as_raw_fd(), &c_cwd, &c_path, &argv, &envp)
        },
        ForkResult::Parent { child } => {
            drop(pty.slave);
            drop(err_w);
            let mut report = [0u8; 5];
            let n = read_full(err_r.as_raw_fd(), &mut report);
            if n == 0 {
                return Ok(Spawned { master: pty.master, pid: child });
            }
            let _ = waitpid(child, None);
            let errno = std::io::Error::from_raw_os_error(i32::from_be_bytes(report[1..5].try_into().unwrap()));
            match report[0] {
                STAGE_CHDIR => bail!("cannot enter {}: {errno}", spec.cwd),
                STAGE_EXEC => bail!("cannot execute {}: {errno}", path.display()),
                STAGE_SETSID => bail!("setsid: {errno}"),
                STAGE_CTTY => bail!("TIOCSCTTY: {errno}"),
                STAGE_DUP => bail!("dup2: {errno}"),
                _ => bail!("agent failed to start: {errno}"),
            }
        }
    }
}

/// Runs in the forked child. Never returns.
unsafe fn child_exec(
    slave: RawFd,
    err_w: RawFd,
    cwd: &CString,
    path: &CString,
    argv: &[*const c_char],
    envp: &[*const c_char],
) -> ! {
    unsafe {
        let fail = |stage: u8| -> ! {
            let errno = *errno_location();
            let mut report = [stage, 0, 0, 0, 0];
            report[1..5].copy_from_slice(&errno.to_be_bytes());
            libc::write(err_w, report.as_ptr().cast(), report.len());
            libc::_exit(127)
        };

        // Undo the holder's signal setup; ignored dispositions survive exec.
        for sig in [libc::SIGPIPE, libc::SIGCHLD, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
            libc::signal(sig, libc::SIG_DFL);
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());

        if libc::setsid() < 0 {
            fail(STAGE_SETSID);
        }
        if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
            fail(STAGE_CTTY);
        }
        for fd in 0..3 {
            if libc::dup2(slave, fd) < 0 {
                fail(STAGE_DUP);
            }
        }
        if slave > 2 {
            libc::close(slave);
        }
        if libc::chdir(cwd.as_ptr()) < 0 {
            fail(STAGE_CHDIR);
        }
        libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
        fail(STAGE_EXEC)
    }
}

/// The client's environment plus argus variables. TERM is forced so agents
/// render for a known terminal regardless of where they are attached from.
fn agent_env(spec: &HolderSpec) -> Vec<String> {
    let mut env: Vec<String> = spec
        .env
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "TERM" | "ARGUS_AGENT_ID" | "ARGUS_SOCKET"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    env.push("TERM=xterm-256color".into());
    env.push(format!("ARGUS_AGENT_ID={}", spec.id));
    env.push(format!("ARGUS_SOCKET={}", spec.manager_socket.display()));
    env
}

/// Resolves the program like a shell would, but against the agent's PATH and
/// cwd, so "command not found" is reported before anything is forked.
fn resolve_program(program: &str, cwd: &str, env: &[(String, String)]) -> Result<PathBuf> {
    if program.contains('/') {
        let path = Path::new(cwd).join(program);
        if is_executable(&path) {
            return Ok(path);
        }
        bail!("not an executable file: {program}");
    }
    let search = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap_or("/usr/bin:/bin");
    for dir in search.split(':').filter(|d| !d.is_empty()) {
        let path = Path::new(cwd).join(dir).join(program);
        if is_executable(&path) {
            return Ok(path);
        }
    }
    bail!("command not found: {program}")
}

fn is_executable(path: &Path) -> bool {
    path.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

fn cstring(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).context("argument or environment contains a NUL byte")
}

fn null_terminated(strings: &[CString]) -> Vec<*const c_char> {
    strings.iter().map(|s| s.as_ptr()).chain(std::iter::once(std::ptr::null())).collect()
}

pub fn set_cloexec(fd: RawFd) {
    let _ = fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC));
}

pub fn set_nonblocking(fd: RawFd) {
    if let Ok(flags) = fcntl(fd, FcntlArg::F_GETFL) {
        let _ = fcntl(fd, FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK));
    }
}

pub fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let (read, write) = pipe().context("pipe")?;
    set_cloexec(read.as_raw_fd());
    set_cloexec(write.as_raw_fd());
    Ok((read, write))
}

fn read_full(fd: RawFd, buf: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buf.len() {
        match read(fd, &mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
    filled
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__error() }
}

#[cfg(not(target_os = "macos"))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__errno_location() }
}
