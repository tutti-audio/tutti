//! The blocking pool behind [`Executor::unblock`](crate::Executor::unblock): threads
//! started on demand that run closures, and end after a while idle.
//!
//! The reference is `blocking`'s `unblock`: a closure is a task whose scheduler pushes it
//! on this pool's queue, a thread runs it, and its [`Task`] resolves with the output. The
//! pool is the executor's own, not a process-wide one, and it is capped so a flood of
//! blocking calls queues instead of starting a thread each.

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_task::{Runnable, Task};

use super::lock;
use crate::thread::spawn_named;

/// The most threads the pool runs at once; a closure past that waits for a thread to free.
/// It bounds the blocking calls in flight together (decodes, disk reads, IPC round trips)
/// far above what a session issues, while a flood still queues instead of starting a
/// thread per call.
const MAX_THREADS: usize = 32;

/// How long a thread with nothing to run waits before it ends.
pub(super) const IDLE_LIFE: Duration = Duration::from_millis(500);

#[derive(Default)]
struct State {
    queue: VecDeque<Runnable>,
    /// Threads waiting for a closure and not yet woken for one.
    idle: usize,
    /// Threads running, idle or not.
    threads: usize,
    /// Threads started so far, for their names.
    started: usize,
    closed: bool,
}

pub(super) struct Blocking {
    state: Mutex<State>,
    wake: Condvar,
    idle_life: Duration,
}

impl Blocking {
    pub(super) fn new(idle_life: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            idle_life,
        })
    }

    /// Queues `f` and returns its task, or the refusal of the thread it needed.
    pub(super) fn unblock<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<Task<T>> {
        let pool = Arc::clone(self);
        // A closure's future completes in its one poll, so this scheduler runs only for
        // the first schedule below, or for a cancel; neither has a caller to tell.
        let (runnable, task) = async_task::spawn(async move { f() }, move |runnable| {
            let _ = pool.schedule(runnable);
        });
        self.schedule(runnable)?;
        Ok(task)
    }

    /// Queues a runnable, and wakes an idle thread or starts one for it.
    ///
    /// # Errors
    ///
    /// The pool is closed, or no thread runs and none can start: the runnable, and every
    /// runnable queued behind it, is dropped unrun.
    fn schedule(self: &Arc<Self>, runnable: Runnable) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.closed {
            drop(state);
            drop(runnable);
            return Err(io::Error::other("the executor has shut down"));
        }
        state.queue.push_back(runnable);
        if state.queue.len() <= state.idle {
            self.wake.notify_one();
        } else if state.threads < MAX_THREADS {
            state.threads += 1;
            state.started += 1;
            let name = format!("tutti-blocking-{}", state.started);
            let pool = Arc::clone(self);
            match spawn_named(&name, None, move |_| pool.work()) {
                Ok(thread) => thread.detach(),
                Err(error) => {
                    state.threads -= 1;
                    if state.threads > 0 {
                        // A running thread takes the queued closures in turn.
                        return Ok(());
                    }
                    // With no thread to run on, the queued closures are cancelled.
                    let stranded = std::mem::take(&mut state.queue);
                    drop(state);
                    drop(stranded);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// A pool thread's body: runs closures until the pool closes or it idles out.
    fn work(&self) {
        let mut state = lock(&self.state);
        loop {
            if let Some(runnable) = state.queue.pop_front() {
                drop(state);
                // A closure that panics ends its own task: the thread takes the next.
                let _ = catch_unwind(AssertUnwindSafe(|| runnable.run()));
                state = lock(&self.state);
                continue;
            }
            if state.closed {
                break;
            }
            state.idle += 1;
            let (guard, wait) = self
                .wake
                .wait_timeout(state, self.idle_life)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
            state.idle -= 1;
            if wait.timed_out() && state.queue.is_empty() {
                break;
            }
        }
        state.threads -= 1;
        self.wake.notify_all();
    }

    /// Refuses further closures, drops the queued ones and releases the idle threads. A
    /// closure already running finishes on its thread, which then ends.
    pub(super) fn close(&self) {
        let queued = {
            let mut state = lock(&self.state);
            state.closed = true;
            std::mem::take(&mut state.queue)
        };
        drop(queued);
        self.wake.notify_all();
    }

    /// Waits until no pool thread runs. For tests of the idle exit.
    #[cfg(test)]
    pub(super) fn wait_until_empty(&self, hang_guard: Duration) -> bool {
        let state = lock(&self.state);
        let (state, _) = self
            .wake
            .wait_timeout_while(state, hang_guard, |state| state.threads > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.threads == 0
    }

    #[cfg(test)]
    pub(super) fn thread_count(&self) -> usize {
        lock(&self.state).threads
    }
}

#[cfg(test)]
#[path = "blocking/tests.rs"]
mod tests;
