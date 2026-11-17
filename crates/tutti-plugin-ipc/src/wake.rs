//! [`Wake`]: a wait for a state that another thread changes, ended by the
//! change and not by a poll.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;
use tutti_rt::LockExt;

/// A gate a thread waits on until a condition holds, and that the threads which
/// change the condition notify.
///
/// A notifier changes the state first and then calls [`notify`](Self::notify),
/// which takes the gate: a waiter that checked the condition before the change
/// is parked on the condvar by then, so the notification cannot be missed. The
/// condition runs under the gate: it only reads the state.
///
/// Off the audio thread only: [`notify`](Self::notify) takes a lock.
#[derive(Debug, Default)]
pub struct Wake {
    gate: Mutex<()>,
    changed: Condvar,
}

impl Wake {
    /// Tells every waiter to check its condition again.
    pub fn notify(&self) {
        drop(self.gate.lock_recover());
        self.changed.notify_all();
    }

    /// Waits until `done` is true or `deadline` passes, checking `done` first
    /// and after each [`notify`](Self::notify). `true` when `done` held.
    pub fn wait_until(&self, deadline: Instant, mut done: impl FnMut() -> bool) -> bool {
        let gate = self.gate.lock_recover();
        let left = deadline.saturating_duration_since(Instant::now());
        let (_gate, timeout) = self
            .changed
            .wait_timeout_while(gate, left, |()| !done())
            .unwrap_or_else(PoisonError::into_inner);
        !timeout.timed_out()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// A change made before the wait is seen at once, and one made during it
    /// ends it, whichever thread runs first.
    #[test]
    fn a_change_and_its_notification_end_the_wait() {
        let wake = Arc::new(Wake::default());
        let flag = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (wake, flag) = (Arc::clone(&wake), Arc::clone(&flag));
            std::thread::spawn(move || {
                wake.wait_until(Instant::now() + Duration::from_secs(60), || {
                    flag.load(Ordering::Acquire)
                })
            })
        };
        flag.store(true, Ordering::Release);
        wake.notify();
        assert!(waiter.join().expect("no panic"), "ended by the change");
    }

    /// A condition that never holds ends at the deadline, reporting it.
    #[test]
    fn a_wait_past_its_deadline_reports_the_condition_unmet() {
        let wake = Wake::default();
        assert!(!wake.wait_until(Instant::now(), || false));
        assert!(wake.wait_until(Instant::now(), || true));
    }
}
