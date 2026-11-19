//! Creating a CoreMIDI client while the MIDI server may be starting or exiting.
//!
//! CoreMIDI starts its server on demand and the server exits when idle. A client
//! created while the server is shutting down fails with `SERVER_GONE`, and a
//! fresh server is up about five seconds later. `ClientRetry` decides when to
//! try again; [`create_client`] is the loop that reads the clock and sleeps.

use super::error::OsStatus;
use std::sync::Mutex;
use std::time::Duration;
use tutti_platform::Instant;

/// How long a client creation keeps retrying, measured from the first attempt.
/// It covers the server's restart of about five seconds with room to spare.
pub(crate) const RETRY_BOUND: Duration = Duration::from_secs(10);

/// The wait after the first failed attempt; each later wait doubles.
const FIRST_WAIT: Duration = Duration::from_millis(100);

/// The longest single wait.
const LONGEST_WAIT: Duration = Duration::from_secs(1);

/// What to do after a failed client creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Try again at this instant.
    At(Instant),
    /// Return the error.
    GiveUp,
}

/// The retry rule for one client creation: a pure function of the `now` it is
/// given, so the loop around it owns the clock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientRetry {
    deadline: Instant,
    wait: Duration,
    windowed: bool,
}

impl ClientRetry {
    /// A rule for a creation whose first attempt starts at `start`.
    ///
    /// `gave_up` is the instant an earlier creation's window ended in failure.
    /// Within [`RETRY_BOUND`] of it the server is still known to be gone, so this
    /// creation makes its one attempt and does not retry; after that it gets a
    /// full window again.
    pub(crate) fn new(start: Instant, gave_up: Option<Instant>) -> Self {
        let suppressed = gave_up.is_some_and(|at| start < at + RETRY_BOUND);
        Self {
            deadline: if suppressed {
                start
            } else {
                start + RETRY_BOUND
            },
            wait: FIRST_WAIT,
            windowed: !suppressed,
        }
    }

    /// Whether this creation had a retry window (it was not suppressed).
    pub(crate) fn windowed(&self) -> bool {
        self.windowed
    }

    /// The decision for an attempt that failed with `status` at `now`.
    ///
    /// Only [`OsStatus::SERVER_GONE`] is retried, with waits of 100 ms doubling to 1 s,
    /// the last one clipped to the bound; at or past the bound it gives up.
    pub(crate) fn after(&mut self, status: OsStatus, now: Instant) -> Retry {
        if status != OsStatus::SERVER_GONE || now >= self.deadline {
            return Retry::GiveUp;
        }
        let next = (now + self.wait).min(self.deadline);
        self.wait = (self.wait * 2).min(LONGEST_WAIT);
        Retry::At(next)
    }
}

/// Runs `create` until it succeeds, fails with a status other than
/// the server's exit, or `RETRY_BOUND` passes; returns the last status then.
///
/// The interim path of the ports' client in `tutti-midi-hardware`, which moves here
/// with the ports; every new caller takes [`SharedClient`](crate::SharedClient)
/// instead, which retries through this.
///
/// Every client creation in the process goes through here. A window that ends
/// in failure is remembered process-wide: creations within `RETRY_BOUND` after
/// it make one attempt each, so a persistent failure costs one wait per window.
///
/// # Errors
///
/// The status of the last attempt.
pub fn create_client<T>(mut create: impl FnMut() -> Result<T, OsStatus>) -> Result<T, OsStatus> {
    static GAVE_UP: Mutex<Option<Instant>> = Mutex::new(None);
    let gave_up = *GAVE_UP.lock().unwrap_or_else(|p| p.into_inner());
    let mut rule = ClientRetry::new(Instant::now(), gave_up);
    loop {
        let status = match create() {
            Ok(client) => return Ok(client),
            Err(status) => status,
        };
        match rule.after(status, Instant::now()) {
            Retry::At(at) => std::thread::sleep(at.saturating_duration_since(Instant::now())),
            Retry::GiveUp => {
                if status == OsStatus::SERVER_GONE && rule.windowed() {
                    *GAVE_UP.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                }
                return Err(status);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER_GONE: OsStatus = OsStatus::SERVER_GONE;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn retries_a_gone_server_with_doubling_waits_capped_at_one_second() {
        let start = Instant::now();
        let mut rule = ClientRetry::new(start, None);
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..6 {
            let Retry::At(at) = rule.after(SERVER_GONE, now) else {
                panic!("gave up inside the bound");
            };
            waits.push(at - now);
            now = at;
        }
        assert_eq!(
            waits,
            [ms(100), ms(200), ms(400), ms(800), ms(1000), ms(1000)]
        );
    }

    #[test]
    fn gives_up_at_the_bound_and_clips_the_last_wait_to_it() {
        let start = Instant::now();
        let mut rule = ClientRetry::new(start, None);
        let near = start + RETRY_BOUND - ms(50);
        assert_eq!(
            rule.after(SERVER_GONE, near),
            Retry::At(start + RETRY_BOUND)
        );
        assert_eq!(rule.after(SERVER_GONE, start + RETRY_BOUND), Retry::GiveUp);
        assert_eq!(
            rule.after(SERVER_GONE, start + RETRY_BOUND + ms(1)),
            Retry::GiveUp
        );
    }

    #[test]
    fn covers_a_five_second_server_restart() {
        let start = Instant::now();
        let mut rule = ClientRetry::new(start, None);
        let mut now = start;
        while now < start + Duration::from_secs(5) {
            let Retry::At(at) = rule.after(SERVER_GONE, now) else {
                panic!("gave up before the server could restart");
            };
            now = at;
        }
    }

    #[test]
    fn never_retries_another_status() {
        let start = Instant::now();
        for raw in [-1, 0, -10839, -10833, -303, -305, i32::MIN, i32::MAX] {
            let status = OsStatus::new(raw);
            let mut rule = ClientRetry::new(start, None);
            assert_eq!(rule.after(status, start), Retry::GiveUp, "status {status}");
        }
    }

    #[test]
    fn create_client_returns_a_non_retryable_status_at_once() {
        let mut calls = 0;
        let result: Result<(), OsStatus> = create_client(|| {
            calls += 1;
            Err(OsStatus::new(-10839))
        });
        assert_eq!(result, Err(OsStatus::new(-10839)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_creation_within_the_bound_of_a_failed_window_makes_one_attempt() {
        let start = Instant::now();
        let gave_up = start;
        for later in [ms(0), ms(5_000), RETRY_BOUND - ms(1)] {
            let mut rule = ClientRetry::new(start + later, Some(gave_up));
            assert!(!rule.windowed());
            assert_eq!(rule.after(SERVER_GONE, start + later), Retry::GiveUp);
        }
    }

    #[test]
    fn a_creation_after_the_bound_of_a_failed_window_gets_a_full_window() {
        let start = Instant::now();
        let mut rule = ClientRetry::new(start + RETRY_BOUND, Some(start));
        assert!(rule.windowed());
        assert_eq!(
            rule.after(SERVER_GONE, start + RETRY_BOUND),
            Retry::At(start + RETRY_BOUND + ms(100))
        );
    }

    #[test]
    fn create_client_retries_a_gone_server_until_it_answers() {
        let mut calls = 0;
        let result = create_client(|| {
            calls += 1;
            if calls == 1 { Err(SERVER_GONE) } else { Ok(7) }
        });
        assert_eq!(result, Ok(7));
        assert_eq!(calls, 2);
    }
}
