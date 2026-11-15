//! Where a stream error goes when there is no call to return it from.
//!
//! A backend's error callback has no return value, so a device unplugged
//! mid-session would otherwise surface nowhere. The fault is stored in
//! [`StreamFaults`] for the host to read.
//!
//! [`StreamFaults::record`] can run on the audio thread: some backends call
//! their error callback on the data callback's worker thread, and CoreAudio can
//! call it inside the render callback. So `record` never waits: the counters
//! are atomics, and the lock around the last fault is only `try_lock`ed there.
//! It still allocates the message string, as a backend's own error path does.

use std::borrow::Cow;
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tutti_rt::{Drained, FaultLatch};

/// How long backend errors may arrive with no audio delivered before the
/// stream counts as gone.
///
/// A backend that cannot name a lost device reports it as an error that
/// repeats at spin rate (ALSA's `POLLERR` loop), and `POLLERR` is also what an
/// xrun raises, so neither the kind nor the message separates the two. What
/// does is time: an xrun is followed by data within a few periods, a lost
/// device is not. 500 ms is many periods at any usual buffer size and
/// still short enough to end a take promptly.
pub const STALL_GRACE: Duration = Duration::from_millis(500);

/// What kind of fault the backend reported.
///
/// A disconnect means the stream is gone and will not recover without a
/// restart, so [`OutputStream::is_running`](crate::OutputStream::is_running)
/// consults it. A backend-specific error may be transient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamFaultKind {
    /// The device went away: unplugged, or taken by an exclusive-mode client.
    Disconnected,
    /// Anything else the backend reported.
    Backend,
    /// The stream's own callback panicked. The stream is latched: every later
    /// callback writes silence and never renders again, since what panicked
    /// may have been left half-written. Only a new stream (a restart) renders
    /// again.
    CallbackPanicked,
    /// A clock that paces the stream itself (the timer driver) woke so far
    /// behind that it gave up the blocks it owed and began again from now.
    /// The stream keeps running: this is a count, not a loss of the stream.
    Overrun,
}

/// One fault reported by the backend, with the backend's own message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFault {
    /// Whether the device went away or the backend reported something else.
    pub kind: StreamFaultKind,
    /// The backend's message, as its `Display` rendered it; a fixed text for a
    /// panic, which is recorded on the audio thread and must not allocate.
    pub message: Cow<'static, str>,
}

/// What a backend's error means for the stream, as the driver classifies it
/// ([`StreamFaults::record`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendErrorKind {
    /// The backend says the device is gone: unplugged, or taken by an
    /// exclusive-mode client.
    DeviceGone,
    /// The backend reports a change the stream survives (a route change, a
    /// refused real-time promotion): not a fault.
    Survivable,
    /// Any other error. It may be transient (an xrun) or the first of a lost
    /// device's spinning errors; time tells them apart.
    Other,
}

/// The accumulated faults of one output stream, shared between the backend's
/// error callback and the host.
///
/// Created by [`OutputStream`](crate::OutputStream) and handed out by
/// [`OutputStream::faults`](crate::OutputStream::faults). The same handle
/// survives stop and restart, so a host can take it once at startup. Starting
/// a stream clears it, so a restart after a disconnect reads healthy again.
#[derive(Debug)]
pub struct StreamFaults {
    count: AtomicU64,
    /// Raised once the device is gone; a capture source reads it through
    /// `drain`, before its ring.
    disconnected: FaultLatch,
    last: Mutex<Option<StreamFault>>,
    /// Zero point of `run_start`.
    pub(crate) epoch: Instant,
    /// Nanoseconds after `epoch`, plus one, at which the current run of
    /// backend errors with no delivered audio began; zero when there is none.
    run_start: AtomicU64,
}

impl Default for StreamFaults {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFaults {
    /// Creates an empty fault record.
    #[must_use]
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            disconnected: FaultLatch::new(),
            last: Mutex::new(None),
            epoch: Instant::now(),
            run_start: AtomicU64::new(0),
        }
    }

    /// Notes that the stream delivered audio, which ends any run of backend
    /// errors: they were transient. Lock-free; the data callback calls it once
    /// per callback.
    pub fn delivered(&self) {
        self.run_start.store(0, Ordering::Relaxed);
    }

    fn last_guard(&self) -> MutexGuard<'_, Option<StreamFault>> {
        self.last.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns how many faults this stream has reported since the last start.
    ///
    /// Lock-free. A per-frame consumer compares this against what it saw last
    /// and calls [`last`](Self::last) only when it moved. The message of a
    /// fault recorded while `last` was being read is dropped, so `last` can
    /// lag the count.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// Returns whether any fault has been reported since the last start.
    pub fn is_faulted(&self) -> bool {
        self.count() > 0
    }

    /// Returns whether the stream is gone: the backend reported the device
    /// gone, or the stream's own callback panicked
    /// ([`StreamFaultKind::CallbackPanicked`]). Lock-free; stays set until the
    /// stream is started again.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.is_raised()
    }

    /// Reads the disconnect, **then** runs `pop` over the capture ring, and
    /// answers `Drained::Failed` only if the pop found nothing
    /// ([`FaultLatch::drain`]). The order is the point: a pop first and a flag
    /// after could report a drained ring with frames the callback pushed in
    /// between.
    pub fn drain<T>(&self, pop: impl FnOnce() -> Option<T>) -> Drained<T> {
        self.disconnected.drain(pop)
    }

    /// Returns a copy of the most recent fault, if any.
    pub fn last(&self) -> Option<StreamFault> {
        self.last_guard().clone()
    }

    /// Takes the most recent fault, leaving `None`, for a host that shows each
    /// fault once. [`count`](Self::count) and
    /// [`is_disconnected`](Self::is_disconnected) are unaffected.
    pub fn take_last(&self) -> Option<StreamFault> {
        self.last_guard().take()
    }

    /// Records a fault from the backend's error callback: `kind` is the
    /// driver's reading of `err`, which supplies the message. Never waits: if
    /// another thread holds `last`, the message is dropped and the count still
    /// moves. Written message, then `disconnected`, then `count`, so a reader
    /// that sees the count also sees the message.
    ///
    /// [`DeviceGone`](BackendErrorKind::DeviceGone) is a disconnect at once.
    /// [`Survivable`](BackendErrorKind::Survivable) says the stream keeps
    /// playing, so it is not a fault and is not recorded. Any other error
    /// starts a run, counted and described once; the run becomes a disconnect
    /// when errors still arrive [`STALL_GRACE`] later with no
    /// [`delivered`](Self::delivered) between. Errors repeated inside a run,
    /// and every backend error after the disconnect, only cost two atomic
    /// loads: no string, no count, so a backend that spins on `POLLERR` neither
    /// allocates nor floods a host that reports each new count. The message
    /// string is allocated on the first error of a run and at the disconnect,
    /// as a backend's own error path does.
    pub fn record(&self, kind: BackendErrorKind, err: &dyn Display) {
        self.record_at(kind, err, Instant::now());
    }

    /// [`record`](Self::record) at a given time, for tests.
    pub(crate) fn record_at(&self, kind: BackendErrorKind, err: &dyn Display, now: Instant) {
        match kind {
            BackendErrorKind::Survivable => return,
            BackendErrorKind::DeviceGone => {
                self.publish(StreamFaultKind::Disconnected, err.to_string().into());
                return;
            }
            BackendErrorKind::Other => {}
        }
        if self.is_disconnected() {
            return;
        }
        let at = now.saturating_duration_since(self.epoch).as_nanos() as u64 + 1;
        let started = self.run_start.load(Ordering::Relaxed);
        if started == 0 {
            self.run_start.store(at, Ordering::Relaxed);
            self.publish(StreamFaultKind::Backend, err.to_string().into());
        } else if Duration::from_nanos(at.saturating_sub(started)) >= STALL_GRACE {
            self.publish(
                StreamFaultKind::Disconnected,
                format!(
                    "no audio for {} ms while the backend kept failing: {err}",
                    STALL_GRACE.as_millis()
                )
                .into(),
            );
        }
    }

    /// Records that a callback of this stream panicked, from the audio thread
    /// that caught it: `what` names the callback. Allocates nothing and never
    /// waits (the message is dropped if another thread holds `last`). Raises
    /// the same latch a disconnect does, so
    /// [`OutputStream::is_running`](crate::OutputStream::is_running) reads
    /// `false` and a capture source answers `Failed`.
    pub fn record_panic(&self, what: &'static str) {
        self.publish_rt(StreamFaultKind::CallbackPanicked, what);
    }

    /// Records that a clock paced by a thread of its own fell too far behind
    /// and re-anchored ([`StreamFaultKind::Overrun`]): `what` says so. The
    /// stream continues, so this raises no disconnect. Allocates nothing and
    /// never waits.
    pub fn record_overrun(&self, what: &'static str) {
        self.publish_rt(StreamFaultKind::Overrun, what);
    }

    /// [`publish`](Self::publish) for a fault recorded on the render thread,
    /// which must not free: the message it replaces is never dropped there.
    ///
    /// - An overrun never replaces the message of a latching fault
    ///   ([`Disconnected`](StreamFaultKind::Disconnected),
    ///   [`CallbackPanicked`](StreamFaultKind::CallbackPanicked)), nor an owned
    ///   message it could not free; it only counts.
    /// - A panic replaces any message; an owned one it replaces is leaked,
    ///   once per stream, as the panic payload is.
    fn publish_rt(&self, kind: StreamFaultKind, what: &'static str) {
        let place = |last: &mut Option<StreamFault>| {
            let keep = kind == StreamFaultKind::Overrun
                && last.as_ref().is_some_and(|old| {
                    matches!(
                        old.kind,
                        StreamFaultKind::Disconnected | StreamFaultKind::CallbackPanicked
                    ) || matches!(old.message, Cow::Owned(_))
                });
            if !keep {
                let fault = StreamFault {
                    kind,
                    message: Cow::Borrowed(what),
                };
                if let Some(old) = last.replace(fault)
                    && matches!(old.message, Cow::Owned(_))
                {
                    std::mem::forget(old);
                }
            }
        };
        match self.last.try_lock() {
            Ok(mut last) => place(&mut last),
            Err(std::sync::TryLockError::Poisoned(p)) => place(&mut p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        self.raise_and_count(kind);
    }

    fn publish(&self, kind: StreamFaultKind, message: Cow<'static, str>) {
        let fault = StreamFault { kind, message };
        match self.last.try_lock() {
            Ok(mut last) => *last = Some(fault),
            Err(std::sync::TryLockError::Poisoned(p)) => *p.into_inner() = Some(fault),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        self.raise_and_count(kind);
    }

    /// Raises the latch a latching `kind` sets, then counts the fault.
    fn raise_and_count(&self, kind: StreamFaultKind) {
        if matches!(
            kind,
            StreamFaultKind::Disconnected | StreamFaultKind::CallbackPanicked
        ) {
            self.disconnected.raise();
        }
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Forgets everything. Called when a stream starts.
    pub(crate) fn clear(&self) {
        *self.last_guard() = None;
        self.disconnected.clear();
        self.count.store(0, Ordering::Release);
        self.run_start.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
