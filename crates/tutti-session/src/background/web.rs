//! The web: a page starts no thread, so nothing drives the executor.

use std::io;

use tutti_platform::Executor;

/// No thread can start.
pub(crate) const THREADS: bool = false;

/// Nothing runs.
pub(crate) struct Running;

/// Nothing starts: a build that needs the thread fails first
/// ([`require`](super::require)).
#[expect(clippy::unnecessary_wraps, reason = "the native twin's signature")]
pub(crate) fn start(_executor: &Executor) -> io::Result<Running> {
    Ok(Running)
}

/// Runs, on the caller's thread, what the executor has ready: a page starts no thread, so
/// the session's update is what drives its tasks.
pub(crate) fn drive(executor: &Executor) {
    executor.run_until_stalled();
}
