//! [`spawn_named`]: the one way a control-side or worker thread starts.
//!
//! The thread is named, may ask the OS for a priority ([`PriorityRequest`]),
//! and runs a body that receives a [`StopToken`]. The [`ThreadHandle`] it
//! returns stops the thread and joins it when dropped, so a thread never
//! outlives its owner by accident; [`ThreadHandle::detach`] is the one
//! spelling of "let it run on".
//!
//! This is control-side code: spawning allocates, joining blocks, and the
//! token's [`wait`](StopToken::wait) sleeps. None of it is for the audio
//! callback.
//!
//! # Faults
//!
//! A panic in the body is caught on its own thread and takes one path: the
//! handle's [`FaultLatch`] is raised ([`ThreadHandle::panicked`]) and
//! [`ThreadHandle::join`] returns it as [`Panicked`], once. A handle dropped
//! without a join discards the report, and the dropping thread does not
//! unwind.

use std::any::Any;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{Builder, JoinHandle};
use std::time::{Duration, Instant};

use crate::fault_latch::FaultLatch;
use crate::priority::{PriorityOutcome, PriorityRequest, request_priority};

use std::sync::atomic::{AtomicBool, Ordering};

/// A thread's body panicked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("thread `{name}` panicked: {message}")]
pub struct Panicked {
    /// The thread's name.
    pub name: String,
    /// The panic's message, or `(not a string)` for a payload that is not one.
    pub message: String,
}

#[derive(Debug, Default)]
struct StopState {
    stopped: AtomicBool,
    /// Held only to order a stop against a sleeper's check.
    gate: Mutex<()>,
    wake: Condvar,
}

/// A stop request a thread's body polls or sleeps on: a shared flag and a wake.
///
/// Clones share one request. Raising it never waits on the body.
#[derive(Debug, Clone, Default)]
pub struct StopToken {
    state: Arc<StopState>,
}

impl StopToken {
    /// A token that is not stopped.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a stop was requested. One atomic load.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.state.stopped.load(Ordering::Acquire)
    }

    /// Requests the stop and wakes every sleeper. Idempotent.
    pub fn stop(&self) {
        // The flag is set under the gate, so a sleeper that checked it before
        // the store is already parked on the condvar when the notify comes.
        let _gate = self
            .state
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state.stopped.store(true, Ordering::Release);
        self.state.wake.notify_all();
    }

    /// Sleeps for at most `timeout`, waking early when the stop is requested.
    /// Returns whether the stop was requested.
    #[must_use = "true means the body should return"]
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut gate = self
            .state
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.is_stopped() {
                return true;
            }
            let left = match deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return false,
                },
                None => Duration::MAX,
            };
            gate = self
                .state
                .wake
                .wait_timeout(gate, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// The owner of a thread started by [`spawn_named`]: dropping it requests the
/// stop and joins.
///
/// `T` is what the body returns. A body that never looks at its token ends
/// by itself (a closed queue, say), and the drop's join waits for that.
#[must_use = "dropping the handle stops the thread and waits for it; `detach` lets it run on"]
#[derive(Debug)]
pub struct ThreadHandle<T> {
    stop: StopToken,
    fault: Arc<FaultLatch>,
    priority: Option<PriorityOutcome>,
    join: Option<JoinHandle<Result<T, Panicked>>>,
}

impl<T> ThreadHandle<T> {
    /// Requests the stop without waiting. Idempotent.
    pub fn request_stop(&self) {
        self.stop.stop();
    }

    /// A clone of the thread's token, to stop it from elsewhere.
    #[must_use]
    pub fn token(&self) -> StopToken {
        self.stop.clone()
    }

    /// Whether the body has returned or panicked. Never blocks.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Whether the body panicked. Never blocks.
    #[must_use]
    pub fn panicked(&self) -> bool {
        self.fault.is_raised()
    }

    /// What the OS did with the priority asked of [`spawn_named`], or `None`
    /// when none was asked.
    #[must_use]
    pub fn priority(&self) -> Option<&PriorityOutcome> {
        self.priority.as_ref()
    }

    /// Waits for the body to return, without requesting the stop, and hands
    /// back what it returned.
    ///
    /// # Errors
    ///
    /// [`Panicked`] when the body panicked.
    ///
    /// # Panics
    ///
    /// Never in practice: the handle holds its thread until this call or the
    /// drop, and the thread's wrapper catches the body's panic, so the join
    /// finds a result.
    pub fn join(mut self) -> Result<T, Panicked> {
        let join = self
            .join
            .take()
            .expect("a handle is joined once, and only here");
        // The body's panic is caught on its own thread, so the join finds the
        // closure's result; the closure around the catch cannot unwind.
        join.join()
            .expect("the thread's closure catches the body's panic")
    }

    /// Lets the thread run on, owned by nobody: no stop, no join.
    pub fn detach(mut self) {
        self.join = None;
    }
}

impl<T> Drop for ThreadHandle<T> {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            self.stop.stop();
            let _ = join.join();
        }
    }
}

fn message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(not a string)".to_owned())
}

/// Starts a thread named `name` running `body`, after asking the OS for
/// `priority` on it (a refusal is not an error: it is read from
/// [`ThreadHandle::priority`]).
///
/// `body` receives the thread's [`StopToken`]. A panic in it is caught and
/// reported through the handle (see the module docs). The call returns once
/// the thread has applied its priority and is about to run the body.
///
/// # Errors
///
/// The OS's refusal to start a thread.
pub fn spawn_named<T, F>(
    name: &str,
    priority: Option<PriorityRequest>,
    body: F,
) -> io::Result<ThreadHandle<T>>
where
    T: Send + 'static,
    F: FnOnce(&StopToken) -> T + Send + 'static,
{
    let stop = StopToken::new();
    let fault = Arc::new(FaultLatch::new());
    let (applied_tx, applied_rx) = mpsc::sync_channel::<Option<PriorityOutcome>>(1);
    let join = {
        let (stop, fault, name_owned) = (stop.clone(), Arc::clone(&fault), name.to_owned());
        Builder::new().name(name.to_owned()).spawn(move || {
            let _ = applied_tx.send(priority.map(request_priority));
            drop(applied_tx);
            catch_unwind(AssertUnwindSafe(|| body(&stop))).map_err(|payload| {
                fault.raise();
                Panicked {
                    name: name_owned,
                    message: message(payload.as_ref()),
                }
            })
        })?
    };
    // The sender is dropped without a send only if the thread died before
    // reaching it; there is no priority to report then.
    let priority = applied_rx.recv().ok().flatten();
    Ok(ThreadHandle {
        stop,
        fault,
        priority,
        join: Some(join),
    })
}

#[cfg(test)]
mod tests;
