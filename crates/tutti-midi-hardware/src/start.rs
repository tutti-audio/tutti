//! Waiting for a backend to be ready to open endpoints.
//!
//! A backend may have to start something before it can create an endpoint or open a
//! port: CoreMIDI's one client is made on a thread of the backend's own, and the MIDI
//! server it connects to may be restarting for several seconds. That wait ends when
//! the backend's thread says so, so it is a [`Future`]: [`Start`], which another
//! thread wakes, with no executor of this crate behind it. A backend that has
//! nothing to start is ready at once.
//!
//! [`Started`] is what the future resolves to: the proof a caller holds that the
//! backend is ready, which opening a port or a virtual endpoint takes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::error::MidiHardwareError;

/// The proof that the backend is ready to open endpoints and ports, which [`Start`]
/// resolves to. Only a finished start makes one.
#[derive(Clone, Copy, Debug)]
pub struct Started(());

impl Started {
    /// The proof, made by a start that finished.
    pub(crate) fn new() -> Self {
        Self(())
    }
}

/// Why a [`Start`] did not resolve to a [`Started`]: the error the backend failed to
/// start with, or [`MidiHardwareError::ThreadEnded`] when its thread ended before it
/// answered. Every future waiting on one start gets the same cause, shared.
#[derive(Clone, Debug, thiserror::Error)]
#[error("the MIDI backend did not start: {0}")]
pub struct StartFailed(#[source] Arc<MidiHardwareError>);

impl StartFailed {
    /// The error the start failed with.
    pub fn cause(&self) -> &MidiHardwareError {
        &self.0
    }
}

/// What a [`Start`] waits on: a backend's state, polled with the waker to wake.
pub(crate) trait Waiting: Send + Sync {
    /// The outcome of the start numbered `from`, or `Pending` with `waker` stored to
    /// be woken when it changes.
    fn poll(&self, from: u64, waker: &Waker) -> Poll<Result<(), Arc<MidiHardwareError>>>;
}

/// A start that has nothing to wait on.
struct Done;

impl Waiting for Done {
    fn poll(&self, _from: u64, _waker: &Waker) -> Poll<Result<(), Arc<MidiHardwareError>>> {
        Poll::Ready(Ok(()))
    }
}

/// The wait for the backend to be ready: a `Send` future fed by the backend's
/// own start thread, which wakes it (ready on the first poll where there is
/// nothing to start). [`MidiEndpoints::start`](crate::MidiEndpoints::start),
/// [`Devices::start`](crate::Devices::start) and
/// [`MidiSession::start`](crate::MidiSession::start) make one.
///
/// It resolves to [`Started`], or to the error the backend failed to start with. A
/// backend with nothing to start (ALSA, Windows Multimedia, the stub) is ready on
/// the first poll. On macOS the process's one CoreMIDI client is made on a thread of
/// the backend's own, and this resolves when that thread has made it, or fails with
/// [`StartFailed`]. Making a `Start` after a failed one asks
/// the backend to start again.
///
/// ```
/// fn crosses_threads<T: Send>() {}
/// crosses_threads::<tutti_midi_hardware::Start>();
/// ```
#[must_use = "a start does nothing until it is awaited"]
pub struct Start {
    wait: Arc<dyn Waiting>,
    from: u64,
}

impl core::fmt::Debug for Start {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Start").finish_non_exhaustive()
    }
}

impl Start {
    /// A start over `wait`, of the round numbered `from`.
    pub(crate) fn over(wait: Arc<dyn Waiting>, from: u64) -> Self {
        Self { wait, from }
    }

    /// A start that is ready on the first poll: the default of a backend that
    /// has nothing to start.
    pub(crate) fn ready() -> Self {
        Self::over(Arc::new(Done), 0)
    }
}

impl Future for Start {
    type Output = Result<Started, StartFailed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.wait
            .poll(self.from, cx.waker())
            .map(|outcome| outcome.map(|()| Started::new()).map_err(StartFailed))
    }
}

#[cfg(test)]
mod tests;
