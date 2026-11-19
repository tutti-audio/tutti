//! The sans-IO timing vocabulary of the discoverers: a send that is repeated. Every
//! method takes the `now` it is asked at.

use std::time::{Duration, Instant};

/// A request sent `attempts` times, the last at `sent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Retry {
    sent: Instant,
    attempts: u8,
}

impl Retry {
    /// A request sent for the first time at `now`.
    pub(crate) const fn first(now: Instant) -> Self {
        Self {
            sent: now,
            attempts: 1,
        }
    }

    /// Whether `timeout` has gone by since the last send, at `now`.
    pub(crate) fn due(self, now: Instant, timeout: Duration) -> bool {
        now.saturating_duration_since(self.sent) >= timeout
    }

    /// The instant the last send's `timeout` ends.
    pub(crate) fn deadline(self, timeout: Duration) -> Instant {
        self.sent + timeout
    }

    /// Whether `max` sends are made.
    pub(crate) const fn spent(self, max: u8) -> bool {
        self.attempts >= max
    }

    /// The request is sent again at `now`.
    pub(crate) fn resend(&mut self, now: Instant) {
        self.sent = now;
        self.attempts = self.attempts.saturating_add(1);
    }
}

#[cfg(test)]
mod tests;
