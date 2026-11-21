//! A native target's executor edge: threads that drive the queue and park until a task
//! or a deadline, a thread of its own per dedicated future, and a pool for blocking
//! closures.
//!
//! A driving thread's loop is [`drive`]: it fires the due sleeps and polls the queue's
//! `tick` future with a waker that unparks the thread. `async-executor` wakes the `tick`
//! future's waker when a task is scheduled, and a sleep armed earlier than the queue's
//! earliest deadline unparks every registered thread, so a parked thread wakes for both
//! with no condvar of ours: `park_timeout` to the next deadline is the only wait.

use std::cell::Cell;
use std::future::Future;
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread, ThreadId};
use std::time::Duration;

use super::{Executor, Handle, Shared, Task, Timers, lock};
use crate::Instant;
use crate::priority::{PriorityOutcome, PriorityRequest};
use crate::thread::{ThreadHandle, spawn_named};

#[path = "native/blocking.rs"]
mod blocking;

use blocking::Blocking;

/// The threads parked for a task or a deadline.
#[derive(Default)]
pub(super) struct Parked(Mutex<Vec<Thread>>);

impl Parked {
    fn register(&self, thread: Thread) {
        lock(&self.0).push(thread);
    }

    fn forget(&self, id: ThreadId) {
        lock(&self.0).retain(|thread| thread.id() != id);
    }

    /// Unparks every registered thread.
    pub(super) fn unpark_all(&self) {
        for thread in lock(&self.0).iter() {
            thread.unpark();
        }
    }
}

/// A started thread and, once it runs, its id: the one thread a stop does not join.
struct Worker {
    handle: ThreadHandle<()>,
    id: Arc<OnceLock<ThreadId>>,
}

/// What the executor starts on its own account: the blocking pool and the dedicated
/// threads still running.
pub(super) struct Own {
    blocking: Arc<Blocking>,
    dedicated: Mutex<Vec<Worker>>,
}

impl Own {
    pub(super) fn new() -> Self {
        Self {
            blocking: Blocking::new(blocking::IDLE_LIFE),
            dedicated: Mutex::new(Vec::new()),
        }
    }

    /// Stops and joins every dedicated thread, and closes the blocking pool.
    pub(super) fn shut_down(&self, shared: &Shared) {
        self.blocking.close();
        let threads = std::mem::take(&mut *lock(&self.dedicated));
        stop_and_join(threads, &shared.timers);
    }
}

pub(super) fn unblock<T: Send + 'static>(
    handle: &Handle,
    f: impl FnOnce() -> T + Send + 'static,
) -> io::Result<Task<T>> {
    handle.own.blocking.unblock(f)
}

thread_local! {
    /// Set while this thread runs [`drive`]: a `join` here would stop the drive.
    static DRIVING: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as driving for as long as it lives.
struct Driving;

impl Driving {
    fn enter() -> Self {
        DRIVING.with(|d| d.set(true));
        Self
    }
}

impl Drop for Driving {
    fn drop(&mut self) {
        DRIVING.with(|d| d.set(false));
    }
}

/// Whether this thread runs [`drive`].
pub(super) fn is_driving() -> bool {
    DRIVING.with(Cell::get)
}

/// Blocks the calling thread on `task` until it ends.
pub(super) fn join<T>(task: Task<T>) -> Option<T> {
    refuse_on_a_driving_thread();
    // A task whose future panicked re-raises the panic at the poll, and the panic hook
    // prints it: the task ended with no output. A cancelled one is `None` from the
    // `fallible()` handle.
    catch_unwind(AssertUnwindSafe(|| {
        futures_lite::future::block_on(task.fallible())
    }))
    .unwrap_or(None)
}

/// Parks the calling thread until `task` ends or `bound` has passed. Polls `task`'s
/// handle, parking between polls; the task's waker unparks it.
pub(super) fn join_within<T>(mut task: Task<T>, bound: Duration) -> Result<Option<T>, Task<T>> {
    refuse_on_a_driving_thread();
    let deadline = Instant::now() + bound;
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if task.is_finished() {
            return Ok(ended(task, &mut cx));
        }
        // A poll registers the waker. One that finds the task dropped without ending, in
        // the moment since the look above, panics with `async_task`'s message, which the
        // panic hook prints: `None`. A `fallible()` handle would not panic, but cannot be
        // handed back past the bound.
        match catch_unwind(AssertUnwindSafe(|| Pin::new(&mut task).poll(&mut cx))) {
            Ok(Poll::Ready(output)) => return Ok(Some(output)),
            Err(_) => return Ok(None),
            Ok(Poll::Pending) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(task);
                }
                thread::park_timeout(left);
            }
        }
    }
}

/// Panics on a thread that drives an executor: a wait there would stop the tasks it runs.
fn refuse_on_a_driving_thread() {
    assert!(
        !is_driving(),
        "join on a thread that drives an executor: the wait would stop the tasks it runs"
    );
}

/// The outcome of a task that has finished: its output, or `None` when its future
/// panicked (the hook already reported it on the task's thread, and the poll hands the
/// panic back) or was dropped without ending.
fn ended<T>(task: Task<T>, cx: &mut Context<'_>) -> Option<T> {
    let mut ended = pin!(task.fallible());
    match catch_unwind(AssertUnwindSafe(|| ended.as_mut().poll(cx))) {
        Ok(Poll::Ready(output)) => output,
        Ok(Poll::Pending) | Err(_) => None,
    }
}

/// Unparks the thread that polls.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs the current thread as a driver of `queue` until `stopped`: fires the sleeps due in
/// `timers`, runs the tasks `queue` has ready one at a time, and parks until a task is
/// scheduled or the next deadline. `stopped` is read after every task and every wake, and
/// whoever sets it unparks every registered thread.
fn drive(queue: &impl Queue, timers: &Timers, stopped: &dyn Fn() -> bool) {
    let _driving = Driving::enter();
    let thread = thread::current();
    timers.parked.register(thread.clone());
    let waker = Waker::from(Arc::new(Unpark(thread.clone())));
    let mut cx = Context::from_waker(&waker);
    'tasks: loop {
        let mut tick = pin!(queue.tick());
        loop {
            if stopped() {
                break 'tasks;
            }
            timers.fire(Instant::now());
            // A task that panics ends its own run: the thread takes the next.
            match catch_unwind(AssertUnwindSafe(|| tick.as_mut().poll(&mut cx))) {
                Ok(Poll::Pending) => {}
                Ok(Poll::Ready(())) | Err(_) => continue 'tasks,
            }
            // A stop requested by a task or a waker that ran above ends the loop here,
            // before it parks: only an unpark from another thread would end the park.
            if stopped() {
                break 'tasks;
            }
            let next = lock(&timers.state).due.next_deadline();
            match next {
                None => thread::park(),
                Some(deadline) => {
                    thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
                }
            }
        }
    }
    timers.parked.forget(thread.id());
}

/// A run queue [`drive`] runs: the shared one, or a dedicated thread's local one.
trait Queue {
    /// Runs one ready task, waiting for one when none is.
    fn tick(&self) -> impl Future<Output = ()> + '_;
}

impl Queue for async_executor::Executor<'static> {
    fn tick(&self) -> impl Future<Output = ()> + '_ {
        async_executor::Executor::tick(self)
    }
}

impl Queue for async_executor::LocalExecutor<'static> {
    fn tick(&self) -> impl Future<Output = ()> + '_ {
        async_executor::LocalExecutor::tick(self)
    }
}

/// How long a stop waits, in all, for the threads it stops to end. A thread still
/// running then (a future wedged in an OS call its wake cannot reach) is detached.
pub(super) const STOP_BOUND: Duration = Duration::from_secs(1);

/// One deadline for every join of a stop: each join waits only what is left of it, so
/// stopping `n` threads waits [`STOP_BOUND`] in all, not `n` times it.
#[derive(Clone, Copy, Debug)]
pub(super) struct StopDeadline(Instant);

impl StopDeadline {
    /// The deadline [`STOP_BOUND`] after `now`.
    pub(super) fn after(now: Instant) -> Self {
        Self(now + STOP_BOUND)
    }

    /// What is left of the deadline at `now`: zero once it has passed.
    pub(super) fn left(self, now: Instant) -> Duration {
        self.0.saturating_duration_since(now)
    }
}

/// Requests every thread's stop, wakes them and joins them within [`STOP_BOUND`] in all,
/// except the calling thread, which a drop on one of them leaves to end by itself. A
/// thread still running at the bound is detached, named in a warning: it holds only the
/// queue and timers it drives, and ends when its future does.
fn stop_and_join(threads: Vec<Worker>, timers: &Timers) {
    for worker in &threads {
        worker.handle.token().stop();
    }
    timers.parked.unpark_all();
    let current = thread::current().id();
    let deadline = StopDeadline::after(Instant::now());
    for worker in threads {
        if worker.id.get() == Some(&current) {
            worker.handle.detach();
            continue;
        }
        if let Err(running) = worker.handle.stop_within(deadline.left(Instant::now())) {
            tracing::warn!(
                thread = running.name(),
                "a stopped thread still runs past the stop bound; it is detached"
            );
            running.detach();
        }
    }
}

/// No value: a native target has threads, so nothing builds a [`super::PageDriver`].
pub(super) enum Page {}

/// The native half of [`super::Threads`]: the started threads and the executor they drive.
pub(super) struct Threads {
    executor: Executor,
    workers: Vec<Worker>,
}

impl std::fmt::Debug for Threads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Threads")
            .field("len", &self.workers.len())
            .finish_non_exhaustive()
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        let workers = std::mem::take(&mut self.workers);
        stop_and_join(workers, &self.executor.handle.shared.timers);
    }
}

impl Executor {
    /// Starts `n` threads that drive this executor's queue, until the returned [`Threads`](super::Threads)
    /// drops. Native targets only: a web page starts no threads, and the browser's queue drives with
    /// [`drive_on_page`](Self::drive_on_page).
    ///
    /// # Errors
    ///
    /// The OS's refusal to start a thread; none stays running.
    pub fn spawn_threads(&self, n: NonZeroUsize) -> io::Result<super::Threads> {
        let shared = &self.handle.shared;
        let mut started = Vec::with_capacity(n.get());
        for index in 0..n.get() {
            let id = Arc::new(OnceLock::new());
            let (queue, id_slot) = (Arc::clone(shared), Arc::clone(&id));
            let spawned = spawn_named(&format!("tutti-exec-{index}"), None, move |stop| {
                let _ = id_slot.set(thread::current().id());
                drive(&queue.queue, &queue.timers, &|| stop.is_stopped());
            });
            match spawned {
                Ok(handle) => started.push(Worker { handle, id }),
                Err(error) => {
                    stop_and_join(started, &shared.timers);
                    return Err(error);
                }
            }
        }
        Ok(super::Threads {
            _threads: Threads {
                executor: self.clone(),
                workers: started,
            },
        })
    }
}

/// Ends a dedicated thread when its future ends or is cancelled.
struct Finished {
    done: AtomicBool,
    thread: OnceLock<Thread>,
}

struct EndGuard(Arc<Finished>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        if let Some(thread) = self.0.thread.get() {
            thread.unpark();
        }
    }
}

pub(super) fn spawn_dedicated<T: Send + 'static>(
    executor: &Executor,
    name: &str,
    priority: Option<PriorityRequest>,
    future: impl Future<Output = T> + Send + 'static,
) -> io::Result<Task<T>> {
    let finished = Arc::new(Finished {
        done: AtomicBool::new(false),
        thread: OnceLock::new(),
    });
    let queue = Arc::new(async_executor::Executor::new());
    let guard = EndGuard(Arc::clone(&finished));
    let task = queue.spawn(async move {
        let _guard = guard;
        future.await
    });
    start_dedicated(
        executor,
        name,
        priority,
        finished,
        move |shared, stopped| {
            drive(&*queue, &shared.timers, stopped);
        },
    )?;
    Ok(task)
}

/// Starts a thread named `name` that builds `make`'s future on itself and drives only
/// it, and returns its [`Task`] once the thread has spawned it.
pub(super) fn spawn_dedicated_local<T, F>(
    executor: &Executor,
    name: &str,
    priority: Option<PriorityRequest>,
    make: impl FnOnce() -> F + Send + 'static,
) -> io::Result<Task<T>>
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let finished = Arc::new(Finished {
        done: AtomicBool::new(false),
        thread: OnceLock::new(),
    });
    let guard = EndGuard(Arc::clone(&finished));
    let (spawned, task) = mpsc::sync_channel(1);
    start_dedicated(
        executor,
        name,
        priority,
        finished,
        move |shared, stopped| {
            let queue = async_executor::LocalExecutor::new();
            let _ = spawned.send(queue.spawn(async move {
                let _guard = guard;
                make().await
            }));
            drive(&queue, &shared.timers, stopped);
        },
    )?;
    // The thread sends the task before it drives: a thread that ends before sending has
    // panicked, which the dedicated set already records.
    task.recv()
        .map_err(|_| io::Error::other("the dedicated thread ended before its task"))
}

/// Spawns the dedicated thread for `finished`'s future, running `body` with the executor
/// it shares and its stop condition, and records it in the executor's dedicated set.
fn start_dedicated(
    executor: &Executor,
    name: &str,
    priority: Option<PriorityRequest>,
    finished: Arc<Finished>,
    body: impl FnOnce(&Shared, &dyn Fn() -> bool) + Send + 'static,
) -> io::Result<()> {
    let handle = &executor.handle;
    let id = Arc::new(OnceLock::new());
    let (shared, id_slot) = (Arc::clone(&handle.shared), Arc::clone(&id));
    let thread = spawn_named(name, priority, move |stop| {
        let _ = id_slot.set(thread::current().id());
        let _ = finished.thread.set(thread::current());
        body(&shared, &|| {
            stop.is_stopped() || finished.done.load(Ordering::Acquire)
        });
    })?;
    if let Some(outcome @ PriorityOutcome::Refused { .. }) = thread.priority() {
        tracing::warn!(thread = name, %outcome, "dedicated thread runs without the priority it asked for");
    }
    let mut dedicated = lock(&handle.own.dedicated);
    dedicated.retain(|worker| !worker.handle.is_finished());
    dedicated.push(Worker { handle: thread, id });
    Ok(())
}
