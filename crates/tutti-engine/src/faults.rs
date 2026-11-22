//! Where a stream error goes when there is no call to return it from.
//!
//! A backend's error callback has no return value, so a device unplugged
//! mid-session would otherwise surface nowhere. The fault is stored in
//! [`StreamFaults`] for the host to read.
//!
//! [`StreamFaults::record`] can run on the audio thread: some backends call
//! their error callback on the data callback's worker thread, and CoreAudio can
//! call it inside the render callback. So every record path takes no lock,
//! allocates nothing and frees nothing: a fault is a fixed record (a stamp, a
//! kind and up to [`RT_TEXT_BYTES`] bytes of text, cut at a character boundary)
//! written into a sequence cell ([`SeqWriter`]) of its own, and counted. The
//! control side reads the cells and builds the owned message when it reads.
//!
//! There are three cells: backend faults (including a disconnect), panics and
//! overruns. [`StreamFaults::last`] reports the newest of a backend fault and a
//! panic; an overrun only when there is neither.

use std::fmt::{self, Display, Write as _};
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tutti_platform::Instant;

use bytemuck::{Pod, Zeroable};
use tutti_rt::{AudioThreadCell, Contended, Drained, FaultLatch, Mutex, SeqReader, SeqWriter};

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
    /// The backend's message, as its `Display` rendered it, or the text a
    /// panic's or overrun's recorder passed; cut to [`RT_TEXT_BYTES`] bytes at
    /// a character boundary.
    pub message: String,
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

/// The longest text a fault keeps, in bytes: what a recorder is given is cut
/// here, at a character boundary.
pub const RT_TEXT_BYTES: usize = 254;

/// One fault as the sequence cell carries it: plain bytes, so writing it
/// allocates nothing. `stamp` orders it against every other fault of the stream
/// (zero is never a stamp).
///
/// `RT_TEXT_BYTES = 254` makes it exactly 264 bytes (8 + 1 + 1 + 254, a
/// multiple of its 8-byte alignment), so the struct has no padding, which
/// `Pod` requires.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct RtFault {
    stamp: u64,
    kind: u8,
    len: u8,
    text: [u8; RT_TEXT_BYTES],
}

/// A `fmt::Write` into a fixed buffer: cuts at a character boundary and
/// refuses everything after the cut. Allocates nothing.
struct Sink {
    text: [u8; RT_TEXT_BYTES],
    len: usize,
    cut: bool,
}

impl fmt::Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.cut {
            return Err(fmt::Error);
        }
        let mut n = s.len().min(RT_TEXT_BYTES - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.text[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        if n < s.len() {
            self.cut = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl RtFault {
    /// `kind`'s code and back; a `kind` outside the table reads as a backend fault.
    fn code(kind: StreamFaultKind) -> u8 {
        match kind {
            StreamFaultKind::Disconnected => 0,
            StreamFaultKind::Backend => 1,
            StreamFaultKind::CallbackPanicked => 2,
            StreamFaultKind::Overrun => 3,
        }
    }

    /// `text` rendered into a fixed buffer, cut to fit. Calls `Display::fmt`
    /// on the caller's thread; allocates nothing itself.
    fn render(text: &dyn Display) -> Sink {
        let mut sink = Sink {
            text: [0; RT_TEXT_BYTES],
            len: 0,
            cut: false,
        };
        // A cut is the sink's refusal; the text so far is what is kept.
        let _ = write!(sink, "{text}");
        sink
    }

    fn of(stamp: u64, kind: StreamFaultKind, sink: Sink) -> Self {
        Self {
            stamp,
            kind: Self::code(kind),
            len: sink.len as u8,
            text: sink.text,
        }
    }

    fn fault(&self) -> StreamFault {
        let kind = match self.kind {
            0 => StreamFaultKind::Disconnected,
            1 => StreamFaultKind::Backend,
            2 => StreamFaultKind::CallbackPanicked,
            _ => StreamFaultKind::Overrun,
        };
        let len = usize::from(self.len).min(RT_TEXT_BYTES);
        StreamFault {
            kind,
            message: String::from_utf8_lossy(&self.text[..len]).into_owned(),
        }
    }
}

/// A fault cell: the write side behind an [`AudioThreadCell`] (a second
/// writer is refused, not waited for) and its reader.
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

    /// Claims the cell, takes the next stamp from `order` and writes the
    /// record `sink` and `kind` make with it. The stamp is taken inside the
    /// claim, so the stamps written to one cell rise in write order. Wait-free;
    /// allocates and frees nothing. Dropped (no stamp taken) if another thread
    /// is mid-write.
    fn write(&self, order: &AtomicU64, kind: StreamFaultKind, sink: Sink) {
        if let Some(mut writer) = self.writer.try_borrow_mut() {
            let stamp = order.fetch_add(1, Ordering::Relaxed) + 1;
            writer.write(RtFault::of(stamp, kind, sink));
        }
    }

    /// The last completed write, if there is one. Control side: it retries past
    /// a write in flight and gives up with [`Contended`] after a bounded number.
    fn read(&self) -> Result<Option<RtFault>, Contended> {
        self.reader.read(1024)
    }
}

impl fmt::Debug for RtCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RtCell(..)")
    }
}

/// A read found a fault cell mid-write and gave up after its retries: ask
/// again. Not an answer about the faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a fault was being written; ask again")]
pub struct Lagging;

/// What the control side holds: the stamp at or below which every fault is
/// forgotten.
#[derive(Debug, Default)]
struct Slots {
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
    /// Serialises the control side's reads; holds the forgetting floor.
    slots: Mutex<Slots>,
    /// Hands out the stamp that orders one fault against another.
    order: AtomicU64,
    /// The latest backend fault or disconnect, written on the error callback's
    /// thread.
    backend: RtCell,
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
            backend: RtCell::new(),
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
        self.slots.lock()
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
    /// answers `Drained::Failed(())` only if the pop found nothing
    /// ([`FaultLatch::drain`]). The order is the point: a pop first and a flag
    /// after could report a drained ring with frames the callback pushed in
    /// between.
    pub fn drain<T>(&self, pop: impl FnOnce() -> Option<T>) -> Drained<T> {
        self.disconnected.drain(pop)
    }

    /// The fault `last` reports and its stamp, among those newer than the
    /// floor and no newer than `ceiling`: the newest of the backend fault and
    /// the panic; an overrun only when neither is there, so it never replaces
    /// a latching fault's message. Also the largest stamp seen in any cell.
    ///
    /// A cell that is mid-write after the retries makes the read [`Lagging`],
    /// never a lower-priority fault than the one that cell may hold.
    fn newest(
        &self,
        floor: u64,
        ceiling: u64,
    ) -> Result<Option<((u64, StreamFault), u64)>, Lagging> {
        let read = |cell: &RtCell| {
            cell.read()
                .map(|fault| fault.filter(|f| f.stamp > floor && f.stamp <= ceiling))
                .map_err(|Contended| Lagging)
        };
        let (backend, panic, overrun) = (
            read(&self.backend)?,
            read(&self.panic)?,
            read(&self.overrun)?,
        );
        let seen = [backend, panic, overrun]
            .iter()
            .flatten()
            .map(|f| f.stamp)
            .max()
            .unwrap_or(0);
        let chosen = [backend, panic]
            .into_iter()
            .flatten()
            .max_by_key(|f| f.stamp)
            .or(overrun);
        Ok(chosen.map(|chosen| ((chosen.stamp, chosen.fault()), seen)))
    }

    /// Returns a copy of the most recent fault, if any.
    ///
    /// Reads without locking the writers out: a fault written while the read
    /// runs is seen by the next one, and a cell found mid-write after a
    /// bounded number of retries makes this answer `None` (it lags, as
    /// [`count`](Self::count) says) rather than a lesser fault.
    pub fn last(&self) -> Option<StreamFault> {
        let slots = self.last_guard();
        self.newest(slots.floor, u64::MAX)
            .ok()
            .flatten()
            .map(|((_, fault), _)| fault)
    }

    /// Takes the most recent fault, leaving `None`, for a host that shows each
    /// fault once. [`count`](Self::count) and
    /// [`is_disconnected`](Self::is_disconnected) are unaffected.
    ///
    /// What it forgets is what existed when it began (one snapshot of the
    /// stamp counter): the fault it returns and anything older, an overrun it
    /// outranked included. A fault recorded after the snapshot is reported by
    /// the next [`last`](Self::last).
    ///
    /// `Ok(None)` is an answer: nothing newer than what was taken. It is also
    /// what a counted fault whose text was lost leaves (a second thread wrote
    /// the same cell at that instant), so a caller that advances its seen count
    /// on `Ok(None)` does not spin on it. [`Lagging`] is not an answer: a cell
    /// was mid-write after the retries, nothing was taken or forgotten, and the
    /// caller asks again.
    ///
    /// One window remains across cells: a fault stamped and not yet written in
    /// one cell, older than a fault another cell already holds, is hidden by
    /// the take that reports the newer one (its count stands). Stamps in one
    /// cell rise in write order, so a cell never hides its own.
    ///
    /// # Errors
    ///
    /// [`Lagging`], as above.
    pub fn take_last(&self) -> Result<Option<StreamFault>, Lagging> {
        self.take_up_to(self.order.load(Ordering::Acquire))
    }

    /// [`take_last`](Self::take_last) over the faults stamped `ceiling` or
    /// less.
    fn take_up_to(&self, ceiling: u64) -> Result<Option<StreamFault>, Lagging> {
        let mut slots = self.last_guard();
        let Some(((_, fault), seen)) = self.newest(slots.floor, ceiling)? else {
            return Ok(None);
        };
        slots.floor = slots.floor.max(seen);
        Ok(Some(fault))
    }

    /// Records a fault from the backend's error callback: `kind` is the
    /// driver's reading of `err`, which supplies the message. Takes no lock,
    /// allocates and frees nothing, and never waits: `err` is rendered into a
    /// fixed record (cut to [`RT_TEXT_BYTES`]), so `err`'s own `Display` is the
    /// only code of the caller's that runs. Written message, then
    /// `disconnected`, then `count`, so a reader that sees the count also sees
    /// the message (unless another thread was writing the same cell at that
    /// instant, which loses its text and keeps its count).
    ///
    /// [`DeviceGone`](BackendErrorKind::DeviceGone) is a disconnect at once.
    /// [`Survivable`](BackendErrorKind::Survivable) says the stream keeps
    /// playing, so it is not a fault and is not recorded. Any other error
    /// starts a run, counted and described once; the run becomes a disconnect
    /// when errors still arrive [`STALL_GRACE`] later with no
    /// [`delivered`](Self::delivered) between. Errors repeated inside a run,
    /// and every backend error after the disconnect, only cost two atomic
    /// loads: no record, no count, so a backend that spins on `POLLERR` does not
    /// flood a host that reports each new count. The message is written on the
    /// first error of a run and at the disconnect.
    ///
    /// `now` is the instant of the error callback: the grace is measured
    /// between the instants given, never on the clock.
    pub fn record(&self, kind: BackendErrorKind, err: &dyn Display, now: Instant) {
        match kind {
            BackendErrorKind::Survivable => return,
            BackendErrorKind::DeviceGone => {
                self.publish(StreamFaultKind::Disconnected, err);
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
            self.publish(StreamFaultKind::Backend, err);
        } else if Duration::from_nanos(at.saturating_sub(started)) >= STALL_GRACE {
            self.publish(
                StreamFaultKind::Disconnected,
                &format_args!(
                    "no audio for {} ms while the backend kept failing: {err}",
                    STALL_GRACE.as_millis()
                ),
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
    /// reports (it replaces any other fault), and the `String` is built there, on
    /// the control side. A second thread writing the same cell at
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
    /// fault, a backend fault or a panic: [`last`](Self::last) may keep an older
    /// message, and an older kind, than the overrun.
    pub fn record_overrun(&self, what: &'static str) {
        self.publish_rt(StreamFaultKind::Overrun, what);
    }

    /// A fault recorded on the render thread: the text and a stamp go into
    /// the kind's own cell. No lock, no allocation, no free; see
    /// [`record_panic`](Self::record_panic).
    fn publish_rt(&self, kind: StreamFaultKind, what: &'static str) {
        self.publish(kind, &what);
    }

    /// Writes `text` as a fault of `kind` into the kind's cell, then raises the
    /// latch and counts. A reader that sees the count sees the record, unless
    /// another thread was writing the same cell, whose loser loses its text and
    /// keeps its count.
    fn publish(&self, kind: StreamFaultKind, text: &dyn Display) {
        let sink = RtFault::render(text);
        let cell = match kind {
            StreamFaultKind::CallbackPanicked => &self.panic,
            StreamFaultKind::Overrun => &self.overrun,
            StreamFaultKind::Disconnected | StreamFaultKind::Backend => &self.backend,
        };
        cell.write(&self.order, kind, sink);
        self.raise_and_count(kind);
    }

    /// Raises the latch a latching `kind` sets, then counts the fault.
    fn raise_and_count(&self, kind: StreamFaultKind) {
        if matches!(
            kind,
            StreamFaultKind::Disconnected | StreamFaultKind::CallbackPanicked
        ) {
            self.disconnected.raise(());
        }
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Forgets everything. Called when a stream starts, never while one is
    /// live: a fault recorded during it may be forgotten whole or in part.
    pub(crate) fn clear(&self) {
        let mut slots = self.last_guard();
        slots.floor = self.order.load(Ordering::Acquire);
        drop(slots);
        self.disconnected.clear();
        self.count.store(0, Ordering::Release);
        self.run_start.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
