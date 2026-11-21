//! What a task on the executor shares with its owner: a [`Latch`] the owner waits
//! on, and the [`Finish`] guard that raises it however the task ends.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tutti_rt::LockExt;

/// Raised once: a wait for the event it stands for is a wait on this, never a
/// poll. Raised by [`Finish`] when a task ends, and by [`raise`](Self::raise) for
/// a flag that is only set (the host is gone, the shutdown is under way).
#[derive(Debug, Default)]
pub(crate) struct Latch {
    raised: Mutex<bool>,
    gate: Condvar,
}

impl Latch {
    /// Raises the latch and wakes every waiter. Idempotent.
    pub(crate) fn raise(&self) {
        // Set under the gate, so a waiter that checked before the store is
        // already waiting on the condvar when the notify comes.
        *self.raised.lock_recover() = true;
        self.gate.notify_all();
    }

    /// Whether the latch is raised.
    pub(crate) fn is_raised(&self) -> bool {
        *self.raised.lock_recover()
    }

    /// Waits up to `timeout` for the latch: `true` when it is raised.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let raised = self.raised.lock_recover();
        let (raised, _) = self
            .gate
            .wait_timeout_while(raised, timeout, |raised| !*raised)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *raised
    }
}

/// Raises its [`Latch`] when dropped. A task's future holds one from its
/// creation, so the latch is raised by every way out of the task: its end, a
/// panic in it, its cancellation, and the drop of a future that was never polled.
#[derive(Debug)]
pub(crate) struct Finish(Arc<Latch>);

impl Finish {
    /// The guard of `latch`.
    pub(crate) fn new(latch: &Arc<Latch>) -> Self {
        Self(Arc::clone(latch))
    }
}

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.raise();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A future dropped before it is polled raises its latch**: the guard is
    /// held from the future's creation, so an owner waiting for a task the
    /// executor never ran is not left waiting.
    #[test]
    fn a_future_dropped_before_it_is_polled_raises_its_latch() {
        let latch = Arc::new(Latch::default());
        let finish = Finish::new(&latch);
        let future = async move {
            let _finish = finish;
        };
        assert!(!latch.is_raised());
        drop(future);
        assert!(latch.is_raised());
    }

    /// **A raise ends a wait that began before it.**
    #[test]
    fn a_raise_ends_a_wait_that_began_before_it() {
        let latch = Arc::new(Latch::default());
        let waiter = std::thread::spawn({
            let latch = Arc::clone(&latch);
            move || latch.wait(Duration::from_secs(600))
        });
        latch.raise();
        assert!(waiter.join().expect("the waiter"));
    }

    /// **A wait on a latch nobody raises gives up at its bound.**
    #[test]
    fn a_wait_on_an_unraised_latch_gives_up() {
        assert!(!Latch::default().wait(Duration::ZERO));
    }
}
