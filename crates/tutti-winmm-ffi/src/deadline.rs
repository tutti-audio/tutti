//! A bound on a wait: a pure function of the time it is given.

use std::time::{Duration, Instant};

/// The instant a bounded wait gives up. The loop that waits reads the clock and
/// asks [`passed`](Self::passed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deadline(Instant);

impl Deadline {
    /// A deadline `span` after `now`.
    pub(crate) fn after(now: Instant, span: Duration) -> Self {
        Self(now + span)
    }

    /// Whether `now` is at or past the deadline.
    pub(crate) fn passed(self, now: Instant) -> bool {
        now >= self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deadline is not passed before its span, and is at it and after.
    #[test]
    fn a_deadline_passes_at_its_span() {
        let t0 = Instant::now();
        let deadline = Deadline::after(t0, Duration::from_secs(1));
        assert!(!deadline.passed(t0));
        assert!(!deadline.passed(t0 + Duration::from_millis(999)));
        assert!(deadline.passed(t0 + Duration::from_secs(1)));
        assert!(deadline.passed(t0 + Duration::from_secs(5)));
    }
}
