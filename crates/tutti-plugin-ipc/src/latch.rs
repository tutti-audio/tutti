//! What a task on the executor shares with its owner: a latch the task raises
//! as it ends, however it ends, and the yield that lets the executor look at
//! the task between two slices of blocking work.

use crate::Wake;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Raised once, by the end of a task: the owner's wait for the task is a wait on
/// this, an event, and never a poll.
#[derive(Debug, Default)]
pub(crate) struct Latch {
    raised: AtomicBool,
    gate: Wake,
}

impl Latch {
    /// Whether the task has ended.
    pub(crate) fn is_raised(&self) -> bool {
        self.raised.load(Ordering::Acquire)
    }

    /// Waits up to `timeout` for the task to end: `true` when it has.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        self.gate
            .wait_until(Wake::deadline_in(timeout), || self.is_raised())
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
        self.0.raised.store(true, Ordering::Release);
        self.0.gate.notify();
    }
}

/// Returns to the executor once, which sees a dropped task or a shut-down
/// executor between two slices of blocking work.
pub(crate) async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}
