//! Creating a CoreMIDI client while the MIDI server may be starting or exiting.
//!
//! CoreMIDI starts its server on demand and the server exits when idle. A client
//! created while the server is shutting down fails with `SERVER_GONE`, and a
//! fresh server is up about five seconds later. [`ClientStart`] is the state of
//! one creation: polled with the creating call, it makes an attempt only when the
//! client's [`Backoff`] says one is due, and remembers a window that ended in
//! failure.

use super::backoff::{Backoff, CLIENT_BOUND, Failure, poll_attempt};
use super::os_status::OsStatus;
use std::task::Poll;
use tutti_platform::Instant;

/// The rule for a creation whose first attempt starts at `start`, and whether it
/// has a retry window.
///
/// `gave_up` is the instant an earlier creation's window ended in failure. Within
/// [`CLIENT_BOUND`] of it the server is still known to be gone, so this creation
/// makes its one attempt and does not retry; after that it gets a full window
/// again.
pub(crate) fn rule(start: Instant, gave_up: Option<Instant>) -> (Backoff, bool) {
    let suppressed = gave_up.is_some_and(|at| start < at + CLIENT_BOUND);
    let backoff = Backoff::client(start);
    if suppressed {
        (backoff.single_attempt(), false)
    } else {
        (backoff, true)
    }
}

/// One client creation in progress.
#[derive(Debug)]
pub(crate) struct ClientStart {
    backoff: Backoff,
    windowed: bool,
    gave_up_at: Option<Instant>,
}

impl ClientStart {
    /// A creation whose first attempt is due at `start`, after a window that
    /// ended in failure at `gave_up` (see [`rule`]).
    pub(crate) fn new(start: Instant, gave_up: Option<Instant>) -> Self {
        let (backoff, windowed) = rule(start, gave_up);
        Self {
            backoff,
            windowed,
            gave_up_at: None,
        }
    }

    /// When the next poll can make progress.
    pub(crate) fn deadline(&self) -> Instant {
        self.backoff.deadline()
    }

    /// The instant this creation's window ended in failure, once it has: the
    /// server was gone for the whole window.
    pub(crate) fn gave_up_at(&self) -> Option<Instant> {
        self.gave_up_at
    }

    /// One poll at `now`: runs `create` only when an attempt is due.
    pub(crate) fn poll<T>(
        &mut self,
        now: Instant,
        create: impl FnOnce() -> Result<T, OsStatus>,
    ) -> Poll<Result<T, Failure<OsStatus>>> {
        let polled = poll_attempt(&mut self.backoff, now, create, |status| Some(*status));
        if self.windowed && matches!(polled, Poll::Ready(Err(Failure::TimedOut { .. }))) {
            self.gave_up_at = Some(now);
        }
        polled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::backoff::Verdict;
    use std::time::Duration;

    const SERVER_GONE: OsStatus = OsStatus::SERVER_GONE;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A creation that fails `failures` times with `status`, then makes 7, and
    /// the attempts it was asked for.
    fn creator(
        failures: usize,
        status: OsStatus,
    ) -> (
        impl FnMut() -> Result<i32, OsStatus>,
        std::rc::Rc<std::cell::Cell<usize>>,
    ) {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = std::rc::Rc::clone(&calls);
        (
            move || {
                calls.set(calls.get() + 1);
                if calls.get() <= failures {
                    Err(status)
                } else {
                    Ok(7)
                }
            },
            seen,
        )
    }

    #[test]
    fn a_creation_within_the_bound_of_a_failed_window_makes_one_attempt() {
        let start = Instant::now();
        for later in [ms(0), ms(5_000), CLIENT_BOUND - ms(1)] {
            let (mut backoff, windowed) = rule(start + later, Some(start));
            assert!(!windowed);
            assert_eq!(backoff.failed(SERVER_GONE, start + later), Verdict::Fatal);
        }
    }

    #[test]
    fn a_creation_after_the_bound_of_a_failed_window_gets_a_full_window() {
        let start = Instant::now();
        let (mut backoff, windowed) = rule(start + CLIENT_BOUND, Some(start));
        assert!(windowed);
        assert_eq!(
            backoff.failed(SERVER_GONE, start + CLIENT_BOUND),
            Verdict::Retry
        );
        assert_eq!(backoff.deadline(), start + CLIENT_BOUND + ms(100));
    }

    #[test]
    fn a_failed_first_attempt_is_pending_until_the_first_wait_has_passed() {
        let t0 = Instant::now();
        let mut start = ClientStart::new(t0, None);
        let (mut create, calls) = creator(1, SERVER_GONE);
        assert_eq!(start.poll(t0, &mut create), Poll::Pending);
        assert_eq!(start.deadline(), t0 + ms(100));
        assert_eq!(start.poll(t0 + ms(99), &mut create), Poll::Pending);
        assert_eq!(
            calls.get(),
            1,
            "a poll before the deadline makes no attempt"
        );
        assert_eq!(
            start.poll(start.deadline(), &mut create),
            Poll::Ready(Ok(7))
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_window_that_runs_out_is_a_timeout_with_no_attempt_and_is_remembered() {
        let t0 = Instant::now();
        let mut start = ClientStart::new(t0, None);
        let (mut create, calls) = creator(usize::MAX, SERVER_GONE);
        assert_eq!(start.poll(t0, &mut create), Poll::Pending);
        let attempts = calls.get();
        assert_eq!(start.gave_up_at(), None);
        assert_eq!(
            start.poll(t0 + CLIENT_BOUND, &mut create),
            Poll::Ready(Err(Failure::TimedOut { last: SERVER_GONE }))
        );
        assert_eq!(calls.get(), attempts, "the timeout makes no attempt");
        assert_eq!(start.gave_up_at(), Some(t0 + CLIENT_BOUND));
    }

    #[test]
    fn a_creation_with_no_window_fails_at_once_and_remembers_nothing() {
        let t0 = Instant::now();
        let mut start = ClientStart::new(t0 + ms(1), Some(t0));
        let (mut create, calls) = creator(usize::MAX, SERVER_GONE);
        assert_eq!(
            start.poll(t0 + ms(1), &mut create),
            Poll::Ready(Err(Failure::Failed(SERVER_GONE)))
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(start.gave_up_at(), None);
    }

    #[test]
    fn a_creation_with_no_window_first_polled_at_the_bound_is_not_a_failed_window() {
        let t0 = Instant::now();
        let mut start = ClientStart::new(t0 + ms(1), Some(t0));
        let (mut create, calls) = creator(usize::MAX, SERVER_GONE);
        assert_eq!(
            start.poll(t0 + ms(1) + CLIENT_BOUND, &mut create),
            Poll::Ready(Err(Failure::TimedOut { last: SERVER_GONE }))
        );
        assert_eq!(calls.get(), 0);
        assert_eq!(
            start.gave_up_at(),
            None,
            "only a full window that ran out is remembered"
        );
    }

    #[test]
    fn a_status_the_rule_does_not_retry_is_returned_at_once() {
        let t0 = Instant::now();
        let mut start = ClientStart::new(t0, None);
        let (mut create, calls) = creator(usize::MAX, OsStatus::new(-10839));
        assert_eq!(
            start.poll(t0, &mut create),
            Poll::Ready(Err(Failure::Failed(OsStatus::new(-10839))))
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(start.gave_up_at(), None);
    }

    #[test]
    fn a_server_that_is_back_within_five_seconds_is_waited_for() {
        let t0 = Instant::now();
        let mut start = ClientStart::new(t0, None);
        let mut now = t0;
        let mut calls = 0;
        // The server answers from 5 s on.
        let outcome = loop {
            let polled = start.poll(now, || {
                calls += 1;
                if now >= t0 + Duration::from_secs(5) {
                    Ok(1)
                } else {
                    Err(SERVER_GONE)
                }
            });
            if let Poll::Ready(done) = polled {
                break done;
            }
            now = start.deadline();
        };
        assert_eq!(outcome, Ok(1));
        assert!(calls > 1);
    }
}
