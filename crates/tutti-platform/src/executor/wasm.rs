//! The web's default executor: futures run on the caller's `poll`, since a page
//! starts no threads of its own.

use super::{Drive, Executor};

/// A host executor on the web has no thread to start: its timers fire in `poll`.
pub(super) const HOST_DRIVE: Drive = Drive::Poll;

pub(super) fn default_executor() -> Executor {
    Executor::polled()
}
