//! The web's thread edge: a page starts no threads and cannot block, and std's condvar
//! wait panics there, so a start is refused as `Unsupported` and a wait returns at once.

use std::io;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use super::StopState;

/// Refused: the web has no threads.
pub(super) fn spawn<F, R>(_name: &str, _body: F) -> io::Result<JoinHandle<R>>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the web has no threads",
    ))
}

/// Returns at once, with whether the stop was requested: a page cannot block.
pub(super) fn wait(state: &StopState, _timeout: Duration) -> bool {
    state.stopped.load(Ordering::Acquire)
}
