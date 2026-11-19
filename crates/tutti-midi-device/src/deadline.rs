//! The sans-IO timing vocabulary of the initiators: a wait that ends at an
//! instant and a send that is repeated. Every method takes the `now` it is asked
//! at.

use std::time::{Duration, Instant};

/// A wait that ends at an instant: reached when `now` is at or past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    deadline: Instant,
}

impl Pending {
    /// A wait of `timeout` from `now`.
    pub(crate) fn after(now: Instant, timeout: Duration) -> Self {
        Self {
            deadline: now + timeout,
        }
    }

    /// The wait runs `timeout` from `now` again.
    pub(crate) fn restart(&mut self, now: Instant, timeout: Duration) {
        *self = Self::after(now, timeout);
    }

    /// The wait ends `wait` later than it did.
    pub(crate) fn extend(&mut self, wait: Duration) {
        self.deadline += wait;
    }

    /// Whether the wait is over at `now`: an instant at the deadline is late.
    pub(crate) fn expired(self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// The instant the wait ends.
    pub(crate) fn deadline(self) -> Instant {
        self.deadline
    }

    /// The soonest end among `waits`, or `None` with none.
    pub(crate) fn earliest(waits: impl IntoIterator<Item = Self>) -> Option<Instant> {
        waits.into_iter().map(Self::deadline).min()
    }
}

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
