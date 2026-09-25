//! Wire protocol shared by the `argus` client, the manager and `argus-holder`.
//!
//! Every connection speaks the same framing (see [`frame`]). Control messages
//! are JSON ([`msg`]); PTY bytes travel in binary stream frames.
//!
//! Compatibility rule: the holder protocol only ever grows. A newer manager
//! must be able to talk to every holder that is still running.

pub mod ansi;
pub mod frame;
pub mod msg;
pub mod paths;

/// Client ↔ manager protocol. Bumped for incompatible request/response changes.
pub const MANAGER_PROTOCOL_VERSION: u32 = 2;

/// Manager/client ↔ holder protocol. Kept independent so a new manager can
/// reconnect to holders left running by an older installation.
pub const HOLDER_PROTOCOL_VERSION: u32 = 1;

/// This build: the crate version plus the git commit it was built at, when
/// known. Reported in `Hello` replies so a client can tell that the manager
/// (or a holder) still runs code from an older install; unlike the protocol
/// versions it changes with every commit, and is only ever informational.
pub const BUILD: &str = env!("ARGUS_BUILD");
