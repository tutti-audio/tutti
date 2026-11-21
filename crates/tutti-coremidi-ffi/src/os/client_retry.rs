//! Creating a CoreMIDI client while the MIDI server may be starting or exiting.
//!
//! CoreMIDI starts its server on demand and the server exits when idle. A client
//! created while the server is shutting down fails with `SERVER_GONE`, and a
//! fresh server is up about five seconds later. [`create_client`] retries it with
//! the client's [`Backoff`] and remembers a window that ended in failure.

use super::error::OsStatus;
use super::retry::{Backoff, CLIENT_BOUND, retry};
use std::sync::Mutex;
use tutti_platform::Instant;

/// The rule for a creation whose first attempt starts at `start`, and whether it
/// has a retry window.
///
/// `gave_up` is the instant an earlier creation's window ended in failure. Within
/// [`CLIENT_BOUND`] of it the server is still known to be gone, so this creation
/// makes its one attempt and does not retry; after that it gets a full window
/// again.
fn rule(start: Instant, gave_up: Option<Instant>) -> (Backoff, bool) {
    let suppressed = gave_up.is_some_and(|at| start < at + CLIENT_BOUND);
    let backoff = Backoff::client(start);
    if suppressed {
        (backoff.without_window(start), false)
    } else {
        (backoff, true)
    }
}

/// Runs `create` until it succeeds, fails with a status other than
/// the server's exit, or `CLIENT_BOUND` passes; returns the last status then.
///
/// [`SharedClient::start`](crate::SharedClient::start) creates the process's client
/// through here. A window that ends
/// in failure is remembered process-wide: creations within `CLIENT_BOUND` after
/// it make one attempt each, so a persistent failure costs one wait per window.
///
/// # Errors
///
/// The status of the last attempt.
pub(crate) fn create_client<T>(create: impl FnMut() -> Result<T, OsStatus>) -> Result<T, OsStatus> {
    static GAVE_UP: Mutex<Option<Instant>> = Mutex::new(None);
    let gave_up = *GAVE_UP.lock().unwrap_or_else(|p| p.into_inner());
    let (backoff, windowed) = rule(Instant::now(), gave_up);
    let result = retry(backoff, create, |status| Some(*status));
    if windowed && result.as_ref().err() == Some(&OsStatus::SERVER_GONE) {
        *GAVE_UP.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::retry::Retry;
    use super::*;
    use std::time::Duration;

    const SERVER_GONE: OsStatus = OsStatus::SERVER_GONE;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
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
        for later in [ms(0), ms(5_000), CLIENT_BOUND - ms(1)] {
            let (mut backoff, windowed) = rule(start + later, Some(gave_up));
            assert!(!windowed);
            assert_eq!(backoff.after(SERVER_GONE, start + later), Retry::GiveUp);
        }
    }

    #[test]
    fn a_creation_after_the_bound_of_a_failed_window_gets_a_full_window() {
        let start = Instant::now();
        let (mut backoff, windowed) = rule(start + CLIENT_BOUND, Some(start));
        assert!(windowed);
        assert_eq!(
            backoff.after(SERVER_GONE, start + CLIENT_BOUND),
            Retry::At(start + CLIENT_BOUND + ms(100))
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
