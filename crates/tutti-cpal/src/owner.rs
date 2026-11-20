//! A stream that lives and dies on one thread, behind a `Send` handle.
//!
//! `cpal::Stream` is `!Send` because CoreAudio and WASAPI tie it to the
//! thread that created it. [`Owned`] builds the stream on a dedicated thread,
//! keeps it there, and drops it there when the handle is dropped. The handle
//! holds only a [`ThreadHandle`], so it is `Send` without an `unsafe impl`. Do
//! not create a COM object on the owner thread that another thread calls into:
//! it is a WASAPI STA with no message pump.

use std::sync::mpsc;
use std::time::Duration;

use tutti_platform::{ThreadHandle, spawn_named};

use crate::error::{DeviceError, Result};

/// The handle to a value owned by its own thread. Dropping it asks that
/// thread to drop the value and waits for it, so the stream is stopped when the drop
/// returns.
#[derive(Debug)]
pub(crate) struct Owned {
    // Dropped with `self`: requests the stop and joins. A panic in the value's
    // own drop is the owner thread's, caught there and not re-raised here.
    _thread: ThreadHandle<()>,
}

impl Owned {
    /// Runs `build` on a new thread and keeps the value it returns there until
    /// `self` is dropped. The value need not be `Send`: it is created, held and
    /// dropped on that one thread.
    ///
    /// # Errors
    /// Whatever `build` returns, or [`DeviceError::Io`] if the thread cannot be
    /// spawned or `build` panics.
    pub(crate) fn spawn<S: 'static>(
        name: &str,
        build: impl FnOnce() -> Result<S> + Send + 'static,
    ) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = spawn_named(name, None, move |stop| match build() {
            Ok(value) => {
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while !stop.wait(Duration::MAX) {}
                drop(value);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { _thread: thread }),
            Ok(Err(e)) => Err(e),
            // The sender dropped without a result: `build` panicked.
            Err(_) => Err(DeviceError::Io(std::io::Error::other(
                "the stream thread panicked while opening",
            ))),
        }
    }
}

const _: () = {
    const fn send<T: Send>() {}
    send::<Owned>();
};

#[cfg(test)]
mod tests;
