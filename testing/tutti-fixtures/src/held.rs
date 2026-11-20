//! A host executor a test steps by hand: it holds every future it is handed, and the
//! test runs them when it chooses.
//!
//! These are for jobs that are one closure's poll. A future that awaits a bell (the
//! streamer, an open) is not one: a test that has such a future uses
//! `Executor::polled()` and drives it with the session's `update`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Waker};

use tutti_platform::Executor;

type Held = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Polls `future` once, for a job that is one closure's poll. A session's job is a closure in an `async` block, so it
/// completes in that poll; a future still pending after it (a streamer awaiting
/// its bell) is dropped, since nothing here wakes it.
pub fn run_future(mut future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
    let mut cx = Context::from_waker(Waker::noop());
    let _ = future.as_mut().poll(&mut cx);
}

/// A future a [`HeldFutures`] took out of its queue: run it, or drop it unrun. For a job
/// that is one closure's poll.
#[must_use = "a future neither run nor kept is dropped unrun"]
pub struct HeldFuture(Held);

impl HeldFuture {
    /// Polls the future once, as [`run_future`] does.
    pub fn run(self) {
        run_future(self.0);
    }
}

impl fmt::Debug for HeldFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldFuture").finish_non_exhaustive()
    }
}

/// An [`Executor`] that holds the futures it is given, until [`run_all`](Self::run_all).
/// For jobs that are one closure's poll: it never wakes a pending future.
///
/// Clones share one queue.
#[derive(Clone, Default)]
pub struct HeldFutures {
    held: Arc<Mutex<Vec<Held>>>,
}

impl fmt::Debug for HeldFutures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldFutures")
            .field("waiting", &self.len())
            .finish()
    }
}

impl HeldFutures {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An executor that appends every future to this queue.
    #[must_use]
    pub fn executor(&self) -> Executor {
        let held = Arc::clone(&self.held);
        Executor::new(move |future| {
            held.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(future);
        })
    }

    /// How many futures wait.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether none waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Takes every waiting future out of the queue, oldest first.
    pub fn take(&self) -> Vec<HeldFuture> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .map(HeldFuture)
            .collect()
    }

    /// Takes every waiting future, oldest first, and polls each once, as
    /// [`run_future`] does. Returns how many were taken. A future that spawns another
    /// leaves it for the next call.
    pub fn run_all(&self) -> usize {
        let futures = self.take();
        let count = futures.len();
        for future in futures {
            future.run();
        }
        count
    }

    /// Drops every waiting future unrun, as a host that discards what it was handed.
    pub fn drop_all(&self) {
        drop(self.take());
    }
}

/// An executor for jobs that are one closure's poll: it runs each future inside the spawn call, on the caller's
/// thread: a job runs before the call that started it returns.
#[must_use]
pub fn inline_executor() -> Executor {
    Executor::new(run_future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A future the queue holds does not run until `run_all`, and runs once then.
    #[test]
    fn a_held_future_runs_only_in_run_all() {
        let held = HeldFutures::new();
        let executor = held.executor();
        let ran = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&ran);
        executor
            .spawn(async move {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(held.len(), 1);
        assert_eq!(held.run_all(), 1);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(held.is_empty());
    }

    /// The inline executor has run the future when the spawn returns.
    #[test]
    fn the_inline_executor_runs_before_the_spawn_returns() {
        let ran = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&ran);
        inline_executor()
            .spawn(async move {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
