//! A stream that lives and dies on one thread, behind a `Send` handle.
//!
//! `cpal::Stream` is `!Send` because CoreAudio and WASAPI tie it to the
//! thread that created it. [`Owned`] builds the stream on a dedicated thread of
//! the session's executor ([`Executor::spawn_dedicated_local`]), keeps it in a
//! future that waits for the stop, and drops it there when the handle is
//! dropped. The handle holds only a [`Task`] and the stop, so it is `Send`
//! without an `unsafe` impl. Do not create a COM object on the owner thread
//! that another thread calls into: it is a WASAPI STA with no message pump.

use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Poll, Waker};
use std::time::Duration;

use tutti_platform::{Executor, Task, is_driving, join_within, panic_message};

use crate::error::{DeviceError, Result};

/// The longest a drop waits for the owner thread to drop its value. A stream
/// that has not stopped by then is left to finish on its own thread.
const STOP_PATIENCE: Duration = Duration::from_secs(5);

/// The stop request the owner's future waits on: a flag and the waker of the
/// future that reads it.
#[derive(Debug, Default)]
struct Stop {
    state: Mutex<StopState>,
}

#[derive(Debug, Default)]
struct StopState {
    requested: bool,
    waker: Option<Waker>,
}

impl Stop {
    /// Requests the stop and wakes the future waiting for it. Idempotent.
    fn request(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.requested = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Resolves once [`request`](Self::request) has been called.
    fn requested(&self) -> impl Future<Output = ()> + '_ {
        poll_fn(|cx| {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.requested {
                Poll::Ready(())
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

/// The handle to a value owned by its own thread. Dropping it asks that
/// thread to drop the value and waits for it (at most [`STOP_PATIENCE`]), so
/// the stream is stopped when the drop returns.
///
/// A drop on a thread that drives an executor does not wait: it asks for the
/// stop and detaches, with a warning.
#[derive(Debug)]
pub(crate) struct Owned {
    stop: Arc<Stop>,
    task: Option<Task<()>>,
}

impl Owned {
    /// Runs `build` on a new dedicated thread of `executor` and keeps the value
    /// it returns there until `self` is dropped. The value need not be `Send`:
    /// it is created, held and dropped on that one thread.
    ///
    /// # Errors
    /// Whatever `build` returns, or [`DeviceError::Io`] if the thread cannot be
    /// spawned or `build` panics.
    pub(crate) fn spawn<S: 'static>(
        executor: &Executor,
        name: &str,
        build: impl FnOnce() -> Result<S> + Send + 'static,
    ) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let stop = Arc::new(Stop::default());
        let held = Arc::clone(&stop);
        let task = executor.spawn_dedicated_local(name, None, move || {
            let built = catch_unwind(AssertUnwindSafe(build));
            let value = match built {
                Ok(Ok(value)) => {
                    let _ = ready_tx.send(Ok(()));
                    Some(value)
                }
                Ok(Err(e)) => {
                    let _ = ready_tx.send(Err(e));
                    None
                }
                Err(payload) => {
                    let _ = ready_tx.send(Err(DeviceError::Io(std::io::Error::other(format!(
                        "the stream thread panicked while opening: {}",
                        panic_message(&*payload)
                    )))));
                    None
                }
            };
            async move {
                if value.is_some() {
                    held.requested().await;
                }
                drop(value);
            }
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                task: Some(task),
            }),
            Ok(Err(e)) => Err(e),
            // The sender dropped without a result: the executor ended before
            // the thread ran `build`.
            Err(_) => Err(DeviceError::Io(std::io::Error::other(
                "the stream thread ended before opening",
            ))),
        }
    }
}

impl Drop for Owned {
    /// Asks the owner thread to drop the value and waits for it.
    fn drop(&mut self) {
        self.stop.request();
        let Some(task) = self.task.take() else {
            return;
        };
        if is_driving() {
            tracing::warn!("a stream is dropped on an executor's thread; leaving it to stop there");
            task.detach();
        } else if let Err(task) = join_within(task, STOP_PATIENCE) {
            tracing::warn!("the stream thread did not stop in time; leaving it");
            task.detach();
        }
    }
}

const _: () = {
    const fn send<T: Send>() {}
    send::<Owned>();
};

#[cfg(test)]
mod tests;
