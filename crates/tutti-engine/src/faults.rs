//! Where a stream error goes when there is no call to return it from.
//!
//! A backend's error callback has no return value, so a device unplugged
//! mid-session would otherwise surface nowhere. The fault is stored in
//! [`StreamFaults`] for the host to read.
//!
//! [`StreamFaults::record`] can run on the audio thread: some backends call
//! their error callback on the data callback's worker thread, and CoreAudio can
//! call it inside the render callback. So `record` never waits: the counters
//! are atomics, and the lock around the last owned fault is only `try_lock`ed
//! there. It still allocates the message string, as a backend's own error path
//! does.
//!
//! [`StreamFaults::record_panic`] and [`StreamFaults::record_overrun`] are the
//! render thread's own: they take no lock, allocate nothing and free nothing.
//! Each writes its `&'static str` into a sequence cell of its own
//! ([`SeqWriter`]) and counts; the control side builds the owned message when
//! it reads.

use std::borrow::Cow;
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use tutti_rt::{AudioThreadCell, Drained, FaultLatch, SeqReader, SeqWriter};

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
    /// The backend's message, as its `Display` rendered it; for a panic or an
    /// overrun, the text its recorder passed, cut to
    /// [`RT_TEXT_BYTES`] bytes at a character boundary.
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

/// The longest text a render-thread fault keeps, in bytes: what
/// [`StreamFaults::record_panic`] and [`StreamFaults::record_overrun`] are
/// given is cut here, at a character boundary.
pub const RT_TEXT_BYTES: usize = 126;

/// One render-thread fault as the sequence cell carries it: plain bytes, so
/// writing it allocates nothing. `stamp` orders it against every other fault
/// of the stream (zero is never a stamp).
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct RtFault {
    stamp: u64,
    kind: u8,
    len: u8,
    text: [u8; RT_TEXT_BYTES],
}

impl RtFault {
    const PANIC: u8 = 0;
    const OVERRUN: u8 = 1;

    fn new(stamp: u64, kind: u8, what: &str) -> Self {
        let mut len = what.len().min(RT_TEXT_BYTES);
        while !what.is_char_boundary(len) {
            len -= 1;
        }
        let mut text = [0; RT_TEXT_BYTES];
        text[..len].copy_from_slice(&what.as_bytes()[..len]);
        Self {
            stamp,
            kind,
            len: len as u8,
            text,
        }
    }

    fn fault(&self) -> StreamFault {
        let kind = if self.kind == Self::PANIC {
            StreamFaultKind::CallbackPanicked
        } else {
            StreamFaultKind::Overrun
        };
        let len = usize::from(self.len).min(RT_TEXT_BYTES);
        StreamFault {
            kind,
            message: Cow::Owned(String::from_utf8_lossy(&self.text[..len]).into_owned()),
        }
    }
}

/// A render-thread fault cell: the write side behind an [`AudioThreadCell`]
/// (a second writer is refused, not waited for) and its reader.
struct RtCell {
    writer: AudioThreadCell<SeqWriter<RtFault>>,
    reader: SeqReader<RtFault>,
}

impl RtCell {
    fn new() -> Self {
        let writer = SeqWriter::new();
        let reader = writer.reader();
        Self {
            writer: AudioThreadCell::new(writer),
            reader,
        }
    }

    /// Wait-free; allocates and frees nothing. Dropped if another thread is
    /// mid-write.
    fn write(&self, fault: RtFault) {
        if let Some(mut writer) = self.writer.try_borrow_mut() {
            writer.write(fault);
        }
    }

    /// The last completed write newer than `floor`. Control side: it retries
    /// past a write in flight and gives up (`None`) after a bounded number.
    fn read(&self, floor: u64) -> Option<RtFault> {
        match self.reader.read(1024) {
            Ok(Some(fault)) if fault.stamp > floor => Some(fault),
            _ => None,
        }
    }
}

impl std::fmt::Debug for RtCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RtCell(..)")
    }
}

/// What the control side holds: the latest owned fault with its stamp, and
/// the stamp at or below which every fault is forgotten.
#[derive(Debug, Default)]
struct Slots {
    owned: Option<(u64, StreamFault)>,
    floor: u64,
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
    slots: Mutex<Slots>,
    /// Hands out the stamp that orders one fault against another.
    order: AtomicU64,
    /// The latest panic, written on the render thread.
    panic: RtCell,
    /// The latest overrun, written on the timer's thread.
    overrun: RtCell,
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
            slots: Mutex::new(Slots::default()),
            order: AtomicU64::new(0),
            panic: RtCell::new(),
            overrun: RtCell::new(),
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

    fn last_guard(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns how many faults this stream has reported since the last start.
    ///
    /// Lock-free. A per-frame consumer compares this against what it saw last
    /// and calls [`last`](Self::last) only when it moved. The message of a
    /// fault recorded while the slots were being read is dropped, and one
    /// written while the control side reads is seen by the next read, so `last`
    /// can lag the count.
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

    /// The fault `last` reports and its stamp: the newest of the owned fault
    /// and the latest panic; an overrun only when neither is there, so it
    /// never replaces a latching fault's message or an owned one.
    fn newest(&self, slots: &Slots) -> Option<(u64, StreamFault)> {
        let panic = self
            .panic
            .read(slots.floor)
            .map(|fault| (fault.stamp, fault.fault()));
        let firm = [
            slots
                .owned
                .clone()
                .filter(|(stamp, _)| *stamp > slots.floor),
            panic,
        ]
        .into_iter()
        .flatten()
        .max_by_key(|(stamp, _)| *stamp);
        firm.or_else(|| {
            self.overrun
                .read(slots.floor)
                .map(|fault| (fault.stamp, fault.fault()))
        })
    }

    /// Returns a copy of the most recent fault, if any.
    pub fn last(&self) -> Option<StreamFault> {
        let slots = self.last_guard();
        self.newest(&slots).map(|(_, fault)| fault)
    }

    /// Takes the most recent fault, leaving `None`, for a host that shows each
    /// fault once. [`count`](Self::count) and
    /// [`is_disconnected`](Self::is_disconnected) are unaffected.
    pub fn take_last(&self) -> Option<StreamFault> {
        let mut slots = self.last_guard();
        let (stamp, fault) = self.newest(&slots)?;
        // Everything recorded so far goes with it, an overrun it outranked
        // included.
        slots.floor = slots
            .floor
            .max(stamp)
            .max(self.overrun.read(0).map_or(0, |fault| fault.stamp));
        slots.owned = None;
        Some(fault)
    }

    /// Records a fault from the backend's error callback: `kind` is the
    /// driver's reading of `err`, which supplies the message. Never waits: if
    /// another thread holds the slots, the message is dropped and the count still
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
    /// that caught it: `what` names the callback. Allocates, locks and
    /// frees nothing, and never waits. Raises
    /// the same latch a disconnect does, so
    /// [`OutputStream::is_running`](crate::OutputStream::is_running) reads
    /// `false` and a capture source answers `Failed`.
    ///
    /// The audio side does a `fetch_add` on the stamp (`Relaxed`: it only
    /// orders faults), a [`SeqWriter`] write of the text (a few `Relaxed`
    /// atomic stores between two `Release` marks) and the count; the panic's
    /// text, cut to [`RT_TEXT_BYTES`], is what [`last`](Self::last) then
    /// reports (it replaces any other fault), and the owned `String` is built
    /// there, on the control side. A second thread writing the same cell at
    /// the same instant loses its text; the count still moves.
    pub fn record_panic(&self, what: &'static str) {
        self.publish_rt(StreamFaultKind::CallbackPanicked, what);
    }

    /// Records that a clock paced by a thread of its own fell too far behind
    /// and re-anchored ([`StreamFaultKind::Overrun`]): `what` says so. The
    /// stream continues, so this raises no disconnect. Allocates, locks and
    /// frees nothing, and never waits (see [`record_panic`](Self::record_panic)).
    ///
    /// An overrun counts, but it does not replace the message of a latching
    /// fault, a panic or an owned one: [`last`](Self::last) may keep an older
    /// message, and an older kind, than the overrun.
    pub fn record_overrun(&self, what: &'static str) {
        self.publish_rt(StreamFaultKind::Overrun, what);
    }

    /// A fault recorded on the render thread: the text and a stamp go into
    /// the kind's own cell. No lock, no allocation, no free; see
    /// [`record_panic`](Self::record_panic).
    fn publish_rt(&self, kind: StreamFaultKind, what: &'static str) {
        let stamp = self.order.fetch_add(1, Ordering::Relaxed) + 1;
        match kind {
            StreamFaultKind::CallbackPanicked => {
                self.panic.write(RtFault::new(stamp, RtFault::PANIC, what))
            }
            _ => self
                .overrun
                .write(RtFault::new(stamp, RtFault::OVERRUN, what)),
        }
        self.raise_and_count(kind);
    }

    fn publish(&self, kind: StreamFaultKind, message: Cow<'static, str>) {
        let fault = StreamFault { kind, message };
        let stamp = self.order.fetch_add(1, Ordering::Relaxed) + 1;
        match self.slots.try_lock() {
            Ok(mut slots) => slots.owned = Some((stamp, fault)),
            Err(std::sync::TryLockError::Poisoned(p)) => {
                p.into_inner().owned = Some((stamp, fault));
            }
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
        let mut slots = self.last_guard();
        slots.owned = None;
        slots.floor = self.order.load(Ordering::Relaxed);
        drop(slots);
        self.disconnected.clear();
        self.count.store(0, Ordering::Release);
        self.run_start.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
