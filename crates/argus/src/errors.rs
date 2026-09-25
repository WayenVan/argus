//! Errors with a stable code, shared by the manager (which sends the code in
//! `Response::Error`) and the client (which prints it under `--json` and
//! turns it into an exit status).

/// An error scripts can tell apart from others by `code`.
#[derive(Debug)]
pub struct CodedError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

/// The target names no agent.
pub const NOT_FOUND: &str = "not_found";
/// The target names more than one agent where one is needed.
pub const AMBIGUOUS: &str = "ambiguous";
/// The agent cannot take a prompt now; retry later (exit 75).
pub const NOT_READY: &str = "not_ready";
/// A `--timeout` ran out (exit 124).
pub const TIMEOUT: &str = "timeout";
/// An agent being waited for exited or was removed first.
pub const EXITED: &str = "exited";
/// The manager could not be started or reached, or speaks another protocol.
pub const MANAGER_UNAVAILABLE: &str = "manager_unavailable";
/// Anything else.
pub const FAILED: &str = "failed";

pub fn coded(code: &str, message: impl Into<String>) -> anyhow::Error {
    CodedError { code: code.into(), message: message.into() }.into()
}

/// The code of `e`, looking through any context added on the way up.
pub fn code_of(e: &anyhow::Error) -> &str {
    e.downcast_ref::<CodedError>().map_or(FAILED, |c| c.code.as_str())
}

pub fn has_code(e: &anyhow::Error, code: &str) -> bool {
    code_of(e) == code
}

/// The process exit status for a failed command, like `timeout(1)` and
/// `sysexits.h` where they apply.
pub fn exit_status(e: &anyhow::Error) -> u8 {
    match code_of(e) {
        NOT_READY => 75,
        TIMEOUT => 124,
        _ => 1,
    }
}
