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
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// Parks the calling thread until `task` ends: the task's waker unparks it.
pub(super) fn join<T>(task: Task<T>) -> Option<T> {
    assert!(
        !DRIVING.with(Cell::get),
        "join on a thread that drives an executor: the wait would stop the tasks it runs"
    );
    let mut ended = pin!(task.fallible());
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        // A task whose future panicked hands the panic back to its poll; the panic hook
        // already reported it on the task's thread, so the wait ends as `None`.
        match catch_unwind(AssertUnwindSafe(|| ended.as_mut().poll(&mut cx))) {
            Ok(Poll::Ready(output)) => return output,
            Ok(Poll::Pending) => thread::park(),
            Err(_) => return None,
        }
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
fn drive(queue: &async_executor::Executor<'static>, timers: &Timers, stopped: &dyn Fn() -> bool) {
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
    /// drops. Native targets only: a web page starts no threads, and its caller drives with
    /// [`run_until_stalled`](Self::run_until_stalled).
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
    let handle = &executor.handle;
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
    let id = Arc::new(OnceLock::new());
    let (shared, id_slot) = (Arc::clone(&handle.shared), Arc::clone(&id));
    let thread = spawn_named(name, priority, move |stop| {
        let _ = id_slot.set(thread::current().id());
        let _ = finished.thread.set(thread::current());
        drive(&queue, &shared.timers, &|| {
            stop.is_stopped() || finished.done.load(Ordering::Acquire)
        });
    })?;
    if let Some(outcome @ PriorityOutcome::Refused { .. }) = thread.priority() {
        tracing::warn!(thread = name, %outcome, "dedicated thread runs without the priority it asked for");
    }
    let mut dedicated = lock(&handle.own.dedicated);
    dedicated.retain(|worker| !worker.handle.is_finished());
    dedicated.push(Worker { handle: thread, id });
    Ok(task)
}
