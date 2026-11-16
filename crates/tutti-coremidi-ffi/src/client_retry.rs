//! Creating a CoreMIDI client while the MIDI server may be starting or exiting.
//!
//! CoreMIDI starts its server on demand and the server exits when idle. A client
//! created while the server is shutting down fails with `SERVER_GONE`, and a
//! fresh server is up about five seconds later. `ClientRetry` decides when to
//! try again; [`create_client`] is the loop that reads the clock and sleeps.

use std::time::{Duration, Instant};

/// The `OSStatus` `MIDIClientCreate` returns while the MIDI server is exiting.
///
/// `MIDIServices.h` names no constant for it. The number is the Carbon code
/// `smNoDir` (`MacErrors.h`), which CoreMIDI reports when its Mach connection to
/// the server fails; this crate calls it by what it means here.
pub(crate) const SERVER_GONE: i32 = -304;

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
}

impl ClientRetry {
    /// A rule for a creation whose first attempt starts at `start`.
    pub(crate) fn new(start: Instant) -> Self {
        Self {
            deadline: start + RETRY_BOUND,
            wait: FIRST_WAIT,
        }
    }

    /// The decision for an attempt that failed with `status` at `now`.
    ///
    /// Only [`SERVER_GONE`] is retried, with waits of 100 ms doubling to 1 s,
    /// the last one clipped to the bound; at or past the bound it gives up.
    pub(crate) fn after(&mut self, status: i32, now: Instant) -> Retry {
        if status != SERVER_GONE || now >= self.deadline {
            return Retry::GiveUp;
        }
        let next = (now + self.wait).min(self.deadline);
        self.wait = (self.wait * 2).min(LONGEST_WAIT);
        Retry::At(next)
    }
}

/// Runs `create` until it succeeds, fails with a status other than
/// `SERVER_GONE`, or `RETRY_BOUND` passes; returns the last status then.
///
/// Every client creation in the process goes through here.
///
/// # Errors
///
/// The status of the last attempt.
pub fn create_client<T>(mut create: impl FnMut() -> Result<T, i32>) -> Result<T, i32> {
    let mut rule = ClientRetry::new(Instant::now());
    loop {
        let status = match create() {
            Ok(client) => return Ok(client),
            Err(status) => status,
        };
        match rule.after(status, Instant::now()) {
            Retry::At(at) => std::thread::sleep(at.saturating_duration_since(Instant::now())),
            Retry::GiveUp => return Err(status),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn retries_a_gone_server_with_doubling_waits_capped_at_one_second() {
        let start = Instant::now();
        let mut rule = ClientRetry::new(start);
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
        let mut rule = ClientRetry::new(start);
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
        let mut rule = ClientRetry::new(start);
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
        for status in [-1, 0, -10839, -10833, -303, -305, i32::MIN, i32::MAX] {
            let mut rule = ClientRetry::new(start);
            assert_eq!(rule.after(status, start), Retry::GiveUp, "status {status}");
        }
    }

    #[test]
    fn create_client_returns_a_non_retryable_status_at_once() {
        let mut calls = 0;
        let result: Result<(), i32> = create_client(|| {
            calls += 1;
            Err(-10839)
        });
        assert_eq!(result, Err(-10839));
        assert_eq!(calls, 1);
    }
}
