//! A native target's thread edge: std's threads and a condvar wait.

use std::io;
use std::sync::PoisonError;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use super::{StopState, lock_recover};
use crate::Instant;

/// The end of a [`StopToken::wait`](super::StopToken::wait): a function of the `now` it is
/// asked at.
#[derive(Debug, Clone, Copy)]
pub(super) struct Deadline(Option<Instant>);

impl Deadline {
    /// The deadline `timeout` after `now`; a timeout that overflows the clock
    /// never ends.
    pub(super) fn after(now: Instant, timeout: Duration) -> Self {
        Self(now.checked_add(timeout))
    }

    /// The time left at `now`, or `None` once the deadline is reached.
    pub(super) fn left(self, now: Instant) -> Option<Duration> {
        match self.0 {
            Some(end) => end.checked_duration_since(now).filter(|d| !d.is_zero()),
            None => Some(Duration::MAX),
        }
    }
}

/// Starts a thread named `name` running `body`.
#[expect(
    clippy::disallowed_methods,
    reason = "the platform's own thread start: the executor's threads and `spawn_named` begin here"
)]
pub(super) fn spawn<F, R>(name: &str, body: F) -> io::Result<JoinHandle<R>>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    Builder::new().name(name.to_owned()).spawn(body)
}

/// Sleeps at most `timeout`, waking early when the stop is requested; returns whether it
/// was.
pub(super) fn wait(state: &StopState, timeout: Duration) -> bool {
    let deadline = Deadline::after(Instant::now(), timeout);
    let mut gate = lock_recover(&state.gate);
    loop {
        if state.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        let Some(left) = deadline.left(Instant::now()) else {
            return false;
        };
        gate = state
            .wake
            .wait_timeout(gate, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}
