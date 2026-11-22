//! When to try a CoreMIDI call again while it fails with one status that passes.
//!
//! [`Backoff`] is a schedule: a pure function of the `now` it is given, with no
//! clock and no sleep. A value that owns one (a client's start, an endpoint's
//! creation) is polled, and [`Backoff::deadline`] is the instant its next poll can
//! make progress. A client creation and an endpoint's registration and enable are
//! two parameter sets of the same rule.

use super::os_status::OsStatus;
use std::task::Poll;
use std::time::Duration;
use tutti_platform::Instant;

/// How long a client creation keeps retrying, measured from the first attempt.
/// It covers the MIDI server's restart of about five seconds with room to spare.
pub(crate) const CLIENT_BOUND: Duration = Duration::from_secs(10);

/// How long an endpoint's enable or registration keeps retrying, measured from the first attempt.
const ENDPOINT_BOUND: Duration = Duration::from_secs(2);

/// What a poll at some instant does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Due {
    /// The next attempt is not due yet.
    Wait,
    /// An attempt is due now.
    Attempt,
    /// The bound has passed: no attempt is made.
    TimedOut,
}

/// What to do after a failed attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Try again at [`Backoff::deadline`].
    Retry,
    /// Return the error.
    Fatal,
}

/// The waits between attempts that fail with one status: the first wait, doubling
/// to the longest, until the bound from the first attempt. An attempt is made only
/// at an instant before the bound.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Backoff {
    retried: OsStatus,
    longest_wait: Duration,
    wait: Duration,
    bound_at: Instant,
    next_at: Instant,
    single_attempt: bool,
    last: Option<OsStatus>,
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
            wait: first_wait,
            bound_at: start + bound,
            next_at: start,
            single_attempt: false,
            last: None,
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

    /// The rule for an endpoint's enable and registration: it retries
    /// [`OsStatus::NOT_PERMITTED`] with waits of 10 ms doubling to 200 ms, for two seconds.
    pub(crate) fn endpoint(start: Instant) -> Self {
        Self::new(
            OsStatus::NOT_PERMITTED,
            Duration::from_millis(10),
            Duration::from_millis(200),
            ENDPOINT_BOUND,
            start,
        )
    }

    /// This rule making one attempt: the first failure is [`Verdict::Fatal`].
    pub(crate) fn single_attempt(mut self) -> Self {
        self.single_attempt = true;
        self
    }

    /// Whether this rule makes one attempt ([`Backoff::single_attempt`]).
    pub(crate) fn is_single_attempt(&self) -> bool {
        self.single_attempt
    }

    /// When the next poll can make progress: the next attempt's instant, never
    /// past the bound (a poll at the bound is the timeout). After the last failure
    /// it is the give-up instant: the bound.
    pub(crate) fn deadline(&self) -> Instant {
        self.next_at
    }

    /// What a poll at `now` does.
    pub(crate) fn due(&self, now: Instant) -> Due {
        if now >= self.bound_at {
            Due::TimedOut
        } else if now >= self.next_at {
            Due::Attempt
        } else {
            Due::Wait
        }
    }

    /// The status the rule last retried; `None` before any attempt failed (a bound
    /// that passed with no attempt).
    pub(crate) fn last(&self) -> Option<OsStatus> {
        self.last
    }

    /// The verdict for an attempt at `now` that failed with `status`.
    ///
    /// Only the retried status is retried, with the next attempt a wait after
    /// `now` (the wait doubles up to the longest), clipped to the bound.
    pub(crate) fn failed(&mut self, status: OsStatus, now: Instant) -> Verdict {
        if self.single_attempt || status != self.retried {
            return Verdict::Fatal;
        }
        self.last = Some(status);
        self.next_at = (now + self.wait).min(self.bound_at);
        self.wait = (self.wait * 2).min(self.longest_wait);
        Verdict::Retry
    }
}

/// Why a polled retry ended without a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure<E> {
    /// An attempt failed with an error the rule does not retry.
    Failed(E),
    /// The bound passed; `last` is the status that kept failing, if an attempt ran.
    TimedOut {
        /// The last status retried.
        last: Option<OsStatus>,
    },
}

/// One poll of a retried call: makes `attempt` only when `backoff` says one is
/// due at `now`.
///
/// `status` is the `OsStatus` an error carries, which is what the rule filters
/// on; an error with none is final.
pub(crate) fn poll_attempt<T, E>(
    backoff: &mut Backoff,
    now: Instant,
    attempt: impl FnOnce() -> Result<T, E>,
    status: impl Fn(&E) -> Option<OsStatus>,
) -> Poll<Result<T, Failure<E>>> {
    match backoff.due(now) {
        Due::Wait => Poll::Pending,
        Due::TimedOut => Poll::Ready(Err(Failure::TimedOut {
            last: backoff.last(),
        })),
        Due::Attempt => match attempt() {
            Ok(done) => Poll::Ready(Ok(done)),
            Err(error) => match status(&error) {
                Some(status) if backoff.failed(status, now) == Verdict::Retry => Poll::Pending,
                _ => Poll::Ready(Err(Failure::Failed(error))),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::midi::ms;

    const SERVER_GONE: OsStatus = OsStatus::SERVER_GONE;
    const REFUSED: OsStatus = OsStatus::NOT_PERMITTED;

    /// The waits a rule schedules for `count` failures of `status`, each at the
    /// instant the previous one made due.
    fn waits(mut rule: Backoff, status: OsStatus, count: usize, start: Instant) -> Vec<Duration> {
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..count {
            assert_eq!(rule.due(now), Due::Attempt);
            assert_eq!(rule.failed(status, now), Verdict::Retry);
            waits.push(rule.deadline() - now);
            now = rule.deadline();
        }
        waits
    }

    #[test]
    fn each_rule_retries_with_doubling_waits_up_to_its_cap() {
        let start = Instant::now();
        for (name, rule, status, count, expected) in [
            (
                "client, a gone server, capped at one second",
                Backoff::client(start),
                SERVER_GONE,
                6,
                vec![ms(100), ms(200), ms(400), ms(800), ms(1000), ms(1000)],
            ),
            (
                "endpoint, a refused enable, capped at 200 ms",
                Backoff::endpoint(start),
                REFUSED,
                7,
                vec![ms(10), ms(20), ms(40), ms(80), ms(160), ms(200), ms(200)],
            ),
        ] {
            assert_eq!(waits(rule, status, count, start), expected, "{name}");
        }
    }

    #[test]
    fn the_first_attempt_is_due_at_the_start_and_not_before() {
        let start = Instant::now();
        let rule = Backoff::client(start);
        assert_eq!(rule.deadline(), start);
        assert_eq!(rule.due(start), Due::Attempt);
    }

    #[test]
    fn the_bound_is_a_timeout_at_the_bound_and_an_attempt_a_nanosecond_before() {
        let start = Instant::now();
        for (rule, bound) in [
            (Backoff::client(start), Duration::from_secs(10)),
            (Backoff::endpoint(start), Duration::from_secs(2)),
        ] {
            assert_ne!(
                rule.due(start + bound - Duration::from_nanos(1)),
                Due::TimedOut
            );
            assert_eq!(rule.due(start + bound), Due::TimedOut);
            assert_eq!(rule.due(start + bound + ms(1)), Due::TimedOut);
        }
    }

    #[test]
    fn the_deadline_never_decreases_and_never_passes_the_bound() {
        let start = Instant::now();
        for (mut rule, status, bound) in [
            (Backoff::client(start), SERVER_GONE, Duration::from_secs(10)),
            (Backoff::endpoint(start), REFUSED, Duration::from_secs(2)),
        ] {
            let mut now = start;
            let mut before = rule.deadline();
            while rule.due(now) == Due::Attempt {
                assert_eq!(rule.failed(status, now), Verdict::Retry);
                assert!(rule.deadline() >= before);
                assert!(rule.deadline() <= start + bound);
                before = rule.deadline();
                now = rule.deadline();
            }
            assert_eq!(now, start + bound, "the last wait is clipped to the bound");
            assert_eq!(rule.due(now), Due::TimedOut);
        }
    }

    #[test]
    fn a_poll_before_the_deadline_waits() {
        let start = Instant::now();
        let mut rule = Backoff::client(start);
        assert_eq!(rule.failed(SERVER_GONE, start), Verdict::Retry);
        assert_eq!(rule.due(start + ms(99)), Due::Wait);
        assert_eq!(rule.due(start + ms(100)), Due::Attempt);
    }

    #[test]
    fn each_rule_retries_only_its_own_status() {
        let start = Instant::now();
        for raw in [-1, 0, -303, -305, -10839, -10843, i32::MIN, i32::MAX] {
            let status = OsStatus::new(raw);
            assert_eq!(
                Backoff::client(start).failed(status, start),
                Verdict::Fatal,
                "client, status {status}"
            );
            assert_eq!(
                Backoff::endpoint(start).failed(status, start),
                Verdict::Fatal,
                "enable, status {status}"
            );
        }
        assert_eq!(
            Backoff::client(start).failed(REFUSED, start),
            Verdict::Fatal,
            "the client does not retry a refusal"
        );
        assert_eq!(
            Backoff::endpoint(start).failed(SERVER_GONE, start),
            Verdict::Fatal,
            "the enable does not retry a gone server"
        );
    }

    #[test]
    fn a_single_attempt_rule_is_fatal_after_one_failure() {
        let start = Instant::now();
        let mut rule = Backoff::client(start).single_attempt();
        assert_eq!(rule.failed(SERVER_GONE, start), Verdict::Fatal);
    }

    #[test]
    fn the_client_rule_covers_a_five_second_server_restart() {
        let start = Instant::now();
        let mut rule = Backoff::client(start);
        let mut now = start;
        while now < start + Duration::from_secs(5) {
            assert_eq!(
                rule.due(now),
                Due::Attempt,
                "gave up before the server restarted"
            );
            assert_eq!(rule.failed(SERVER_GONE, now), Verdict::Retry);
            now = rule.deadline();
        }
    }

    #[test]
    fn poll_attempt_makes_no_attempt_before_the_deadline_or_at_the_bound() {
        let start = Instant::now();
        let mut rule = Backoff::client(start);
        let mut calls = 0;
        let mut fail = || {
            calls += 1;
            Err::<(), _>(SERVER_GONE)
        };
        assert_eq!(
            poll_attempt(&mut rule, start, &mut fail, |s| Some(*s)),
            Poll::Pending
        );
        assert_eq!(
            poll_attempt(&mut rule, start + ms(99), &mut fail, |s| Some(*s)),
            Poll::Pending
        );
        assert_eq!(
            poll_attempt(&mut rule, start + CLIENT_BOUND, &mut fail, |s| Some(*s)),
            Poll::Ready(Err(Failure::TimedOut {
                last: Some(SERVER_GONE)
            }))
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn poll_attempt_returns_an_error_without_a_status_at_once() {
        let start = Instant::now();
        let mut rule = Backoff::endpoint(start);
        assert_eq!(
            poll_attempt(&mut rule, start, || Err::<(), _>("no status"), |_| None),
            Poll::Ready(Err(Failure::Failed("no status")))
        );
    }
}
