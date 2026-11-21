//! Trying a CoreMIDI call again while it fails with one status that passes.
//!
//! [`Backoff`] decides when to try again, as a pure function of the `now` it is
//! given; [`retry`] is the loop that reads the clock and sleeps. A client creation
//! and an endpoint enable are two parameter sets of the same rule.

use super::error::OsStatus;
use std::time::Duration;
use tutti_platform::Instant;

/// How long a client creation keeps retrying, measured from the first attempt.
/// It covers the MIDI server's restart of about five seconds with room to spare.
pub(crate) const CLIENT_BOUND: Duration = Duration::from_secs(10);

/// How long an endpoint enable keeps retrying, measured from the first attempt.
const ENABLE_BOUND: Duration = Duration::from_secs(2);

/// What to do after a failed attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Try again at this instant.
    At(Instant),
    /// Return the error.
    GiveUp,
}

/// The waits between attempts that fail with one status: the first wait, doubling
/// to the longest, until the bound from the first attempt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Backoff {
    retried: OsStatus,
    longest_wait: Duration,
    deadline: Instant,
    wait: Duration,
}

impl Backoff {
    /// A rule for a first attempt at `start` that retries `retried` with waits of
    /// `first_wait` doubling to `longest_wait`, for `bound`.
    fn new(
        retried: OsStatus,
        first_wait: Duration,
        longest_wait: Duration,
        bound: Duration,
        start: Instant,
    ) -> Self {
        Self {
            retried,
            longest_wait,
            deadline: start + bound,
            wait: first_wait,
        }
    }

    /// The rule for a client creation: it retries [`OsStatus::SERVER_GONE`] with
    /// waits of 100 ms doubling to 1 s, for [`CLIENT_BOUND`].
    pub(crate) fn client(start: Instant) -> Self {
        Self::new(
            OsStatus::SERVER_GONE,
            Duration::from_millis(100),
            Duration::from_secs(1),
            CLIENT_BOUND,
            start,
        )
    }

    /// The rule for an endpoint enable: it retries [`OsStatus::NOT_PERMITTED`]
    /// with waits of 10 ms doubling to 200 ms, for two seconds.
    pub(crate) fn enable(start: Instant) -> Self {
        Self::new(
            OsStatus::NOT_PERMITTED,
            Duration::from_millis(10),
            Duration::from_millis(200),
            ENABLE_BOUND,
            start,
        )
    }

    /// This rule with its window already over: it answers every failure with
    /// [`Retry::GiveUp`].
    pub(crate) fn without_window(mut self, start: Instant) -> Self {
        self.deadline = start;
        self
    }

    /// The decision for an attempt that failed with `status` at `now`.
    ///
    /// Only the retried status gets a wait, the last one clipped to the bound; at
    /// or past the bound it gives up.
    pub(crate) fn after(&mut self, status: OsStatus, now: Instant) -> Retry {
        if status != self.retried || now >= self.deadline {
            return Retry::GiveUp;
        }
        let next = (now + self.wait).min(self.deadline);
        self.wait = (self.wait * 2).min(self.longest_wait);
        Retry::At(next)
    }
}

/// Runs `attempt` until it succeeds or `rule` gives up, and returns the last error.
///
/// `status` is the `OsStatus` an error carries, which is what `rule` filters on;
/// an error with none is returned at once.
///
/// # Errors
///
/// The error of the last attempt.
pub(crate) fn retry<T, E>(
    mut rule: Backoff,
    mut attempt: impl FnMut() -> Result<T, E>,
    status: impl Fn(&E) -> Option<OsStatus>,
) -> Result<T, E> {
    loop {
        let error = match attempt() {
            Ok(done) => return Ok(done),
            Err(error) => error,
        };
        let Some(status) = status(&error) else {
            return Err(error);
        };
        match rule.after(status, Instant::now()) {
            Retry::At(at) => std::thread::sleep(at.saturating_duration_since(Instant::now())),
            Retry::GiveUp => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER_GONE: OsStatus = OsStatus::SERVER_GONE;
    const REFUSED: OsStatus = OsStatus::NOT_PERMITTED;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn waits(mut rule: Backoff, status: OsStatus, count: usize, start: Instant) -> Vec<Duration> {
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..count {
            let Retry::At(at) = rule.after(status, now) else {
                panic!("gave up inside the bound");
            };
            waits.push(at - now);
            now = at;
        }
        waits
    }

    #[test]
    fn a_gone_server_is_retried_with_doubling_waits_capped_at_one_second() {
        let start = Instant::now();
        assert_eq!(
            waits(Backoff::client(start), SERVER_GONE, 6, start),
            [ms(100), ms(200), ms(400), ms(800), ms(1000), ms(1000)]
        );
    }

    #[test]
    fn a_refused_enable_is_retried_with_doubling_waits_capped_at_200_ms() {
        let start = Instant::now();
        assert_eq!(
            waits(Backoff::enable(start), REFUSED, 7, start),
            [ms(10), ms(20), ms(40), ms(80), ms(160), ms(200), ms(200)]
        );
    }

    #[test]
    fn each_rule_gives_up_at_its_bound_and_clips_the_last_wait_to_it() {
        let start = Instant::now();
        for (mut rule, status, bound) in [
            (Backoff::client(start), SERVER_GONE, Duration::from_secs(10)),
            (Backoff::enable(start), REFUSED, Duration::from_secs(2)),
        ] {
            assert_eq!(
                rule.after(status, start + bound - ms(5)),
                Retry::At(start + bound)
            );
            assert_eq!(rule.after(status, start + bound), Retry::GiveUp);
            assert_eq!(rule.after(status, start + bound + ms(1)), Retry::GiveUp);
        }
    }

    #[test]
    fn the_client_rule_covers_a_five_second_server_restart() {
        let start = Instant::now();
        let mut rule = Backoff::client(start);
        let mut now = start;
        while now < start + Duration::from_secs(5) {
            let Retry::At(at) = rule.after(SERVER_GONE, now) else {
                panic!("gave up before the server could restart");
            };
            now = at;
        }
    }

    #[test]
    fn each_rule_retries_only_its_own_status() {
        let start = Instant::now();
        for raw in [-1, 0, -303, -305, -10839, -10843, i32::MIN, i32::MAX] {
            let status = OsStatus::new(raw);
            assert_eq!(
                Backoff::client(start).after(status, start),
                Retry::GiveUp,
                "client, status {status}"
            );
            assert_eq!(
                Backoff::enable(start).after(status, start),
                Retry::GiveUp,
                "enable, status {status}"
            );
        }
        assert_eq!(
            Backoff::client(start).after(REFUSED, start),
            Retry::GiveUp,
            "the client does not retry a refusal"
        );
        assert_eq!(
            Backoff::enable(start).after(SERVER_GONE, start),
            Retry::GiveUp,
            "the enable does not retry a gone server"
        );
    }

    #[test]
    fn a_rule_without_its_window_gives_up_at_once() {
        let start = Instant::now();
        let mut rule = Backoff::client(start).without_window(start);
        assert_eq!(rule.after(SERVER_GONE, start), Retry::GiveUp);
    }

    #[test]
    fn retry_runs_an_enable_until_the_refusal_passes() {
        let mut calls = 0;
        let result = retry(
            Backoff::enable(Instant::now()),
            || {
                calls += 1;
                if calls < 3 { Err(REFUSED) } else { Ok(7) }
            },
            |status| Some(*status),
        );
        assert_eq!(result, Ok(7));
        assert_eq!(calls, 3);
    }

    #[test]
    fn retry_returns_a_status_the_rule_does_not_retry_at_once() {
        let mut calls = 0;
        let result: Result<(), OsStatus> = retry(
            Backoff::enable(Instant::now()),
            || {
                calls += 1;
                Err(OsStatus::new(-10843))
            },
            |status| Some(*status),
        );
        assert_eq!(result, Err(OsStatus::new(-10843)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_returns_an_error_without_a_status_at_once_and_unchanged() {
        let mut calls = 0;
        let result: Result<(), &str> = retry(
            Backoff::enable(Instant::now()),
            || {
                calls += 1;
                Err("an NSError whose code is no OSStatus")
            },
            |_| None,
        );
        assert_eq!(result, Err("an NSError whose code is no OSStatus"));
        assert_eq!(calls, 1);
    }
}
