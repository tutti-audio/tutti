//! The off-main deadline as a pure rule.

use std::time::{Duration, Instant};

/// What an off-main call makes of its wait at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wait {
    /// The main thread has until the deadline: block for at most this long.
    For(Duration),
    /// The deadline has come: answer "unavailable".
    Expired,
}

/// How long an off-main call waits for the main thread to run its work: from
/// the instant the call began to `limit` after it.
///
/// It reads no clock. The waiting call passes the instant it sampled, so the
/// rule is a function of the instants it is given and a test drives it with
/// constructed ones.
#[derive(Clone, Copy, Debug)]
pub(super) struct OffMainWait {
    due: Instant,
}

impl OffMainWait {
    /// A wait that began at `began` and lasts `limit`.
    pub(super) fn new(limit: Duration, began: Instant) -> Self {
        Self { due: began + limit }
    }

    /// The verdict at `now`: how much of the wait is left, or [`Wait::Expired`]
    /// once `now` is at or past the deadline.
    pub(super) fn at(&self, now: Instant) -> Wait {
        match self.due.checked_duration_since(now) {
            Some(left) if !left.is_zero() => Wait::For(left),
            _ => Wait::Expired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// **The wait has what is left of the limit until the limit, and not a
    /// moment after.**
    #[test]
    fn the_wait_is_what_is_left_of_the_limit_until_it_is_spent() {
        let began = Instant::now();
        let wait = OffMainWait::new(ms(2000), began);
        assert_eq!(wait.at(began), Wait::For(ms(2000)));
        assert_eq!(wait.at(began + ms(500)), Wait::For(ms(1500)));
        assert_eq!(wait.at(began + ms(1999)), Wait::For(ms(1)));
        assert_eq!(wait.at(began + ms(2000)), Wait::Expired);
        assert_eq!(wait.at(began + ms(9000)), Wait::Expired);
    }

    /// **The deadline is fixed by the start**: an instant before it has more than
    /// the limit left, and a zero limit expires at the start.
    #[test]
    fn the_deadline_is_the_start_plus_the_limit() {
        let began = Instant::now() + ms(1000);
        let wait = OffMainWait::new(ms(100), began);
        assert_eq!(wait.at(began - ms(1000)), Wait::For(ms(1100)));
        assert_eq!(
            OffMainWait::new(Duration::ZERO, began).at(began),
            Wait::Expired
        );
    }
}
