//! [`EventWriter`]: the preallocated sink a node writes an event output port
//! through, its capacity ([`EventDensity`], [`note_off_room`]) and its count.

use std::cell::Cell;
use std::num::NonZeroU32;

use crate::event::types::Event;
use crate::node::param_event::ParamEvent;

/// Why [`EventWriter::push`] refused an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventRejected {
    /// The port's preallocated buffer is full: at its declared capacity for
    /// any event, or at [`note_off_room`] past it for a note-off. The event is
    /// dropped; the executor counts it (see
    /// [`Executor::dropped_event_count`](crate::Executor::dropped_event_count)).
    Full,
    /// `offset` is at or past the end of the block.
    OutOfBlock,
    /// `offset` is before the previous event's. A port's events are delivered
    /// sorted, and sorting on the audio thread would mean either an allocating
    /// stable sort or an unstable one that reorders equal offsets — so the
    /// writer requires order instead of restoring it.
    OutOfOrder,
}

/// How many events a port of declared capacity `cap` can hold when every
/// one past `cap` is a note-off: `cap` plus a reserve of an eighth and 8.
///
/// What every buffer downstream of the port is priced at (the compiler's
/// [`EventSlotCapacity`](crate::EventSlotCapacity), a delay FIFO's rate), so a
/// note-off the port took always fits the slot, the merge and the FIFO behind
/// it.
#[must_use]
pub const fn note_off_room(cap: usize) -> usize {
    cap.saturating_add(cap / 8 + 8)
}

/// The fewest events per block [`EventDensity::capacity`] sizes a port for: a
/// burst at one instant (a chord's note-ons, a controller sweep) is not
/// spread over the block, so a short block still needs room for it.
pub const MIN_EVENT_PORT_CAPACITY: NonZeroU32 = NonZeroU32::new(256).unwrap();

/// The most events per block [`EventDensity::capacity`] sizes a port for: one
/// per frame of the engine's default largest block
/// (`tutti_engine::DEFAULT_BLOCK_CAPACITY`, 8192). A longer block does not get a
/// larger port: a port is 32 bytes an event, 295 KB at the cap with its
/// note-off room, and a PDC delay on it holds up to `3.75 ×` that again
/// (about 2.5 MB: see `EventSlotCapacity::fifo`). Past the cap a port still
/// refuses and counts, it does not grow.
pub const MAX_EVENT_PORT_CAPACITY: NonZeroU32 = NonZeroU32::new(8_192).unwrap();

/// How many events a source writes per frame at the most, for sizing its
/// event port: a port of a block of `n` frames then holds `n × density`,
/// between [`MIN_EVENT_PORT_CAPACITY`] and [`MAX_EVENT_PORT_CAPACITY`].
///
/// A port's capacity is a count per block, so a source whose rate is per
/// second (a clip, a controller) loses events once the block is longer than
/// the capacity was sized for. This is the other half of the declaration:
/// the node reads its [`Prepare`](crate::Prepare) in
/// [`Node::prepare`](crate::Node::prepare), asks for a capacity, and returns
/// it from `shape`, which the editor reads after every prepare and every
/// re-prepare. The allocation is the compiler's and the executor's, on the
/// control side.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_graph::{EventDensity, Prepare};
/// use tutti_units::{SampleRate, Samples};
///
/// let one_per_frame = EventDensity::per_frame(1);
/// let at = |block| one_per_frame.capacity(&Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(block))));
/// assert_eq!(at(64).get(), 256); // the floor
/// assert_eq!(at(1024).get(), 1024);
/// assert_eq!(at(1 << 20).get(), 8_192); // the cap
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventDensity {
    per_kiloframe: u32,
}

impl EventDensity {
    /// `n` events in every frame.
    #[must_use]
    pub const fn per_frame(n: u32) -> Self {
        Self {
            per_kiloframe: n.saturating_mul(1024),
        }
    }

    /// `n` events in every 1024 frames: a density under one per frame.
    #[must_use]
    pub const fn per_kiloframe(n: u32) -> Self {
        Self { per_kiloframe: n }
    }

    /// The declared capacity for a port of a block of `prepare`'s
    /// [`max_block`](crate::Prepare::max_block).
    #[must_use]
    pub fn capacity(self, prepare: &crate::Prepare) -> NonZeroU32 {
        let frames = prepare.max_block().get() as u64;
        let events = (frames * u64::from(self.per_kiloframe)).div_ceil(1024);
        let events: u32 = events
            .clamp(
                u64::from(MIN_EVENT_PORT_CAPACITY.get()),
                u64::from(MAX_EVENT_PORT_CAPACITY.get()),
            )
            .try_into()
            .unwrap_or(MAX_EVENT_PORT_CAPACITY.get());
        // The floor is `MIN_EVENT_PORT_CAPACITY`, so this is never zero.
        NonZeroU32::new(events).unwrap_or(MIN_EVENT_PORT_CAPACITY)
    }
}

/// What one writer counted this block: events refused or skipped, and
/// refused events a node later wrote (`EventWriter::recovered`).
#[derive(Debug, Default)]
pub(crate) struct PortCount {
    pub(crate) refused: Cell<u32>,
    pub(crate) recovered: Cell<u32>,
}

/// The preallocated sink a node writes one event output port through.
///
/// Never allocates: it refuses past its capacity rather than growing, and the
/// capacity was reserved on the control thread.
///
/// # Note-offs have priority
///
/// A port at its declared capacity refuses any new event but a note-off
/// (`Event::is_note_off`), which is taken until the port holds
/// [`note_off_room`] events. The reserve costs a node that keeps to its
/// declaration nothing (it is room beyond what the node declared); it
/// is what lets a node that refused a note-on still end the notes it did
/// start. Order is not touched: nothing already written is moved or evicted.
///
/// The writer does not pair: a note-on it refused leaves its note-off to
/// whoever wrote it. A node that tracks the notes it starts (the clip node)
/// keeps the pairing itself and reports the note-off it then skips with
/// [`count_skipped`](Self::count_skipped); a relay that does not track (hardware input,
/// the queue) passes the note-off on, which a receiver treats as a no-op for
/// a key that is not sounding.
pub struct EventWriter<'a> {
    // `None` only for a detached writer, which the executor uses to fill the
    // unused tail of its fixed-size port table and never hands to a node.
    buf: Option<&'a mut Vec<Event>>,
    // The port's param events: the preallocated buffer beside `buf`, empty
    // of room when no node upstream of it writes any
    // ([`Shape::param_events`](crate::Shape::param_events)).
    params: Option<&'a mut Vec<ParamEvent>>,
    cap: usize,
    // `note_off_room(cap)`, computed once.
    room: usize,
    frames: u32,
    // Counted into a cell rather than a field: the executor reads it after
    // the node returns, while the writer itself is still borrowed by the `Io`
    // the node consumed. One per port, so the loss is reported per port.
    dropped: Option<&'a PortCount>,
}

impl std::fmt::Debug for EventWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventWriter")
            .field("cap", &self.cap)
            .field("frames", &self.frames)
            .field("len", &self.buf.as_ref().map(|b| b.len()))
            .finish_non_exhaustive()
    }
}

impl<'a> EventWriter<'a> {
    /// A writer over `buf`, which the caller has cleared. At most `cap`
    /// events are accepted (a note-off, [`note_off_room`] of them), and only
    /// at offsets `< frames`.
    pub(crate) fn new(
        buf: &'a mut Vec<Event>,
        params: &'a mut Vec<ParamEvent>,
        cap: usize,
        frames: u32,
        dropped: &'a PortCount,
    ) -> Self {
        debug_assert!(buf.is_empty() && params.is_empty());
        Self {
            buf: Some(buf),
            params: Some(params),
            cap,
            room: note_off_room(cap),
            frames,
            dropped: Some(dropped),
        }
    }

    /// A writer that accepts nothing: a placeholder for an unused port slot.
    pub(crate) const fn detached() -> Self {
        Self {
            buf: None,
            params: None,
            cap: 0,
            room: 0,
            frames: 0,
            dropped: None,
        }
    }

    fn reject(&self, why: EventRejected) -> Result<(), EventRejected> {
        self.count_lost();
        Err(why)
    }

    fn count_lost(&self) {
        if let Some(d) = self.dropped {
            d.refused.set(d.refused.get() + 1);
        }
    }

    /// Appends `event`. Offsets must be non-decreasing and inside the block.
    /// Audio thread: never allocates.
    ///
    /// # Errors
    ///
    /// [`EventRejected::OutOfBlock`], [`EventRejected::OutOfOrder`], or
    /// [`EventRejected::Full`] when the port's declared capacity is reached
    /// (a note-off is refused only at [`note_off_room`]).
    /// A refused event is dropped and counted in
    /// [`Executor::dropped_event_count`](crate::Executor::dropped_event_count); the
    /// events already written stand.
    pub fn push(&mut self, event: Event) -> Result<(), EventRejected> {
        if event.offset.get() >= self.frames {
            return self.reject(EventRejected::OutOfBlock);
        }
        let len = self.len();
        // The only test on the way in when the port is not full.
        if len >= self.cap && (len >= self.room || !event.is_note_off()) {
            return self.reject(EventRejected::Full);
        }
        let Some(buf) = self.buf.as_deref_mut() else {
            return self.reject(EventRejected::Full);
        };
        if buf.last().is_some_and(|e| e.offset > event.offset) {
            return self.reject(EventRejected::OutOfOrder);
        }
        buf.push(event);
        Ok(())
    }

    /// Appends the param event `event`. Offsets must be non-decreasing and
    /// inside the block, among the param events: they are sorted apart from
    /// [`push`](Self::push)'s events. Audio thread: never allocates.
    ///
    /// A port holds at most its declared capacity of param events, and none
    /// unless the node declares it writes them
    /// ([`Shape::param_events`](crate::Shape::param_events)).
    ///
    /// # Errors
    ///
    /// [`EventRejected::OutOfBlock`], [`EventRejected::OutOfOrder`], or
    /// [`EventRejected::Full`] at the port's capacity. A refused event is
    /// dropped and counted as an event is.
    pub fn push_param(&mut self, event: ParamEvent) -> Result<(), EventRejected> {
        if event.offset.get() >= self.frames {
            return self.reject(EventRejected::OutOfBlock);
        }
        let cap = self.cap;
        let Some(buf) = self.params.as_deref_mut() else {
            return self.reject(EventRejected::Full);
        };
        if buf.len() >= cap.min(buf.capacity()) {
            return self.reject(EventRejected::Full);
        }
        if buf.last().is_some_and(|e| e.offset > event.offset) {
            return self.reject(EventRejected::OutOfOrder);
        }
        buf.push(event);
        Ok(())
    }

    /// Counts one event the node skipped rather than wrote: it was not
    /// refused by the port, but the node left it out because the event it
    /// belongs to was (a note-off whose note-on the port refused). Without
    /// it the port's count is short by the notes a refusal then unpaired.
    pub fn count_skipped(&self) {
        self.count_lost();
    }

    /// Says that an event the port refused earlier (counted when it was) has
    /// now been written: a note-off a node kept owed and sent in a later
    /// block. The refusal stays in the dropped count; the executor's lost
    /// count (`Executor::lost_event_count`) takes it back.
    pub fn recovered(&self) {
        if let Some(d) = self.dropped {
            d.recovered.set(d.recovered.get() + 1);
        }
    }

    /// How many events this port has accepted so far this block.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.as_ref().map_or(0, |b| b.len())
    }

    /// Whether nothing has been written yet this block.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
