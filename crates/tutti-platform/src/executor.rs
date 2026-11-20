//! [`Executor`]: where tutti's background work runs.
//!
//! One type: a run queue of woken tasks, a queue of sleep deadlines, and a drive that
//! fires the due sleeps and runs the ready tasks. Whoever calls
//! [`run_until_stalled`](Executor::run_until_stalled) or
//! [`try_tick`](Executor::try_tick) drives it, and on a native target
//! [`spawn_threads`](Executor::spawn_threads) starts threads that drive it for good.
//! This is not an async runtime: it has no I/O reactor.

use std::fmt;
use std::future::Future;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::Instant;
use crate::priority::PriorityRequest;

mod timers;

pub use async_task::Task;
use timers::Timers;
pub use timers::{Shutdown, Sleep, Timer};

#[cfg_attr(not(target_arch = "wasm32"), path = "executor/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "executor/wasm.rs")]
mod os;

/// Threads that drive an [`Executor`]'s queue, started by `Executor::spawn_threads` on a
/// native target. The web has no constructor for it.
///
/// Each fires the due sleeps, runs a ready task, and parks until a task is scheduled or the
/// next deadline falls due. Dropping the `Threads` stops and joins them; a drop on one of
/// the threads itself (the last holder of a waker, say) does not join that thread. The
/// executor it drives stays alive while it does.
#[must_use = "dropping `Threads` stops and joins its threads"]
#[derive(Debug)]
pub struct Threads {
    /// Held for its drop, which stops and joins the threads.
    _threads: os::Threads,
}

/// A `Mutex`'s guard whether or not a panicking holder poisoned it. Each lock here
/// guards a queue or a slot that a panic leaves consistent.
fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs tutti's background work: a run queue, a queue of deadlines, and a drive.
///
/// `Clone` shares one executor. [`spawn`](Self::spawn) queues a future and returns its
/// [`Task`]: a future of the output that cancels the work when dropped (and
/// [`detach`](Task::detach)es it to run on). Nothing runs until something drives the
/// queue.
///
/// # Who drives
///
/// - **The caller.** [`run_until_stalled`](Self::run_until_stalled) fires every sleep that
///   is due and runs ready tasks on the calling thread until none is ready;
///   [`try_tick`](Self::try_tick) runs one. Neither waits. On the web the caller is the
///   only driver.
/// - **Threads** (native only). [`spawn_threads`](Self::spawn_threads) starts threads
///   that each fire the due sleeps, run a ready task, and park until a task is scheduled
///   or the next deadline. Dropping the returned `Threads` stops and joins them.
///
/// Blocking work does not run on a driver: [`unblock`](Self::unblock) runs a closure on a
/// small pool of its own (native), or inside the next drive (web), and
/// [`spawn_dedicated`](Self::spawn_dedicated) gives one long-lived future a thread of its
/// own (native), or queues it like [`spawn`](Self::spawn) (web).
///
/// # Shutdown
///
/// When the last clone drops, the dedicated threads are told to stop, woken and joined, the
/// blocking pool's idle threads end, and every task still pending is dropped. A future
/// that holds an `Executor` clone keeps the executor alive, so that drop never comes:
/// tutti's own futures never hold a clone, and a spawner passes a future what it needs
/// instead. A long-lived future ends on its own stop flag before the executor drops. A
/// future that needs deadlines holds a [`Timer`], which keeps nothing alive.
///
/// The executor's deadline queue closes with it: every [`Sleep`] pending wakes and resolves
/// `Err(`[`Shutdown`]`)`, and one made or polled later does at once; nothing fires deadlines after that.
///
/// # Not on the audio thread
///
/// No future is polled on the audio thread, and a [`Waker`](std::task::Waker)'s `wake`
/// may allocate or lock, so the audio thread never wakes one.
///
/// # Panics
///
/// A task that panics is dropped, and a [`Task`] awaited after that panics; the driving
/// thread carries on. So does a `Task` awaited after its executor dropped it at shutdown:
/// a caller that may outlive the executor awaits the task's fallible form from
/// async-task, which resolves `None` instead.
///
/// ```
/// use tutti_platform::Executor;
///
/// let executor = Executor::new();
/// let task = executor.spawn(async { 6 * 7 });
/// assert_eq!(executor.run_until_stalled(), 1);
/// assert!(task.is_finished());
/// ```
#[derive(Clone)]
pub struct Executor {
    handle: Arc<Handle>,
}

impl fmt::Debug for Executor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executor").finish_non_exhaustive()
    }
}

/// What the executor's clones share.
struct Handle {
    shared: Arc<Shared>,
    /// The threads the executor starts on its own account, and stops with it.
    own: os::Own,
}

/// The run queue and the deadline queue.
struct Shared {
    queue: async_executor::Executor<'static>,
    timers: Arc<Timers>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.own.shut_down(&self.shared);
        self.shared.timers.close();
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor {
    /// An executor with nothing running: no thread starts, and nothing runs until
    /// [`run_until_stalled`](Self::run_until_stalled) or
    /// [`try_tick`](Self::try_tick) is called, or threads are spawned.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handle: Arc::new(Handle {
                shared: Arc::new(Shared {
                    queue: async_executor::Executor::new(),
                    timers: Arc::new(Timers::new()),
                }),
                own: os::Own::new(),
            }),
        }
    }

    /// Queues `future` and returns its [`Task`].
    ///
    /// The task cancels the future when dropped; [`detach`](Task::detach) lets it run to
    /// completion unobserved. It runs when a driver gets to it.
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Task<T> {
        self.handle.shared.queue.spawn(future)
    }

    /// Runs `f`, a blocking call (a decode, a disk read, an IPC round trip), and returns
    /// its [`Task`].
    ///
    /// On a native target `f` runs on a small pool of threads started on demand, which
    /// end once idle, so a driver never blocks on it; it starts at once, whether or not
    /// anything drives the queue. On the web there are no threads: `f` runs inside the
    /// next [`run_until_stalled`](Self::run_until_stalled) or
    /// [`try_tick`](Self::try_tick), on the caller's thread, for as long as it takes.
    ///
    /// Dropping the task before `f` starts cancels it; one that has started finishes and
    /// its output is dropped.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start a pool thread when none is running to take `f`; `f` is
    /// dropped unrun. With a pool thread running, `f` waits for it instead.
    pub fn unblock<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<Task<T>> {
        os::unblock(&self.handle, f)
    }

    /// Runs a long-lived future (a streamer, a pump) named `name`, and returns its
    /// [`Task`].
    ///
    /// On a native target the future gets a thread of its own, at `priority`, which polls
    /// only it and also fires the executor's due sleeps. It ends when the future ends,
    /// when the [`Task`] drops (which cancels the future), or when the last clone of the
    /// executor drops. On the web there are no threads and no priorities: the future is
    /// queued like [`spawn`](Self::spawn), and `name` and `priority` are ignored.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start the thread (native only). The future is not run.
    ///
    /// The OS refusing `priority` is not an error: the thread runs at the priority it has,
    /// one `tracing::warn!` names the thread and the refusal, and this returns `Ok`.
    pub fn spawn_dedicated<T: Send + 'static>(
        &self,
        name: &str,
        priority: Option<PriorityRequest>,
        future: impl Future<Output = T> + Send + 'static,
    ) -> io::Result<Task<T>> {
        os::spawn_dedicated(self, name, priority, future)
    }

    /// A future that resolves once [`Instant::now`] is at or past `deadline`; a deadline
    /// already past resolves on its first poll.
    ///
    /// A driver fires it, so its resolution is the cadence of whoever drives. Dropping the
    /// [`Sleep`] before it resolves removes it from the queue. It resolves at once with
    /// `Err(`[`Shutdown`]`)` once this executor has shut down.
    ///
    /// ```
    /// use tutti_platform::{Executor, Instant};
    ///
    /// let executor = Executor::new();
    /// let task = executor.spawn(executor.sleep_until(Instant::now()));
    /// executor.run_until_stalled();
    /// assert!(task.is_finished());
    /// ```
    pub fn sleep_until(&self, deadline: Instant) -> Sleep {
        self.timer().sleep_until(deadline)
    }

    /// A future that resolves `duration` from now: [`sleep_until`](Self::sleep_until) of
    /// that instant.
    pub fn sleep(&self, duration: Duration) -> Sleep {
        self.timer().sleep(duration)
    }

    /// A [`Timer`] on this executor's deadlines, for a future that needs a deadline and
    /// must not hold the executor.
    ///
    /// The timer holds the deadline queue only: it keeps the executor, its threads and its
    /// queue from nothing, so the executor still shuts down when its last clone drops, and
    /// a [`Sleep`] it made resolves at once with `Err(`[`Shutdown`]`)` from then on.
    pub fn timer(&self) -> Timer {
        Timer::new(&self.handle.shared.timers)
    }

    /// Fires the due sleeps, then runs ready tasks on this thread until none is ready, and
    /// returns how many ran. Never blocks.
    ///
    /// A task that wakes itself runs again within the same call, so a self-waking task
    /// never lets it return: a caller that must bound its work loops
    /// [`try_tick`](Self::try_tick) instead. A task that panicked counts as run.
    pub fn run_until_stalled(&self) -> usize {
        let mut ran = 0;
        while self.try_tick() {
            ran += 1;
        }
        ran
    }

    /// Fires the due sleeps, then runs one ready task if there is one, and returns whether
    /// it did. Never waits; the meaning of smol's `try_tick`.
    ///
    /// A task that panicked counts as run: its panic is its own end, not the caller's.
    pub fn try_tick(&self) -> bool {
        let shared = &self.handle.shared;
        shared.timers.fire(Instant::now());
        catch_unwind(AssertUnwindSafe(|| shared.queue.try_tick())).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests;
