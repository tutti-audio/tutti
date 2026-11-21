//! Enabling a virtual UMP Endpoint while CoreMIDI's server is busy with another
//! process.
//!
//! The MIDI server serves every process's endpoint changes. An enable that arrives
//! while it is working on another process's change can be refused with
//! `kMIDINotPermitted`, and the same call made a few milliseconds later is accepted.
//! `EnableRetry` decides when to try again; the loop in `ump_endpoint` reads the
//! clock and sleeps.

use super::client_retry::Retry;
use super::error::OsStatus;
use std::time::Duration;
use tutti_platform::Instant;

/// How long an enable keeps retrying, measured from the first attempt.
pub(crate) const RETRY_BOUND: Duration = Duration::from_secs(2);

/// The wait after the first refused attempt; each later wait doubles.
const FIRST_WAIT: Duration = Duration::from_millis(10);

/// The longest single wait.
const LONGEST_WAIT: Duration = Duration::from_millis(200);

/// The retry rule for one enable: a pure function of the `now` it is given.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EnableRetry {
    deadline: Instant,
    wait: Duration,
}

impl EnableRetry {
    /// A rule for an enable whose first attempt starts at `start`.
    pub(crate) fn new(start: Instant) -> Self {
        Self {
            deadline: start + RETRY_BOUND,
            wait: FIRST_WAIT,
        }
    }

    /// The decision for an attempt that failed with `status` at `now`.
    ///
    /// Only [`OsStatus::NOT_PERMITTED`] is retried, with waits of 10 ms doubling to
    /// 200 ms, the last one clipped to the bound; at or past the bound it gives up.
    pub(crate) fn after(&mut self, status: OsStatus, now: Instant) -> Retry {
        if status != OsStatus::NOT_PERMITTED || now >= self.deadline {
            return Retry::GiveUp;
        }
        let next = (now + self.wait).min(self.deadline);
        self.wait = (self.wait * 2).min(LONGEST_WAIT);
        Retry::At(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSED: OsStatus = OsStatus::NOT_PERMITTED;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn retries_a_refusal_with_doubling_waits_capped_at_the_longest() {
        let start = Instant::now();
        let mut rule = EnableRetry::new(start);
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..7 {
            let Retry::At(at) = rule.after(REFUSED, now) else {
                panic!("gave up inside the bound");
            };
            waits.push(at - now);
            now = at;
        }
        assert_eq!(
            waits,
            [ms(10), ms(20), ms(40), ms(80), ms(160), ms(200), ms(200)]
        );
    }

    #[test]
    fn gives_up_at_the_bound_and_clips_the_last_wait_to_it() {
        let start = Instant::now();
        let mut rule = EnableRetry::new(start);
        assert_eq!(
            rule.after(REFUSED, start + RETRY_BOUND - ms(5)),
            Retry::At(start + RETRY_BOUND)
        );
        assert_eq!(rule.after(REFUSED, start + RETRY_BOUND), Retry::GiveUp);
        assert_eq!(
            rule.after(REFUSED, start + RETRY_BOUND + ms(1)),
            Retry::GiveUp
        );
    }

    #[test]
    fn never_retries_another_status() {
        let start = Instant::now();
        for raw in [-1, 0, -304, -10839, -10843, i32::MIN, i32::MAX] {
            let mut rule = EnableRetry::new(start);
            assert_eq!(
                rule.after(OsStatus::new(raw), start),
                Retry::GiveUp,
                "status {raw}"
            );
        }
    }
}
