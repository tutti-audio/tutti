//! A native target's default executor: its own threads.

use super::{Drive, Executor};

/// A host executor on a native target gets a timer thread.
pub(super) const HOST_DRIVE: Drive = Drive::Thread;

pub(super) fn default_executor() -> Executor {
    Executor::threaded()
}
