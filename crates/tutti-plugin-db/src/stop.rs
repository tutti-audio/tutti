//! [`Stop`]: the request that ends a probe in flight.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tutti_rt::LockExt;

#[derive(Debug, Default)]
struct Latch {
    stopped: Mutex<bool>,
    raised: Condvar,
}

/// A request to stop, raised once and never lowered.
///
/// A probe holds a `Stop` and waits on it: [`stop`](Self::stop) wakes every
/// waiter at once, and a wait that starts after the request returns at once.
/// Clones share one latch.
#[derive(Debug, Clone, Default)]
pub struct Stop {
    latch: Arc<Latch>,
}

impl Stop {
    /// A request that has not been raised.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the request was raised.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        *self.latch.stopped.lock_recover()
    }

    /// Raises the request and wakes every waiter. Idempotent.
    pub fn stop(&self) {
        *self.latch.stopped.lock_recover() = true;
        self.latch.raised.notify_all();
    }

    /// Waits for the request for at most `timeout` and returns whether it was
    /// raised. Returns at once when it already was.
    #[must_use = "true means the probe should end"]
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.latch.stopped.lock_recover();
        let (guard, _) = self
            .latch
            .raised
            .wait_timeout_while(guard, timeout, |stopped| !*stopped)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Far past anything a test waits for: a hang guard, never a speed claim.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// **A raised request ends a wait that is already parked**, and the waiter
    /// reports it: the wait returns long before its own timeout.
    #[test]
    fn stop_wakes_a_parked_wait() {
        let stop = Stop::new();
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let waiter = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                parked_tx.send(()).unwrap();
                stop.wait(HANG_GUARD)
            })
        };
        parked_rx.recv().unwrap();
        // Yields give the waiter the processor to park on the condvar before the
        // request; the request is a latch, so a waiter that has not parked yet
        // still returns, and only a parked one needs the wake.
        for _ in 0..10_000 {
            std::thread::yield_now();
        }
        stop.stop();
        assert!(waiter.join().unwrap(), "the wait reports the request");
    }

    /// **A wait that starts after the request returns at once**: the request is
    /// a latch, so a stop raised before the wait is not missed.
    #[test]
    fn a_cancelled_wait_returns_at_once() {
        let stop = Stop::new();
        assert!(!stop.is_stopped());
        stop.stop();
        stop.stop();
        assert!(stop.is_stopped(), "raising twice is one request");
        assert!(
            stop.wait(HANG_GUARD),
            "returned without sleeping the timeout"
        );
    }

    /// A wait nobody raises ends at its timeout and reports no request.
    #[test]
    fn an_unraised_wait_times_out_without_a_request() {
        let stop = Stop::new();
        assert!(!stop.wait(Duration::from_millis(1)));
        assert!(!stop.is_stopped());
    }
}
