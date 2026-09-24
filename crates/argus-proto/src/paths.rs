//! Filesystem layout. Everything is keyed by agent ID, never by name.
//!
//! Runtime dir (sockets, lock): `$ARGUS_RUNTIME_DIR`, else
//! `$XDG_RUNTIME_DIR/argus`, else `$TMPDIR/argus-$UID`.
//! State dir (registry, logs): `$ARGUS_STATE_DIR`, else
//! `$XDG_STATE_HOME/argus`, else `~/.local/state/argus`.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("ARGUS_RUNTIME_DIR") {
        return dir.into();
    }
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        return Path::new(&dir).join("argus");
    }
    let tmp = env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    tmp.join(format!("argus-{uid}"))
}

pub fn state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("ARGUS_STATE_DIR") {
        return dir.into();
    }
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return Path::new(&dir).join("argus");
    }
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
    home.join(".local/state/argus")
}

pub fn manager_socket() -> PathBuf {
    runtime_dir().join("argus.sock")
}

pub fn manager_lock() -> PathBuf {
    runtime_dir().join("argusd.lock")
}

pub fn manager_pid() -> PathBuf {
    runtime_dir().join("argusd.pid")
}

pub fn holders_dir() -> PathBuf {
    runtime_dir().join("holders")
}

pub fn holder_socket(id: u64) -> PathBuf {
    holders_dir().join(format!("{id}.sock"))
}

/// Symlink to the holder socket, maintained by the manager so that `attach`
/// works by name without asking it. Nested names become nested directories.
pub fn name_socket(name: &str) -> PathBuf {
    holders_dir().join("by-name").join(format!("{name}.sock"))
}

pub fn registry_file() -> PathBuf {
    state_dir().join("agents.json")
}

pub fn manager_log() -> PathBuf {
    state_dir().join("argusd.log")
}

pub fn agent_dir(id: u64) -> PathBuf {
    state_dir().join("agents").join(id.to_string())
}

pub fn exit_record(agent_dir: &Path) -> PathBuf {
    agent_dir.join("exit.json")
}

pub fn output_log(agent_dir: &Path) -> PathBuf {
    agent_dir.join("output.log")
}

/// Creates `dir` (and parents) and makes sure only the owner can enter it.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}
