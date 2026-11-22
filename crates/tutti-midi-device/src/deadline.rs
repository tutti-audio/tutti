//! The sans-IO timing vocabulary of the discoverers: a send that is repeated. Every
//! method takes the `now` it is asked at.

use std::time::Duration;

use tutti_midi_ci::Deadline;
use tutti_platform::Instant;

/// A request sent `attempts` times, the last of them ending its wait at `wait`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Retry {
    wait: Deadline,
    attempts: u8,
}

impl Retry {
    /// A request sent for the first time at `now`, awaited for `timeout`.
    pub(crate) fn first(now: Instant, timeout: Duration) -> Self {
        Self {
            wait: Deadline::after(now, timeout),
            attempts: 1,
        }
    }

    /// Whether the last send's wait is over at `now`.
    pub(crate) fn due(self, now: Instant) -> bool {
        self.wait.expired(now)
    }

    /// The instant the last send's wait ends.
    pub(crate) fn deadline(self) -> Instant {
        self.wait.deadline()
    }

    /// Whether `max` sends are made.
    pub(crate) const fn spent(self, max: u8) -> bool {
        self.attempts >= max
    }

    /// The request is sent again at `now`, awaited for `timeout`.
    pub(crate) fn resend(&mut self, now: Instant, timeout: Duration) {
        self.wait.restart(now, timeout);
        self.attempts = self.attempts.saturating_add(1);
    }
}

#[cfg(test)]
mod tests;
