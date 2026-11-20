//! The sans-IO timing vocabulary of the initiators: a wait that ends at an
//! instant. Every method takes the `now` it is asked at.

use std::time::Duration;

use tutti_platform::Instant;

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

    /// Whichever of the two waits ends first.
    pub(crate) fn sooner(self, other: Self) -> Self {
        if other.deadline < self.deadline {
            other
        } else {
            self
        }
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

#[cfg(test)]
mod tests;
