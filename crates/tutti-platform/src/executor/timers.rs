//! The executor's timer: [`Sleep`] and the queue of deadlines behind it.
//!
//! [`Due`] is the timing rule: a method of `now` that returns the sleeps due then.
//! [`Timers::fire`] calls it with [`Instant::now`], from a driving thread's loop or from
//! [`Executor::run_until_stalled`](super::Executor::run_until_stalled) and
//! [`Executor::try_tick`](super::Executor::try_tick).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{lock, os};
use crate::Instant;

/// The sleeps waiting for a deadline.
#[derive(Default)]
pub(super) struct Due {
    /// Earliest deadline first. An entry whose id is not in `wakers` is stale.
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    wakers: HashMap<u64, Waker>,
    next_id: u64,
}

impl Due {
    /// Registers `waker` for `deadline`, or replaces the waker of `id` when it is still
    /// waiting. Returns the id the sleep now holds.
    pub(super) fn arm(&mut self, id: Option<u64>, deadline: Instant, waker: &Waker) -> u64 {
        if let Some(id) = id
            && let Some(slot) = self.wakers.get_mut(&id)
        {
            if !slot.will_wake(waker) {
                slot.clone_from(waker);
            }
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.heap.push(Reverse((deadline, id)));
        self.wakers.insert(id, waker.clone());
        id
    }

    /// Forgets `id`, dropping its waker.
    pub(super) fn remove(&mut self, id: u64) {
        self.wakers.remove(&id);
    }

    /// Removes and returns the sleeps whose deadline is at or before `now`, earliest first.
    pub(super) fn take_due(&mut self, now: Instant) -> Vec<(u64, Waker)> {
        let mut due = Vec::new();
        while let Some(&Reverse((deadline, id))) = self.heap.peek() {
            if deadline > now {
                break;
            }
            self.heap.pop();
            if let Some(waker) = self.wakers.remove(&id) {
                due.push((id, waker));
            }
        }
        due
    }

    /// The earliest deadline still waiting.
    pub(super) fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(&Reverse((deadline, id))) = self.heap.peek() {
            if self.wakers.contains_key(&id) {
                return Some(deadline);
            }
            self.heap.pop();
        }
        None
    }

    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.wakers.len()
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) due: Due,
    closed: bool,
}

/// An executor's deadline queue, and the threads that fire it and park until it
/// changes.
#[derive(Default)]
pub(super) struct Timers {
    pub(super) state: Mutex<State>,
    /// The threads parked for a task or a deadline: a deadline earlier than the
    /// queue's earliest unparks them. None on the web.
    pub(super) parked: os::Parked,
}

impl std::fmt::Debug for Timers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Timers")
    }
}

impl Timers {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Wakes every sleep due at `now`; returns how many.
    pub(super) fn fire(&self, now: Instant) -> usize {
        let due = lock(&self.state).due.take_due(now);
        let fired = due.len();
        for (_, waker) in due {
            waker.wake();
        }
        fired
    }

    /// Closes the queue and wakes every sleep waiting: each resolves on its next poll, and
    /// so does every sleep made or polled afterwards.
    pub(super) fn close(&self) {
        let due = {
            let mut state = lock(&self.state);
            state.closed = true;
            std::mem::take(&mut state.due)
        };
        for (_, waker) in due.wakers {
            waker.wake();
        }
    }
}

/// The executor a [`Sleep`] waits on shut down before its deadline came due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the executor has shut down")]
pub struct Shutdown;

/// A handle to an executor's deadlines that keeps nothing of the executor alive.
///
/// Made by [`Executor::timer`](super::Executor::timer). `Clone` shares it, and a future
/// may hold one: it holds the deadline queue only, never the executor, its threads or its
/// queue, so it never delays the executor's [shutdown](super::Executor#shutdown). Its
/// sleeps are the executor's own ([`Executor::sleep_until`](super::Executor::sleep_until)
/// is this type's).
///
/// A [`Sleep`] resolves at once with `Err(`[`Shutdown`]`)` once its executor has shut down;
/// nothing fires deadlines after that.
#[derive(Clone, Debug)]
pub struct Timer {
    timers: Arc<Timers>,
}

impl Timer {
    pub(super) fn new(timers: &Arc<Timers>) -> Self {
        Self {
            timers: Arc::clone(timers),
        }
    }

    /// A future that resolves once [`Instant::now`] is at or past `deadline`; a deadline
    /// already past resolves on its first poll.
    ///
    /// A driver of the executor fires it, so its resolution is the cadence of whoever
    /// drives. Dropping the [`Sleep`] before it resolves removes it from the queue. It
    /// resolves at once with `Err(`[`Shutdown`]`)` once the executor has shut down;
    /// nothing fires deadlines after that.
    pub fn sleep_until(&self, deadline: Instant) -> Sleep {
        Sleep::new(&self.timers, deadline)
    }

    /// A future that resolves `duration` from now: [`sleep_until`](Self::sleep_until) of
    /// that instant.
    pub fn sleep(&self, duration: Duration) -> Sleep {
        self.sleep_until(Instant::now() + duration)
    }
}

/// A deadline as a future: `Ok(())` when polled at or past its deadline,
/// `Err(`[`Shutdown`]`)` when polled before it once its executor has shut down. A driver
/// firing the deadline is what wakes it.
///
/// Made by [`Executor::sleep_until`](super::Executor::sleep_until) or
/// [`Timer::sleep_until`]. Dropping a pending
/// `Sleep` removes it from the executor's queue and drops its waker. A `Sleep` resolves at
/// once with `Err(Shutdown)` once its executor has shut down; nothing fires deadlines
/// after that.
#[derive(Debug)]
#[must_use = "a Sleep does nothing unless awaited"]
pub struct Sleep {
    timers: Arc<Timers>,
    deadline: Instant,
    id: Option<u64>,
}

impl Sleep {
    pub(super) fn new(timers: &Arc<Timers>, deadline: Instant) -> Self {
        Self {
            timers: Arc::clone(timers),
            deadline,
            id: None,
        }
    }

    /// The instant this sleep resolves at.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    fn cancel(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.timers.state).due.remove(id);
        }
    }
}

impl Future for Sleep {
    type Output = Result<(), Shutdown>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if Instant::now() >= self.deadline {
            self.cancel();
            return Poll::Ready(Ok(()));
        }
        let timers = Arc::clone(&self.timers);
        let mut state = lock(&timers.state);
        if state.closed {
            return Poll::Ready(Err(Shutdown));
        }
        let earliest = state.due.next_deadline();
        let id = state.due.arm(self.id, self.deadline, cx.waker());
        let armed = self.id.replace(id) != Some(id);
        drop(state);
        if armed && earliest.is_none_or(|earliest| self.deadline < earliest) {
            timers.parked.unpark_all();
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests;
