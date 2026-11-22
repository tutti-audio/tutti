//! [`InputInlets`]: the source of the event inlets of the hardware MIDI inputs, and
//! the producer end ([`DeviceInput`]) a device's thread stamps and pushes through.
//!
//! A device's thread learns when an event arrived from the OS, as an
//! [`Instant`]. The graph wants a frame. The thread places each arrival itself,
//! so the audio thread converts nothing: it holds a [`ClockMap`] fed the
//! engine's block-start readings (an [`ArrivalClock`]: one per render call, the
//! OS time the block began and the graph frame it began at) and pushes the
//! event for the [`Moment`] the map names, through the device's own
//! [`EventInletWriter`]. The inlet holds the event until its block and emits it
//! on its frame. **The map is the device's own**: each [`DeviceInput`] keeps
//! one, fed from the shared clock, so a device that restarts or whose clock
//! reading is rejected re-anchors alone.
//!
//! # The lead
//!
//! An event that arrived between two blocks maps to a frame before the next
//! block's start: the block that would play it has already begun by the time
//! the thread pushes it, so it would land late, at offset 0, and the spacing
//! of a run of events would collapse. The thread therefore places each arrival
//! a fixed **lead** after its mapped frame ([`InputInlets::new`]: a block's
//! length is the natural value). An event then sounds exactly the lead after
//! it arrived, whatever the block boundaries, and the spacing between events
//! is the map's: within the bound [`ClockMap`] states. An event is late, and counted so
//! (`late` in [`InletCounts::losses`]), only when the
//! thread's push comes more than the lead after its arrival.
//!
//! # Timing and losses
//!
//! Each [`DeviceInput`] reports through the [`DeviceWriters`] it is opened with
//! ([`open_with`](InputInlets::open_with)): a gauge of the latest correction its
//! map made (signed, positive when a reading came late), a counter of its
//! re-anchors, a [`Resyncs`] each, a counter of the pushes its ring had no room
//! for, and the totals of what its inlet evicted, refused and emitted late
//! ([`Tally`]). The MIDI-CI refusals of the device's output are the
//! stream's own cell, the other half of [`InputWriters`].
//!
//! The readers ([`InputReaders`]) are made once, by [`InputWriters::new`], and
//! outlive any one connection: [`InputWriters::reopen`] hands the writers to the
//! next connection once the last has dropped, so the counts are cumulative
//! across connections and the gauge holds its last reading until the next map
//! writes one. The device's thread writes with atomic stores, so reading them
//! blocks nothing.
//!
//! # One inlet per input
//!
//! [`open`](InputInlets::open) makes one inlet for one input: the [`DeviceInput`]
//! its thread pushes through, and the [`EventInletNode`] the graph's owner adds
//! to the graph and wires to the input's input node. Nothing is shared between
//! inputs but the clock and the lead, so there is no table of slots and no cap.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_graph::{EventCapacity, EventInletNode, EventInletWriter, InletCounts, Stamp};
use tutti_rt::Param64;
use tutti_rt::{
    Counter, CounterWriter, Gauge, GaugeWriter, Publisher, PushError, Reader, Tally, counter,
    gauge, tally,
};
use tutti_ump::Ump;
use tutti_units::Losses;
use tutti_units::{Moment, Resyncs, Seconds};

use crate::clock_map::{ClockMap, ClockReading, Observed};

/// The engine's block clock as a device's thread reads it: the latest render
/// call's start as an [`Instant`] and the graph [`Moment`] it began at, `None`
/// before the first call. A function, not an engine type: this crate depends on
/// no engine, and whoever owns both ends (the session) builds it, converting
/// the engine's OS nanoseconds onto the `Instant`s the backends stamp with.
/// Called from each device's thread on every push: lock-free and
/// allocation-free.
pub type ArrivalClock = Arc<dyn Fn() -> Option<(Instant, Moment)> + Send + Sync>;

/// One open input's identity among the inputs one [`InputInlets`] opened:
/// minted in the order they opened and never reused, so a stale id names
/// nothing.
///
/// Ephemeral: it names a connection of this run, so it has no serde (a saved
/// device is a `MidiDeviceId`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InputId(NonZeroU64);

impl InputId {
    /// The id's number: unique for the inlets' life, increasing in the order
    /// inputs opened, from 1.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// The inlets of the hardware inputs, shared between the threads that connect
/// devices and the owner of the graph. See the module docs.
pub struct InputInlets {
    /// The last id minted.
    last: AtomicU64,
    capacity: EventCapacity,
    /// What each device's thread adds to an arrival's frame.
    lead: Arc<Param64<Seconds>>,
    /// The engine's block clock, lent to each device's thread. `None` until
    /// the owner has an engine ([`set_clock`](Self::set_clock)).
    clock: Publisher<Option<ArrivalClock>>,
}

impl std::fmt::Debug for InputInlets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputInlets")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl InputInlets {
    /// Inlets holding `capacity` events each, with `lead` the time each
    /// arrival is placed after its mapped frame (see the module docs).
    #[must_use]
    pub fn new(capacity: EventCapacity, lead: Duration) -> Arc<Self> {
        Arc::new(Self {
            last: AtomicU64::new(0),
            capacity,
            lead: Arc::new(Param64::new(Seconds::new(lead.as_secs_f64()))),
            clock: Publisher::new(None),
        })
    }

    /// Sets the engine's block clock the devices' threads place arrivals
    /// with (`None`: there is none, and events are pushed for the next
    /// block). Takes effect on each thread's next event. Control side.
    pub fn set_clock(&self, clock: Option<ArrivalClock>) {
        self.clock.publish(clock);
    }

    /// Sets the lead (see the module docs); each device's thread reads it on
    /// its next event.
    pub fn set_lead(&self, lead: Duration) {
        self.lead.store(Seconds::new(lead.as_secs_f64()));
    }

    /// Makes one input's inlet: the end its thread pushes through and the
    /// node the graph's owner adds. Control side: allocates the inlet's ring
    /// and waiting list. The inlet ends with the node (taken out of the graph)
    /// and the [`DeviceInput`] (dropped).
    #[must_use = "the node is the inlet's graph side: until it is added, what the device sends goes nowhere"]
    pub fn open(&self) -> (DeviceInput, EventInletNode) {
        self.open_with(DeviceWriters::unread())
    }

    /// [`open`](Self::open) with the input's cells written to `writers`: the
    /// readers of its cells stay with whoever made them ([`InputWriters::new`]).
    #[must_use = "the node is the inlet's graph side: until it is added, what the device sends goes nowhere"]
    pub fn open_with(&self, writers: DeviceWriters) -> (DeviceInput, EventInletNode) {
        let id = self.last.fetch_add(1, Ordering::Relaxed) + 1;
        let (node, writer) = EventInletNode::pair_with(self.capacity, &writers.inlet);
        let input = DeviceInput {
            id: InputId(NonZeroU64::new(id).unwrap_or(NonZeroU64::MAX)),
            counts: writer.counts(),
            writer,
            clock: self.clock.reader(),
            lead: Arc::clone(&self.lead),
            seen: None,
            map: None,
            timing: writers.timing,
            resyncs: writers.resyncs,
            lost: writers.lost,
        };
        (input, node)
    }
}

impl tutti_rt::Reclaim for InputInlets {
    /// Frees the clocks the devices' threads have let go of; control side, from
    /// the owner's tick (a session's `Reclaim` phase). Returns how many are
    /// still held.
    fn reclaim(&self) -> usize {
        self.clock.reclaim()
    }
}

/// The writing ends of an input's cells: [`DeviceWriters`], which the device's
/// end ([`DeviceInput`]) owns, and the writer of the MIDI-CI refusals, which the
/// device's stream owns. One set writes at a time; dropping the input closes its
/// cells until [`reopen`](Self::reopen) hands the writers on. [`split`](Self::split)
/// gives each owner its own, so none is left unclaimed.
#[derive(Debug)]
#[must_use = "the writers are how an input reports its timing and its losses"]
pub struct InputWriters {
    device: DeviceWriters,
    ci_refused: CounterWriter<Losses>,
}

/// The cells a [`DeviceInput`] writes: the latest correction its clock map made,
/// the count of its re-anchors, and what its inlet lost (pushes the ring
/// refused, and the totals of events evicted from the waiting list and events
/// the port refused).
#[derive(Debug)]
#[must_use = "the writers are how an input reports its timing and its losses"]
pub struct DeviceWriters {
    timing: GaugeWriter<Seconds>,
    resyncs: CounterWriter<Resyncs>,
    lost: CounterWriter<Losses>,
    inlet: Tally<Losses>,
}

/// The reading ends of an input's cells ([`InputWriters`]): made once with the
/// writers, kept by whoever outlives the connections.
#[derive(Clone, Debug)]
pub struct InputReaders {
    /// The latest correction the clock map made, in seconds.
    pub timing: Gauge<Seconds>,
    /// The clock map's re-anchors.
    pub resyncs: Counter<Resyncs>,
    /// The events the inlet had no room for.
    pub lost: Counter<Losses>,
    /// The events the inlets dropped from their waiting lists for room, and
    /// those their ports refused: totals the successive inlets share.
    pub inlet: Tally<Losses>,
    /// The MIDI-CI messages the device's output refused.
    pub ci_refused: Counter<Losses>,
}

impl InputWriters {
    /// New cells and their writers, with their readers: a gauge reading 0 s
    /// and counters at zero. Allocates: control side.
    pub fn new() -> (Self, InputReaders) {
        let (timing, timing_reader) = gauge(Seconds::new(0.0));
        let (resyncs, resyncs_reader) = counter();
        let (lost, lost_reader) = counter();
        let (ci_refused, ci_refused_reader) = counter();
        let inlet = tally();
        (
            Self {
                device: DeviceWriters {
                    timing,
                    resyncs,
                    lost,
                    inlet: inlet.clone(),
                },
                ci_refused,
            },
            InputReaders {
                timing: timing_reader,
                resyncs: resyncs_reader,
                lost: lost_reader,
                inlet,
                ci_refused: ci_refused_reader,
            },
        )
    }

    /// The writers of cells whose last writers have dropped; `None` while a
    /// connection still holds them. The counts carry on from where they were
    /// and the gauge holds its last reading (it is not reset: it reads the
    /// latest correction made, until the next map makes one). Control side.
    pub fn reopen(readers: &InputReaders) -> Option<Self> {
        // If only some reopen, their writers drop here and close them again.
        Some(Self {
            device: DeviceWriters {
                timing: readers.timing.reopen()?,
                resyncs: readers.resyncs.reopen()?,
                lost: readers.lost.reopen()?,
                inlet: readers.inlet.clone(),
            },
            ci_refused: readers.ci_refused.reopen()?,
        })
    }

    /// The device's writers, and the writer of the MIDI-CI refusals: the
    /// conversation is the device stream's, not the inlet's.
    pub fn split(self) -> (DeviceWriters, CounterWriter<Losses>) {
        (self.device, self.ci_refused)
    }
}

impl DeviceWriters {
    /// Writers whose readers nobody keeps: an input nobody reads the cells of.
    /// Allocates: control side.
    pub fn unread() -> Self {
        InputWriters::new().0.device
    }
}

/// One device's end of its inlet: stamp each arrival with a frame and push it.
/// Owned by the device's thread: `Send`, not `Clone`, and
/// [`push`](Self::push) takes `&mut self`. Nothing here blocks, locks or
/// allocates.
pub struct DeviceInput {
    id: InputId,
    writer: EventInletWriter,
    counts: InletCounts,
    clock: Reader<Option<ArrivalClock>>,
    lead: Arc<Param64<Seconds>>,
    /// The latest reading the map was fed.
    seen: Option<(Instant, Moment)>,
    map: Option<ClockMap>,
    /// The latest correction the map made.
    timing: GaugeWriter<Seconds>,
    /// The map's re-anchors.
    resyncs: CounterWriter<Resyncs>,
    /// The pushes the ring refused.
    lost: CounterWriter<Losses>,
}

impl std::fmt::Debug for DeviceInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInput")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl DeviceInput {
    /// The input's id.
    #[must_use]
    pub fn id(&self) -> InputId {
        self.id
    }

    /// The inlet's counts, readable from any thread ([`InletCounts::losses`]):
    /// events the ring had no room for (`lost`), the waiting list had no room
    /// for (`evicted`), the port refused (`refused`), and events that came
    /// late.
    #[must_use]
    pub fn counts(&self) -> InletCounts {
        self.counts.clone()
    }

    /// Pushes `ump`, which the OS delivered at `arrival`, for the frame the
    /// block clock puts that time on, plus the lead; or for the next block
    /// while no reading exists (no engine yet, or `arrival` before frame 0).
    /// Lock-free and allocation-free, so safe to call from a driver callback.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when the ring was full: the event is dropped and
    /// counted in the input's `lost` cell ([`InputReaders::lost`], cumulative
    /// across connections; the ring's own count, [`counts`](Self::counts), is
    /// this connection's). [`PushError::Disconnected`] once the inlet's node is
    /// gone: not counted, the event had nowhere to go.
    #[inline]
    pub fn push(&mut self, ump: impl Into<Ump>, arrival: Instant) -> Result<(), PushError> {
        let ump = ump.into();
        let sent = match self.moment_of(arrival) {
            Some(at) => self.writer.push(at, ump),
            None => self.writer.push(Stamp::NextBlock, ump),
        };
        if sent == Err(PushError::Full) {
            self.lost.add(Losses::lost(1));
        }
        sent
    }

    /// The moment `arrival` plus the lead is, by the latest block reading.
    fn moment_of(&mut self, arrival: Instant) -> Option<Moment> {
        let reading = {
            let clock = self.clock.lend();
            (clock.as_ref()?)()?
        };
        if self.seen != Some(reading) {
            self.seen = Some(reading);
            self.follow(reading);
        }
        let lead = Duration::try_from_secs_f64(self.lead.load().get()).unwrap_or(Duration::ZERO);
        let at = arrival.checked_add(lead).unwrap_or(arrival);
        self.map.as_ref()?.moment_at(at)
    }

    /// Feeds the map a reading: it follows the engine's clock, or is built
    /// again where the clock was restarted (a reading the map rejects, a
    /// frame that went back). The correction the map made
    /// is the timing reading. A re-anchor on a far reading and a rebuild after
    /// a rejection each count as one resync; the first map does not. A change
    /// of rate rescales the map to the same instant (it is not a rebuild), so
    /// it counts only if the reading after it is itself far off.
    fn follow(&mut self, (os, at): (Instant, Moment)) {
        let reading = ClockReading::new(os, at.frame());
        let Some(map) = self.map.as_mut() else {
            self.map = Some(ClockMap::new(at.rate(), reading));
            return;
        };
        if map.rate() != at.rate() {
            map.set_rate(at.rate());
        }
        match map.observe(reading) {
            Observed::Followed { correction } => self.timing.set(correction),
            Observed::Resynced { correction } => {
                self.timing.set(correction);
                self.resyncs.add(Resyncs::new(1));
            }
            Observed::Rejected => {
                self.map = Some(ClockMap::new(at.rate(), reading));
                self.resyncs.add(Resyncs::new(1));
            }
        }
    }
}
