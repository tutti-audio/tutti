//! The web's executor edge: a page starts no threads, so the browser's own queue drives the
//! executor. A blocking closure is a task of the queue, and a dedicated future is queued
//! like any other.
//!
//! [`Executor::drive_on_page`] hands one driver future to the browser
//! (`wasm_bindgen_futures::spawn_local`). The driver is a loop of turns: it fires the due
//! sleeps, runs a bounded batch of ready tasks ([`BATCH`]), and then either yields to the
//! page with a zero timeout (the batch was full, so more may be ready) or waits for the
//! first of a task being scheduled, a sleep armed before the earliest deadline, that
//! deadline (a browser timeout), and the stop. It never spins and never blocks: a wait is a
//! pending future the browser wakes. A blocking closure runs on the page's thread, inside
//! a turn, for as long as it takes.
//!
//! Every piece of the driver's state lives in the executor ([`Own`], [`Parked`]) or the
//! [`PageDriver`] it returns: two executors on one page drive themselves independently.
//! Stopping (the [`PageDriver`]'s drop, or the executor's) ends the driver, which drops its
//! pending timeout and so clears it.

use std::future::{self, Future, poll_fn};
use std::io;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_lite::future::or;
use gloo_timers::future::TimeoutFuture;

use super::{Executor, Handle, Shared, Task, Unjoined, lock};
use crate::Instant;
use crate::priority::PriorityRequest;

/// The most tasks a turn runs before it yields to the page, so a long or self-waking queue
/// never holds the page's own tasks off.
const BATCH: usize = 64;

/// The longest a browser timeout waits, in milliseconds: a wake with nothing due only
/// loops. Under the `setTimeout` bound of `i32::MAX` milliseconds.
const LONGEST_WAIT_MS: u128 = 3_600_000;

/// One page driver's stop: closed once, then closed for good. It holds the one waker of the
/// driver that waits on it, replaced on each poll, so waiting any number of turns holds
/// one waker.
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, Option<Waker>)>,
}

impl Gate {
    fn close(&self) {
        let waker = {
            let mut state = lock(&self.state);
            state.0 = true;
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).0
    }

    /// Resolves once the gate is closed.
    fn closed(&self) -> impl Future<Output = ()> + '_ {
        poll_fn(|cx| {
            let mut state = lock(&self.state);
            if state.0 {
                return Poll::Ready(());
            }
            match &mut state.1 {
                Some(waker) if waker.will_wake(cx.waker()) => {}
                slot => *slot = Some(cx.waker().clone()),
            }
            Poll::Pending
        })
    }
}

/// The web cannot block: one look at `task`, its output when it has ended.
pub(super) fn join<T>(task: Task<T>) -> Option<T> {
    let mut ended = pin!(task.fallible());
    match ended.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => None,
    }
}

/// The web cannot block: one look at `task`, handed back when it has not ended.
pub(super) fn join_within<T>(task: Task<T>, _bound: Duration) -> Result<Option<T>, Unjoined<T>> {
    if !task.is_finished() {
        return Err(Unjoined::TimedOut(task));
    }
    Ok(join(task))
}

/// The page driver's nudge: a sleep armed before the earliest deadline wakes it, so it
/// re-reads the deadline.
#[derive(Default)]
pub(super) struct Parked {
    nudged: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Parked {
    pub(super) fn unpark_all(&self) {
        self.nudged.store(true, Ordering::Release);
        if let Some(waker) = lock(&self.waker).take() {
            waker.wake();
        }
    }

    fn clear(&self) {
        self.nudged.store(false, Ordering::Release);
    }

    /// Resolves once nudged since the last [`clear`](Self::clear).
    fn nudged(&self) -> impl Future<Output = ()> + '_ {
        poll_fn(|cx| {
            if self.nudged.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            *lock(&self.waker) = Some(cx.waker().clone());
            if self.nudged.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            Poll::Pending
        })
    }
}

/// The executor's side of its page drivers: every live driver's gate, which its shutdown
/// closes.
#[derive(Default)]
pub(super) struct Own {
    pages: Mutex<Vec<Weak<Gate>>>,
}

impl Own {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Stops every page driver of the executor.
    pub(super) fn shut_down(&self, _shared: &Shared) {
        let pages = std::mem::take(&mut *lock(&self.pages));
        for gate in pages.iter().filter_map(Weak::upgrade) {
            gate.close();
        }
    }
}

/// The web half of [`super::PageDriver`]: the gate that stops its driver.
pub(super) struct Page {
    stop: Arc<Gate>,
}

impl Drop for Page {
    fn drop(&mut self) {
        self.stop.close();
    }
}

impl Executor {
    /// Hands the browser's queue one future that drives this executor until the returned
    /// [`PageDriver`](super::PageDriver) drops or the executor shuts down. The web's
    /// counterpart of `spawn_threads`; the web only.
    ///
    /// Call it from the page's thread. A second call starts a second driver with a gate of
    /// its own: the executor's single nudge wakes one of them, which is enough, since any
    /// driver fires the due sleeps and runs the tasks.
    pub fn drive_on_page(&self) -> super::PageDriver {
        let shared = Arc::clone(&self.handle.shared);
        let stop = Arc::new(Gate::default());
        {
            let mut pages = lock(&self.handle.own.pages);
            pages.retain(|gate| gate.strong_count() > 0);
            pages.push(Arc::downgrade(&stop));
        }
        wasm_bindgen_futures::spawn_local(drive(shared, Arc::clone(&stop)));
        super::PageDriver {
            _page: Page { stop },
        }
    }
}

/// What woke a turn's wait.
enum Woke {
    Stop,
    Other,
}

/// The driver: turn after turn until stopped.
async fn drive(shared: Arc<Shared>, stop: Arc<Gate>) {
    while !stop.is_closed() {
        shared.timers.fire(Instant::now());
        let mut ran = 0;
        while ran < BATCH && shared.queue.try_tick() {
            ran += 1;
        }
        let stopped = async {
            stop.closed().await;
            Woke::Stop
        };
        if ran == BATCH {
            // More may be ready: let the page's own tasks in first.
            let yielded = async {
                TimeoutFuture::new(0).await;
                Woke::Other
            };
            if matches!(or(stopped, yielded).await, Woke::Stop) {
                return;
            }
            continue;
        }
        // Cleared before the deadline is read: a sleep armed after nudges the wait below.
        shared.timers.parked.clear();
        let next = lock(&shared.timers.state).due.next_deadline();
        let deadline = async {
            match next {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    // Rounded up: a timeout that fires early only loops once more.
                    let millis = left.as_millis().saturating_add(1).min(LONGEST_WAIT_MS);
                    TimeoutFuture::new(u32::try_from(millis).unwrap_or(u32::MAX)).await;
                }
                None => future::pending().await,
            }
            Woke::Other
        };
        let nudged = async {
            shared.timers.parked.nudged().await;
            Woke::Other
        };
        let task = async {
            shared.queue.tick().await;
            Woke::Other
        };
        if matches!(
            or(stopped, or(nudged, or(deadline, task))).await,
            Woke::Stop
        ) {
            return;
        }
    }
}

/// A blocking closure runs as a task of the queue, on the page's thread inside a driver
/// turn.
pub(super) fn unblock<T: Send + 'static>(
    handle: &Handle,
    f: impl FnOnce() -> T + Send + 'static,
) -> io::Result<Task<T>> {
    Ok(handle.shared.queue.spawn(async move { f() }))
}

/// A dedicated future is a queued one: there is no thread to give it, and no priority.
pub(super) fn spawn_dedicated<T: Send + 'static>(
    executor: &Executor,
    _name: &str,
    _priority: Option<PriorityRequest>,
    future: impl Future<Output = T> + Send + 'static,
) -> io::Result<Task<T>> {
    Ok(executor.spawn(future))
}

/// A future that is not `Send` needs a thread of its own, and a page starts none.
pub(super) fn spawn_dedicated_local<T, F>(
    _executor: &Executor,
    _name: &str,
    _priority: Option<PriorityRequest>,
    _make: impl FnOnce() -> F + Send + 'static,
) -> io::Result<Task<T>>
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a web page starts no threads for a future that is not Send",
    ))
}

/// No value: a web page starts no threads, so nothing builds a [`super::Threads`].
#[derive(Debug)]
pub(super) enum Threads {}
