//! [`Deadline`]: when a wait's time is up, a rule over clock readings.

use std::time::{Duration, Instant};

/// When a wait's time is up: a rule over clock readings, which its caller takes.
/// It reads no clock, so a test passes constructed instants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    /// The deadline `timeout` after `now`; one the clock cannot represent never
    /// passes.
    #[must_use]
    pub fn after(now: Instant, timeout: Duration) -> Self {
        Self(now.checked_add(timeout))
    }

    /// A deadline that never passes.
    #[must_use]
    pub const fn never() -> Self {
        Self(None)
    }

    /// Whether the time is up at `now`: at the deadline itself, and after.
    #[must_use]
    pub fn passed(self, now: Instant) -> bool {
        self.0.is_some_and(|at| now >= at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deadline is up at its instant and after, and not before; one the clock
    /// cannot hold, and `never`, are never up.
    #[test]
    fn a_deadline_is_up_at_its_instant_and_after() {
        let start = Instant::now();
        let deadline = Deadline::after(start, Duration::from_secs(15));
        assert!(!deadline.passed(start));
        assert!(!deadline.passed(start + Duration::from_millis(14_999)));
        assert!(deadline.passed(start + Duration::from_secs(15)));
        assert!(deadline.passed(start + Duration::from_secs(16)));
        assert!(Deadline::after(start, Duration::ZERO).passed(start));
        let far = start + Duration::from_secs(1 << 30);
        assert!(!Deadline::after(start, Duration::MAX).passed(far));
        assert!(!Deadline::never().passed(far));
    }
}
