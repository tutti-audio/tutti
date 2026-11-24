//! Event taps: copy-out points for one event port of the compiled plan.
//!
//! The event sibling of [`tap`](crate::TapReader). An event tap is declared on
//! the [`Topology`](crate::Topology) as "copy this event port"
//! ([`EventTapSpec`](crate::EventTapSpec)), an output (what the port wrote) or
//! an input ([`EventTapPort`](crate::EventTapPort)). A tap of an input
//! copies what its node reads there: the sources merged in the order the node
//! is handed them, after the delays that align them. The compiler emits one
//! [`Op::EventTap`](crate::Op::EventTap) per tap whose port exists, and, for
//! an aligned tap, the same delay an event edge into an output-time sink would
//! get ([`EventDelayKey::EventTap`](crate::EventDelayKey::EventTap)). A tap that is not
//! in the topology compiles to nothing. Opening one refuses a port that does
//! not exist then ([`MissingSource::EventPort`](crate::MissingSource)); a port
//! that goes missing later (a node replaced by one with fewer event outputs, a
//! removed node) is read as no events: the commit succeeds, the reader is not
//! told, and nothing counts as lost.
//!
//! What crosses is [`EventKind::Midi`](crate::EventKind) only, each as a
//! [`TapEvent`]: the packet and the absolute [`Frame`] it was due on (the
//! block's frame plus its offset, after any alignment delay). Ramps, segments,
//! harmony and resync are the graph's own control, and are neither copied nor
//! counted.
//!
//! **The frames are on the clock of the rate the reader was opened at**
//! ([`EventTapReader::rate`]). A re-prepare at another rate moves the graph's
//! clock to the same wall time on a new scale; the reader of a tap opened
//! before it keeps the old figure, so whoever re-prepares reopens the tap
//! (`tutti-session`'s `Graph::reopen_event_taps`), and one reader's frames are
//! always on one scale.
//!
//! The ring is `tutti_rt::ring`, the one single-producer, single-consumer
//! queue, as the editor-to-executor queue is. The producer is owned by the
//! executor (it rides a commit in, as an audio tap's does), the consumer by the
//! caller, so the audio thread takes no lock and frees nothing: a producer left
//! behind by a plan that no longer names it is retired with the old state, on
//! the control side.
//!
//! Dropping the [`EventTapReader`] first is safe. The tap then finds no reader
//! and writes nothing, and the ring is freed with the writer when the tap is
//! closed ([`Editor::close_event_tap`](crate::Editor::close_event_tap)) or
//! pruned ([`Editor::prune_event_taps`](crate::Editor::prune_event_taps)), on
//! the control side. Dropping the reader last frees the ring on the reader's
//! thread.

use std::cell::Cell;
use std::sync::Weak;

use tutti_rt::{
    ControlDrop, Counter, CounterWriter, Overflow, RingReader, RingWriter, counter, ring,
};
use tutti_ump::Ump;
use tutti_units::{Frame, SampleRate};

use super::alive::Alive;
use crate::compile::ids::EventSlotId;
use crate::event::types::EventKind;
use crate::event::writer::note_off_room;
use crate::exec::slots::EventSlots;
use crate::node::time::Env;
use crate::{EventCapacity, EventTapId, TapAlign};

/// How an event tap is opened: its ring capacity and where its copy sits in
/// time. Taken by [`Editor::open_event_tap`](crate::Editor::open_event_tap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventTapOptions {
    capacity: EventCapacity,
    align: TapAlign,
}

impl EventTapOptions {
    /// Options for an event tap whose ring holds `capacity` events: aligned
    /// to the output ([`TapAlign::ToOutput`]).
    #[must_use]
    pub fn new(capacity: EventCapacity) -> Self {
        Self {
            capacity,
            align: TapAlign::default(),
        }
    }

    /// Where the copy sits in time.
    #[must_use]
    pub fn align(mut self, align: TapAlign) -> Self {
        self.align = align;
        self
    }

    pub(crate) fn into_parts(self) -> (EventCapacity, TapAlign) {
        (self.capacity, self.align)
    }
}

/// One event a tap copied out: the packet, and the frame of the graph's clock
/// it was due on.
///
/// The frame is `Env::frame_at(offset)` of the block the tap's op ran in, so
/// it keeps the spacing between events whatever the block sizes were, orders
/// events across taps and against the transport's `At::Frame`, and is a
/// `u64`: it does not wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapEvent {
    /// The frame it was due on, on the clock of [`EventTapReader::rate`].
    pub frame: Frame,
    /// The MIDI packet.
    pub ump: Ump,
}

// An event tap's element is what the ring holds by value: a frame and a
// packet, 24 bytes. Asserted here rather than discovered in a ring's size.
const _: () = assert!(std::mem::size_of::<TapEvent>() == 24);

/// The executor's end of an event tap: the ring producer.
///
/// One thread writes and one reads, which is why the occupancy count below is
/// safe: a ring's vacancy only grows under the reader, so the occupancy the
/// writer takes can only be an overestimate. An event may be refused that the
/// reader had just made room for, and none is taken past the ring.
pub struct EventTapWriter {
    _control: ControlDrop<Self>,
    ring: RingWriter<TapEvent>,
    /// The events held before anything but a note-off is refused.
    capacity: usize,
    /// Events refused, by the capacity rule or by a full ring, shared with the
    /// reader: one count for both, so the ring itself (`Refuse`) counts nothing.
    lost: CounterWriter<u64>,
}

impl std::fmt::Debug for EventTapWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventTapWriter")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

/// [`Op::EventTap`](crate::Op::EventTap): every MIDI event of event slot `src`
/// (an output port's, or the merge a node reads at an input)
/// into the tap's ring, stamped with its frame in the block `env` describes.
///
/// A non-note-off is refused once `capacity` events are held, a note-off once
/// the ring (`note_off_room(capacity)`) is full, and each refusal is counted
/// once. With no reader there is nothing to write or count. Allocates nothing.
#[inline]
pub(crate) fn event_tap_op<E: EventSlots + ?Sized>(
    tap: &mut EventTapWriter,
    events: &E,
    src: EventSlotId,
    env: &Env,
) {
    if tap.ring.is_disconnected() {
        return;
    }
    let total = tap.ring.capacity();
    let mut lost = 0u64;
    for event in events.ev(src) {
        let EventKind::Midi(ump) = event.kind else {
            continue;
        };
        let held = total - tap.ring.vacant();
        if held >= tap.capacity && (held >= total || !ump.is_note_off()) {
            lost += 1;
            continue;
        }
        let pushed = tap.ring.push(TapEvent {
            frame: env.frame_at(event.offset),
            ump,
        });
        if pushed.is_err() {
            lost += 1;
        }
    }
    if lost > 0 {
        tap.lost.add(lost);
    }
}

/// The caller's end of an event tap: the MIDI events one port carried, each
/// with the frame it was due on.
///
/// One consumer: not `Clone`, and it reads through `&mut self`; a second
/// consumer opens a second tap.
///
/// ```compile_fail
/// fn is_clone<T: Clone>() {}
/// is_clone::<tutti_graph::EventTapReader>();
/// ```
///
/// Drain it from a thread of your own. Dropping it leaves the tap open until
/// the editor closes or prunes it; the audio thread then writes nothing for it.
/// It must not be dropped on the audio thread (debug builds panic there).
pub struct EventTapReader {
    _control: ControlDrop<Self>,
    id: EventTapId,
    ring: RingReader<TapEvent>,
    lost: Counter<u64>,
    /// What `lost` read at the last [`take_lost`](Self::take_lost).
    lost_seen: Cell<u64>,
    rate: SampleRate,
    /// The editor watches this: when it dies, the reader is gone.
    alive: Alive,
}

impl std::fmt::Debug for EventTapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventTapReader")
            .field("id", &self.id)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl EventTapReader {
    /// A reader and the writer the executor will fill it with: a ring that
    /// holds [`note_off_room`] of `capacity` events,
    /// of which a non-note-off may take `capacity`; its frames are on the
    /// clock of `rate`.
    pub(crate) fn pair(
        id: EventTapId,
        capacity: EventCapacity,
        rate: SampleRate,
    ) -> (Self, EventTapWriter) {
        let capacity = capacity.slots();
        let (lost, lost_reader) = counter::<u64>();
        let (prod, cons) = ring::<TapEvent>(note_off_room(capacity), Overflow::Refuse);
        let reader = Self {
            _control: ControlDrop::new(),
            id,
            ring: cons,
            lost: lost_reader,
            lost_seen: Cell::new(0),
            rate,
            alive: Alive::default(),
        };
        (
            reader,
            EventTapWriter {
                _control: ControlDrop::new(),
                ring: prod,
                capacity,
                lost,
            },
        )
    }

    /// Pops events into `out`, oldest first, as many as fit; returns the
    /// count. The rest of `out` is as it was.
    pub fn pop_into(&mut self, out: &mut [TapEvent]) -> usize {
        self.ring.pop_slice(out)
    }

    /// Pops what is readable now, oldest first, one event at a time: the
    /// events in the ring when this is called, and no more (an event the
    /// executor writes while the iterator is read is for the next call). Each
    /// one yielded is gone from the ring; one left unread (the iterator
    /// dropped early) stays. Allocation-free.
    ///
    /// It reads the ring and nothing else: what the executor could not push is
    /// still counted for [`take_lost`](Self::take_lost), whether or not the
    /// ring is drained.
    pub fn drain(&mut self) -> impl Iterator<Item = TapEvent> + '_ {
        self.ring.drain()
    }

    /// Events readable now.
    #[must_use]
    pub fn available(&self) -> usize {
        self.ring.len()
    }

    /// Events the executor could not push since the last call, and clears the
    /// count: nonzero is a gap in what was read.
    pub fn take_lost(&self) -> u64 {
        let mut seen = self.lost_seen.get();
        let lost = self.lost.take_since(&mut seen);
        self.lost_seen.set(seen);
        lost
    }

    /// The tap's id, the one
    /// [`Editor::close_event_tap`](crate::Editor::close_event_tap) takes.
    #[must_use]
    pub fn id(&self) -> EventTapId {
        self.id
    }

    /// The sample rate of the clock every popped frame is on: the graph's
    /// rate when the tap was opened. A re-prepare at another rate does not
    /// change it; the tap is reopened instead.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// A handle to see whether this reader is still alive: its strong count
    /// is zero once the reader is dropped.
    #[must_use]
    pub fn watch(&self) -> Weak<()> {
        self.alive.watch()
    }
}
