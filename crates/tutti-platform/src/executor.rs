//! [`Executor`]: where tutti's background futures run.
//!
//! One type with three forms, chosen where the app builds it:
//!
//! - [`Executor::threaded`]: a small capped pool of threads for short futures, and
//!   one thread of its own per [`spawn_dedicated`](Executor::spawn_dedicated).
//! - [`Executor::polled`]: nothing runs until the caller's [`poll`](Executor::poll),
//!   which runs the futures that were woken. The form for a target with no threads.
//! - [`Executor::new`]: a host's own executor receives every future.
//!
//! [`Default`] is threaded on a native target and polled on the web.
//!
//! This is not an async runtime: it has no timers and no I/O reactor.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use crate::priority::PriorityRequest;
use crate::thread::{ThreadHandle, spawn_named};

#[cfg_attr(not(target_arch = "wasm32"), path = "executor/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "executor/wasm.rs")]
mod os;

/// A future as an executor holds it.
type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// The most threads the shared pool starts.
const MAX_POOL_THREADS: usize = 4;

/// A `Mutex`'s guard whether or not a panicking holder poisoned it. Each lock here
/// guards a queue or a slot that a panic leaves consistent.
fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs tutti's futures: on threads, on a caller's [`poll`](Self::poll), or on a
/// host's executor.
///
/// `Clone` shares one executor. The forms differ only in who polls:
///
/// - **Threaded** ([`threaded`](Self::threaded)). [`spawn`](Self::spawn) runs the future
///   on a pool of at most `available_parallelism` threads, clamped to 1 to 4, started
///   on demand and parked while idle. [`spawn_dedicated`](Self::spawn_dedicated) gives
///   the future a thread of its own, at the priority asked. A thread polls a future,
///   and parks until the future's [`Waker`] fires when it returns `Pending`.
/// - **Polled** ([`polled`](Self::polled)). Both spawns queue the future. [`poll`](Self::poll)
///   runs the futures that were woken, on the caller's thread.
/// - **Host** ([`new`](Self::new)). Both spawns hand the boxed future to the host's
///   function; the name and the priority are ignored, and the host owns the priority.
///
/// # Shutdown
///
/// When the last clone of a threaded or polled executor drops, its threads are told to
/// stop, woken and joined, and every future still pending is dropped. A future that
/// holds an `Executor` clone keeps the executor alive, so that drop never comes:
/// tutti's own futures never hold a clone, and a spawner passes a future what it needs
/// instead. A long-lived future ends on its own stop flag before the executor drops.
///
/// # Not on the audio thread
///
/// No future is polled on the audio thread, and a [`Waker`]'s `wake` may allocate or
/// lock, so the audio thread never wakes one.
///
/// # Blocking
///
/// A tutti future may block inside `poll` (a disk read, a decode). A host executor
/// must tolerate that: a tokio host wraps each future in `spawn_blocking` and a
/// `block_on`.
///
/// # Panics
///
/// A future that panics while the pool or [`poll`](Self::poll) polls it is dropped; the
/// thread, or the caller's `poll`, carries on. A dedicated thread's panic ends that
/// thread.
#[derive(Clone, Debug)]
pub struct Executor {
    kind: Arc<Kind>,
}

enum Kind {
    Threaded(Threaded),
    Polled(Arc<Ready>),
    Host(Box<dyn Fn(BoxFuture) + Send + Sync>),
}

impl fmt::Debug for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Threaded(_) => "Threaded",
            Kind::Polled(_) => "Polled",
            Kind::Host(_) => "Host",
        })
    }
}

impl Drop for Kind {
    fn drop(&mut self) {
        match self {
            Kind::Threaded(threaded) => threaded.shut_down(),
            Kind::Polled(ready) => ready.shut_down(),
            Kind::Host(_) => {}
        }
    }
}

struct Threaded {
    ready: Arc<Ready>,
    cap: usize,
    pool: Mutex<Vec<ThreadHandle<()>>>,
    dedicated: Mutex<Vec<(ThreadHandle<()>, Thread)>>,
}

impl Threaded {
    fn new() -> Self {
        let cap = thread::available_parallelism()
            .map_or(1, usize::from)
            .clamp(1, MAX_POOL_THREADS);
        Self {
            ready: Arc::new(Ready::default()),
            cap,
            pool: Mutex::new(Vec::new()),
            dedicated: Mutex::new(Vec::new()),
        }
    }

    fn spawn(&self, future: BoxFuture) -> io::Result<()> {
        {
            let mut pool = lock(&self.pool);
            if pool.len() < self.cap {
                let ready = Arc::clone(&self.ready);
                let name = format!("tutti-exec-{}", pool.len());
                match spawn_named(&name, None, move |_| ready.work()) {
                    Ok(thread) => pool.push(thread),
                    // A pool with a thread runs the future on it; only an empty pool
                    // refuses.
                    Err(error) if pool.is_empty() => return Err(error),
                    Err(_) => {}
                }
            }
        }
        self.ready.spawn(future);
        Ok(())
    }

    fn spawn_dedicated(
        &self,
        name: &str,
        priority: Option<PriorityRequest>,
        future: BoxFuture,
    ) -> io::Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        let handle = spawn_named(name, priority, move |stop| {
            let park = Arc::new(Park {
                thread: thread::current(),
                woken: AtomicBool::new(true),
            });
            let _ = tx.send(park.thread.clone());
            let waker = Waker::from(Arc::clone(&park));
            let mut cx = Context::from_waker(&waker);
            let mut future = future;
            while !stop.is_stopped() {
                if !park.woken.swap(false, Ordering::AcqRel) {
                    thread::park();
                } else if future.as_mut().poll(&mut cx).is_ready() {
                    return;
                }
            }
        })?;
        let thread = rx
            .recv()
            .map_err(|_| io::Error::other("the dedicated thread ended before it started"))?;
        let mut dedicated = lock(&self.dedicated);
        dedicated.retain(|(handle, _)| !handle.is_finished());
        dedicated.push((handle, thread));
        Ok(())
    }

    /// Stops and joins every thread, then drops the futures left pending.
    fn shut_down(&self) {
        self.ready.close();
        let dedicated = mem::take(&mut *lock(&self.dedicated));
        for (handle, thread) in &dedicated {
            handle.token().stop();
            thread.unpark();
        }
        drop(dedicated);
        drop(mem::take(&mut *lock(&self.pool)));
        self.ready.drop_futures();
    }
}

/// The wake of a thread that blocks on one future.
struct Park {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for Park {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Arc<Task>>,
    closed: bool,
}

/// The queue of woken tasks, shared by a pool's threads or a caller's `poll`, and
/// every task still alive.
#[derive(Default)]
struct Ready {
    queue: Mutex<Queue>,
    wake: Condvar,
    tasks: Mutex<Vec<Weak<Task>>>,
}

impl Ready {
    fn spawn(self: &Arc<Self>, future: BoxFuture) {
        let task = Arc::new(Task {
            future: Mutex::new(Some(future)),
            queued: AtomicBool::new(true),
            ready: Arc::downgrade(self),
        });
        {
            let mut tasks = lock(&self.tasks);
            tasks.retain(|task| task.strong_count() > 0);
            tasks.push(Arc::downgrade(&task));
        }
        self.push(task);
    }

    fn push(&self, task: Arc<Task>) {
        let mut queue = lock(&self.queue);
        if !queue.closed {
            queue.items.push_back(task);
            self.wake.notify_one();
        }
    }

    /// A pool thread's body: polls woken tasks until the queue closes.
    fn work(&self) {
        loop {
            let task = {
                let mut queue = lock(&self.queue);
                loop {
                    if queue.closed {
                        return;
                    }
                    if let Some(task) = queue.items.pop_front() {
                        break task;
                    }
                    queue = self
                        .wake
                        .wait(queue)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            task.run();
        }
    }

    /// Polls up to `max` of the tasks woken before the call.
    fn poll(&self, max: usize) -> usize {
        let mut batch = mem::take(&mut lock(&self.queue).items);
        let polled = max.min(batch.len());
        for task in batch.drain(..polled) {
            task.run();
        }
        if !batch.is_empty() {
            let mut queue = lock(&self.queue);
            for task in batch.into_iter().rev() {
                queue.items.push_front(task);
            }
        }
        polled
    }

    /// Refuses further wakes and releases the threads waiting for one.
    fn close(&self) {
        lock(&self.queue).closed = true;
        self.wake.notify_all();
    }

    /// Drops every future still alive. No thread polls once the queue is closed
    /// and joined.
    fn drop_futures(&self) {
        let tasks = mem::take(&mut *lock(&self.tasks));
        for task in tasks.iter().filter_map(Weak::upgrade) {
            *lock(&task.future) = None;
        }
    }

    fn shut_down(&self) {
        self.close();
        let items = mem::take(&mut lock(&self.queue).items);
        drop(items);
        self.drop_futures();
    }
}

/// A spawned future and whether it sits on the ready queue.
struct Task {
    future: Mutex<Option<BoxFuture>>,
    queued: AtomicBool,
    ready: Weak<Ready>,
}

impl Task {
    /// Polls once. A woken task is queued again, a finished or panicked one drops its
    /// future.
    fn run(self: &Arc<Self>) {
        // `queued` is only ever swapped, here and in `wake_by_ref`: two read-modify-writes
        // on one atomic. Either the waker's swap is first, and this acquire sees what
        // the waker published before it, or this swap is first, and the waker reads
        // `false` and queues the task again. A store here would let the poll read stale
        // state while the waker still saw `true`: a lost wake.
        self.queued.swap(false, Ordering::AcqRel);
        let mut slot = lock(&self.future);
        let Some(future) = slot.as_mut() else {
            return;
        };
        let waker = Waker::from(Arc::clone(self));
        let mut cx = Context::from_waker(&waker);
        match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut cx))) {
            Ok(Poll::Pending) => {}
            Ok(Poll::Ready(())) | Err(_) => *slot = None,
        }
    }
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(ready) = self.ready.upgrade()
            && !self.queued.swap(true, Ordering::AcqRel)
        {
            ready.push(Arc::clone(self));
        }
    }
}

impl Executor {
    /// The threaded form: a capped pool for [`spawn`](Self::spawn) and a thread per
    /// [`spawn_dedicated`](Self::spawn_dedicated). No thread starts until a future needs one.
    #[must_use]
    pub fn threaded() -> Self {
        Self {
            kind: Arc::new(Kind::Threaded(Threaded::new())),
        }
    }

    /// The polled form: futures run only inside [`poll`](Self::poll).
    ///
    /// Work runs only when you call `poll` (a session calls it from `update`). A live
    /// stream needs it called at least as often as its ring drains. An app that can't
    /// promise that uses [`threaded`](Self::threaded) or a host's pool.
    #[must_use]
    pub fn polled() -> Self {
        Self {
            kind: Arc::new(Kind::Polled(Arc::new(Ready::default()))),
        }
    }

    /// A host's executor: `spawn` receives every future. It owns running each one,
    /// and must tolerate a `poll` that blocks.
    #[must_use]
    pub fn new(
        spawn: impl Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: Arc::new(Kind::Host(Box::new(spawn))),
        }
    }

    /// Runs a short future in the background.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start the pool's first thread (threaded form only). The
    /// future is not run. Once the pool has a thread, a refused further thread only
    /// leaves the pool smaller.
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> io::Result<()> {
        match &*self.kind {
            Kind::Threaded(threaded) => threaded.spawn(Box::pin(future)),
            Kind::Polled(ready) => {
                ready.spawn(Box::pin(future));
                Ok(())
            }
            Kind::Host(spawn) => {
                spawn(Box::pin(future));
                Ok(())
            }
        }
    }

    /// Runs a long-lived future (a streamer, a pump) named `name`. The threaded form gives
    /// it a thread of its own at `priority`; the polled and host forms treat it as
    /// [`spawn`](Self::spawn), and the priority is then the caller's or the host's.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start the thread (threaded form only). The future is not run.
    pub fn spawn_dedicated(
        &self,
        name: &str,
        priority: Option<PriorityRequest>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        match &*self.kind {
            Kind::Threaded(threaded) => threaded.spawn_dedicated(name, priority, Box::pin(future)),
            Kind::Polled(_) | Kind::Host(_) => self.spawn(future),
        }
    }

    /// Polls up to `max` futures that were woken before the call, on this thread, and
    /// returns how many it polled. A future that wakes itself while polled runs again
    /// on the next call. Only the polled form does anything; the others return 0.
    pub fn poll(&self, max: usize) -> usize {
        match &*self.kind {
            Kind::Polled(ready) => ready.poll(max),
            Kind::Threaded(_) | Kind::Host(_) => 0,
        }
    }
}

impl Default for Executor {
    /// Threaded on a native target, polled on the web.
    fn default() -> Self {
        os::default_executor()
    }
}

#[cfg(test)]
mod tests;
