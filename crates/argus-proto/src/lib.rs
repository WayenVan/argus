//! Wire protocol shared by the `argus` client, the manager and `argus-holder`.
//!
//! Every connection speaks the same framing (see [`frame`]). Control messages
//! are JSON ([`msg`]); PTY bytes travel in binary stream frames.
//!
//! Compatibility rule: the holder protocol only ever grows. A newer manager
//! must be able to talk to every holder that is still running.

pub mod frame;
pub mod msg;
pub mod paths;

/// Bumped only when a change cannot be expressed as an added optional field.
pub const PROTOCOL_VERSION: u32 = 1;
