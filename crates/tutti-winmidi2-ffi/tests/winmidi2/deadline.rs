//! One overall bound on a wait for several messages: a stream of messages the wait does not
//! want cannot hold it open past the bound.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use tutti_platform::Instant;

/// Only a hang guard: it bounds a wait for an event that never comes, and is not a speed.
pub(crate) const HANG_GUARD: Duration = Duration::from_secs(60);

/// The instant a wait gives up, [`HANG_GUARD`] after it starts.
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) fn start() -> Self {
        Self(Instant::now() + HANG_GUARD)
    }

    /// The next message on `rx`, or a timeout once the deadline has passed.
    pub(crate) fn recv<T>(&self, rx: &Receiver<T>) -> Result<T, RecvTimeoutError> {
        rx.recv_timeout(self.0.saturating_duration_since(Instant::now()))
    }
}
