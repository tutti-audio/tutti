//! [`Executor`]: where tutti's background work runs.
//!
//! One type: a run queue of woken tasks, a queue of sleep deadlines, and a drive that
//! fires the due sleeps and runs the ready tasks. Whoever calls
//! [`run_until_stalled`](Executor::run_until_stalled) or
//! [`tick`](Executor::tick) drives it; on a native target
//! [`spawn_threads`](Executor::spawn_threads) starts threads that drive it for good, and on
//! the web `Executor::drive_on_page` hands the browser's queue a future that does.
//! This is not an async runtime: it has no I/O reactor.

use std::fmt;
use std::future::Future;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
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

/// Waits on the calling thread until `task` ends, and returns its output: `None` when the
/// executor dropped the task instead (its future panicked, or the executor shut down
/// first).
///
/// It only waits: the task runs where its executor runs it (a dedicated thread, a
/// [`Threads`] driver), never here, so this is no way to run a future inline. A drop that
/// must not return before a future it stopped has ended (an OS handle the future still
/// uses) waits with it.
///
/// Never call it on the thread that runs `task` (its own dedicated thread, or the only
/// thread that drives its executor): nothing else would run it, and the wait never ends.
///
/// The web cannot block: there it looks once and returns the output of a task that has
/// ended, or `None`, and a task that has not ended is dropped, which cancels it.
///
/// # Panics
///
/// On a thread that drives an executor (a dedicated thread, a [`Threads`] driver): a wait
/// there would stop the thread running the tasks it drives, its own task among them.
/// Code that may run on a task (a `Drop`, say) calls [`join_within`], which hands the task
/// back there.
pub fn join<T>(task: Task<T>) -> Option<T> {
    os::join(task)
}

/// [`join`] with a bound: waits at most `bound` for `task` to end, and hands the task back
/// when it has not or when this thread cannot wait.
///
/// The caller keeps the task while it waits, so nothing is cancelled by the wait. An owner
/// that stops a future (a stop flag it reads) and must not wait for it without end calls
/// this, then waits again, [`detach`](Task::detach)es or drops (cancels) what comes back.
/// It is the form for code that may run on a task, a `Drop` among it: on a thread that
/// drives an executor a wait would stop the tasks that thread runs, so it returns at once
/// with [`Unjoined::Driving`], neither polling nor waiting.
///
/// The web cannot block: there it looks once, whatever the bound.
///
/// # Errors
///
/// The task itself, as [`Unjoined::TimedOut`] when it has not ended within `bound`, and as
/// [`Unjoined::Driving`] when the calling thread drives an executor.
pub fn join_within<T>(task: Task<T>, bound: Duration) -> Result<Option<T>, Unjoined<T>> {
    os::join_within(task, bound)
}

/// A [`Task`] that [`join_within`] gives back instead of its output. The task keeps
/// running: the caller [`detach`](Task::detach)es it, waits again or drops (cancels) it.
#[derive(thiserror::Error)]
pub enum Unjoined<T> {
    /// The task did not end within the bound.
    #[error("the task did not end within the bound")]
    TimedOut(Task<T>),
    /// The calling thread drives an executor, where a wait would stop the tasks it runs.
    #[error("this thread drives an executor and cannot wait")]
    Driving(Task<T>),
}

impl<T> Unjoined<T> {
    /// The task, whichever way it was not joined.
    pub fn into_task(self) -> Task<T> {
        match self {
            Self::TimedOut(task) | Self::Driving(task) => task,
        }
    }
}

impl<T> std::fmt::Debug for Unjoined<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(task) => f.debug_tuple("TimedOut").field(task).finish(),
            Self::Driving(task) => f.debug_tuple("Driving").field(task).finish(),
        }
    }
}

/// Threads that drive an [`Executor`]'s queue, started by `Executor::spawn_threads` on a
/// native target. The web has no constructor for it.
///
/// Each fires the due sleeps, runs a ready task, and parks until a task is scheduled or the
/// next deadline falls due. Dropping the `Threads` stops them and joins them within one
/// second in all, detaching one still running then; a drop on one of
/// the threads itself (the last holder of a waker, say) does not join that thread. The
/// executor it drives stays alive while it does.
#[must_use = "dropping `Threads` stops and joins its threads"]
#[derive(Debug)]
pub struct Threads {
    /// Held for its drop, which stops and joins the threads.
    _threads: os::Threads,
}

/// The handle of the future that drives an [`Executor`] from the browser's queue, started by
/// `Executor::drive_on_page` on the web. A native target has no constructor for it.
///
/// Each turn fires the due sleeps, runs a bounded batch of ready tasks, and yields to the
/// page or waits (as a pending future, never a spin) for a task, the next deadline, or the
/// stop. Dropping the `PageDriver`, or the executor's last clone, stops it and clears its
/// pending browser timeout.
#[must_use = "dropping `PageDriver` stops the driver"]
pub struct PageDriver {
    /// Held for its drop, which stops the driver.
    _page: os::Page,
}

impl fmt::Debug for PageDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageDriver").finish_non_exhaustive()
    }
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
///   [`tick`](Self::tick) runs one. Neither waits.
/// - **Threads** (native only). [`spawn_threads`](Self::spawn_threads) starts threads
///   that each fire the due sleeps, run a ready task, and park until a task is scheduled
///   or the next deadline. Dropping the returned `Threads` stops and joins them.
/// - **The browser's queue** (web only). `drive_on_page` hands it one future that fires
///   the due sleeps and runs the ready tasks in bounded batches, awaiting a browser
///   timeout for the next deadline. Dropping the returned [`PageDriver`] stops it.
///
/// Blocking work does not run on a driver: [`unblock`](Self::unblock) runs a closure on a
/// small pool of its own (native), or as a task of the queue on the page's thread (web), and
/// [`spawn_dedicated`](Self::spawn_dedicated) gives one long-lived future a thread of its
/// own (native), or queues it like [`spawn`](Self::spawn) (web).
///
/// # Shutdown
///
/// When the last clone drops, the dedicated threads are told to stop, woken and joined
/// within one second in all. One still running then (a future wedged in an OS call its wake
/// cannot reach) is detached and named in a `tracing::warn!`: it ends when its future does,
/// and until then holds its queue, the deadline queue and whatever its future captured (a
/// file, a channel). The blocking pool's idle threads end, and every task still pending is
/// dropped. A future
/// that holds an `Executor` clone keeps the executor alive, so that drop never comes:
/// tutti's own futures never hold a clone, and a spawner passes a future what it needs
/// instead. A long-lived future ends on its own stop flag before the executor drops. A
/// future that needs deadlines holds a [`Timer`], which keeps nothing alive, and a future
/// that spawns later holds a [`WeakExecutor`] and upgrades only for the spawn.
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

/// A handle to an [`Executor`] that keeps nothing alive, made by
/// [`Executor::downgrade`]: the `Weak` of the executor's `Arc`.
///
/// Work that is queued or handed out before it runs (a load the app runs later, a task
/// that starts a dedicated watch) holds one instead of a clone, so the executor still
/// shuts down when its last clone drops. [`upgrade`](Self::upgrade) gives a clone back
/// while the executor lives, and `None` once it has shut down.
#[derive(Clone)]
pub struct WeakExecutor {
    handle: Weak<Handle>,
}

impl WeakExecutor {
    /// A clone of the executor while one of its clones is alive, `None` once it has shut
    /// down. Hold the clone only as long as the work that needs it: a clone kept in a
    /// task on the executor keeps the executor alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<Executor> {
        self.handle.upgrade().map(|handle| Executor { handle })
    }

    /// Whether an executor clone still lives: `false` from the moment the last clone
    /// drops, which is when shutdown begins and the dedicated threads are joined. A
    /// dedicated future that never yields reads it each turn and returns. One atomic
    /// load: no lock, allocation or clone.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.handle.strong_count() > 0
    }
}

impl fmt::Debug for WeakExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakExecutor").finish_non_exhaustive()
    }
}

impl Executor {
    /// An executor with nothing running: no thread starts, and nothing runs until
    /// [`run_until_stalled`](Self::run_until_stalled) or
    /// [`tick`](Self::tick) is called, or threads are spawned.
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
    /// anything drives the queue. On the web there are no threads: `f` is a task of the
    /// queue, and runs on the page's thread inside a turn of the driver (or of the next
    /// [`run_until_stalled`](Self::run_until_stalled) or [`tick`](Self::tick)) for as
    /// long as it takes, holding the page's other tasks off until it returns.
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

    /// [`spawn_dedicated`](Self::spawn_dedicated) for a future that is not `Send`: the new
    /// thread calls `make` and runs the future it returns there, so the future never
    /// leaves that thread (an OS object bound to the thread that made it, say). Its output
    /// is `Send`, and so is the [`Task`].
    ///
    /// The thread is the executor's like any dedicated one: it fires the due sleeps, ends
    /// when the future ends, when the [`Task`] drops, or at shutdown, and drops the future
    /// on itself. This returns once the thread has spawned the future, before its first
    /// poll.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start the thread, as [`spawn_dedicated`](Self::spawn_dedicated).
    /// On the web, [`io::ErrorKind::Unsupported`]: a page starts no threads, and a future
    /// that is not `Send` cannot join the shared queue.
    pub fn spawn_dedicated_local<T, F>(
        &self,
        name: &str,
        priority: Option<PriorityRequest>,
        make: impl FnOnce() -> F + Send + 'static,
    ) -> io::Result<Task<T>>
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
    {
        os::spawn_dedicated_local(self, name, priority, make)
    }

    /// A future that resolves once [`Instant::now`] is at or past `deadline`; a deadline
    /// already past resolves on its first poll.
    ///
    /// A driver fires it, so its resolution is the cadence of whoever drives (on the web, the
    /// browser's timeout). Dropping the
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

    /// A [`Timer`] on this executor's deadlines, for a future that needs a deadline and
    /// must not hold the executor.
    ///
    /// The timer holds the deadline queue only: it keeps the executor, its threads and its
    /// queue from nothing, so the executor still shuts down when its last clone drops, and
    /// a [`Sleep`] it made resolves at once with `Err(`[`Shutdown`]`)` from then on.
    pub fn timer(&self) -> Timer {
        Timer::new(&self.handle.shared.timers)
    }

    /// A [`WeakExecutor`] on this executor, for work that must reach the executor later
    /// without keeping it alive: it upgrades to a clone while a clone lives, and gives
    /// `None` once the executor has shut down.
    #[must_use]
    pub fn downgrade(&self) -> WeakExecutor {
        WeakExecutor {
            handle: Arc::downgrade(&self.handle),
        }
    }

    /// Fires the due sleeps, then runs ready tasks on this thread until none is ready, and
    /// returns how many ran. Never blocks.
    ///
    /// A task that wakes itself runs again within the same call, so a self-waking task
    /// never lets it return: a caller that must bound its work loops
    /// [`tick`](Self::tick) instead. A task that panicked counts as run.
    pub fn run_until_stalled(&self) -> usize {
        let mut ran = 0;
        while self.tick() {
            ran += 1;
        }
        ran
    }

    /// Fires the due sleeps, then runs one ready task if there is one, and returns whether
    /// it did. Never waits; the meaning of smol's `try_tick`.
    ///
    /// A task that panicked counts as run: its panic is its own end, not the caller's.
    pub fn tick(&self) -> bool {
        let shared = &self.handle.shared;
        shared.timers.fire(Instant::now());
        catch_unwind(AssertUnwindSafe(|| shared.queue.try_tick())).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests;
