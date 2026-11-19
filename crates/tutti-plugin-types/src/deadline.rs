//! [`Deadline`]: when a wait's time is up, a rule over clock readings.

use std::time::Duration;

use tutti_platform::Instant;

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

    /// The time left at `now`: zero once the deadline has passed, `None` for one
    /// that never does.
    #[must_use]
    pub fn left(self, now: Instant) -> Option<Duration> {
        self.0.map(|at| at.saturating_duration_since(now))
    }

    /// The earlier of this deadline and `other`: a wait bounded by both.
    #[must_use]
    pub fn sooner(self, other: Self) -> Self {
        match (self.0, other.0) {
            (Some(a), Some(b)) => Self(Some(a.min(b))),
            (a, b) => Self(a.or(b)),
        }
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

    /// The time left runs down to zero at the deadline, and a deadline that
    /// never passes has none to count.
    #[test]
    fn the_time_left_runs_down_to_zero_at_the_deadline() {
        let start = Instant::now();
        let deadline = Deadline::after(start, Duration::from_secs(15));
        assert_eq!(deadline.left(start), Some(Duration::from_secs(15)));
        assert_eq!(
            deadline.left(start + Duration::from_secs(14)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            deadline.left(start + Duration::from_secs(15)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            deadline.left(start + Duration::from_secs(99)),
            Some(Duration::ZERO)
        );
        assert_eq!(Deadline::never().left(start), None);
    }

    /// `sooner` is the earlier deadline, and `never` is the later of any.
    #[test]
    fn the_sooner_of_two_deadlines_is_the_earlier() {
        let start = Instant::now();
        let (near, far) = (
            Deadline::after(start, Duration::from_secs(1)),
            Deadline::after(start, Duration::from_secs(9)),
        );
        assert_eq!(near.sooner(far), near);
        assert_eq!(far.sooner(near), near);
        assert_eq!(Deadline::never().sooner(far), far);
        assert_eq!(far.sooner(Deadline::never()), far);
        assert_eq!(
            Deadline::never().sooner(Deadline::never()),
            Deadline::never()
        );
    }
}
