//! The executor's timer: [`Sleep`], the queue of deadlines behind it, and what fires it.
//!
//! [`Due`] is the timing rule: a method of `now` that returns the sleeps due then.
//! Its driver calls it with [`Instant::now`]: a timer thread that sleeps to the next
//! deadline, or the caller's [`Executor::poll`](super::Executor::poll).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use super::lock;
use crate::Instant;
use crate::thread::{ThreadHandle, spawn_named};

/// What fires the due sleeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Drive {
    /// A platform thread of the executor's own, started with the first sleep.
    Thread,
    /// The caller's `poll`.
    Poll,
}

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
struct State {
    due: Due,
    closed: bool,
}

/// The queue and the condvar a timer thread waits on.
#[derive(Default)]
struct Core {
    state: Mutex<State>,
    changed: Condvar,
}

impl Core {
    /// Wakes every sleep due at `now`; returns how many.
    fn fire(&self, now: Instant) -> usize {
        let due = lock(&self.state).due.take_due(now);
        let fired = due.len();
        for (_, waker) in due {
            waker.wake();
        }
        fired
    }

    /// A timer thread's body: wakes the due sleeps and waits for the next deadline
    /// until the queue closes.
    fn run(&self) {
        let mut state = lock(&self.state);
        loop {
            if state.closed {
                return;
            }
            let due = state.due.take_due(Instant::now());
            if !due.is_empty() {
                drop(state);
                for (_, waker) in due {
                    waker.wake();
                }
                state = lock(&self.state);
                continue;
            }
            state = match state.due.next_deadline() {
                None => self.changed.wait(state),
                Some(deadline) => {
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    match self.changed.wait_timeout(state, timeout) {
                        Ok((state, _)) => Ok(state),
                        Err(poisoned) => Err(PoisonError::new(poisoned.into_inner().0)),
                    }
                }
            }
            .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.due = Due::default();
        self.changed.notify_all();
    }
}

/// An executor's timers: the queue and its driver.
pub(super) struct Timers {
    core: Arc<Core>,
    drive: Drive,
    thread: Mutex<Option<ThreadHandle<()>>>,
}

impl std::fmt::Debug for Timers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timers")
            .field("drive", &self.drive)
            .finish_non_exhaustive()
    }
}

impl Timers {
    pub(super) fn new(drive: Drive) -> Self {
        Self {
            core: Arc::default(),
            drive,
            thread: Mutex::new(None),
        }
    }

    /// A sleep until `deadline`, starting the timer thread if this form has one and it
    /// has not started.
    pub(super) fn sleep_until(&self, deadline: Instant) -> io::Result<Sleep> {
        if self.drive == Drive::Thread {
            let mut thread = lock(&self.thread);
            if thread.is_none() {
                let core = Arc::clone(&self.core);
                *thread = Some(spawn_named("tutti-timer", None, move |_| core.run())?);
            }
        }
        Ok(Sleep {
            core: Arc::clone(&self.core),
            deadline,
            id: None,
        })
    }

    /// Wakes the sleeps due now when the caller drives the timers; nothing otherwise.
    pub(super) fn fire_if_polled(&self) {
        if self.drive == Drive::Poll {
            self.core.fire(Instant::now());
        }
    }
}

impl Drop for Timers {
    fn drop(&mut self) {
        self.core.close();
        drop(lock(&self.thread).take());
    }
}

/// A deadline as a future: resolves once [`Instant::now`] is at or past it.
///
/// Made by [`Executor::sleep_until`](super::Executor::sleep_until). Dropping a pending
/// `Sleep` removes it from the executor's timer and drops its waker. A `Sleep` whose
/// executor has dropped never resolves.
#[derive(Debug)]
#[must_use = "a Sleep does nothing unless awaited"]
pub struct Sleep {
    core: Arc<Core>,
    deadline: Instant,
    id: Option<u64>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Core")
    }
}

impl Sleep {
    /// The instant this sleep resolves at.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    fn cancel(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.core.state).due.remove(id);
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            self.cancel();
            return Poll::Ready(());
        }
        let core = Arc::clone(&self.core);
        let mut state = lock(&core.state);
        if !state.closed {
            let id = state.due.arm(self.id, self.deadline, cx.waker());
            if self.id.replace(id) != Some(id) {
                core.changed.notify_one();
            }
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
