//! Events: what flows on an event port.
//!
//! Events are **graph ports**, not a side channel, so the one compiler pass
//! that aligns audio (PDC) aligns notes and automation too. An event input
//! port may have several sources, merged deterministically by `(offset,
//! source order)`, where source order is the source port's `(NodeKey, port)`
//! — layering a keyboard and a clip is the normal case, not an edge case.
//! Automation is carried as **ramp** events ([`ParamRamp`](crate::ParamRamp):
//! relative, from wherever the param stands) and as **curve segments**
//! ([`ParamSegment`](crate::ParamSegment): absolute, between their own
//! endpoints), both moving along a closed set of shapes
//! ([`Law`](tutti_curve::Law)) a param port evaluates per frame. They are
//! [`ParamEvent`]s, and travel in a buffer of their own beside the port's
//! events, merged and delayed with them, so a MIDI event stays 32 bytes.
//!
//! A MIDI event carries a [`tutti_ump::Ump`]. The graph reads one fact of it,
//! [`Event::is_note_off`], which the delay FIFO's overflow policy needs, and
//! that decode is `tutti-ump`'s.

use std::cell::Cell;
use std::num::NonZeroU32;

use tutti_ump::Ump;
use tutti_units::Samples;

use crate::param_event::ParamEvent;
use crate::time::Offset;

/// What an [`Event`] says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// A MIDI message, as UMP words.
    Midi(Ump),
    /// A chord or scale taking effect.
    Harmony(Harmony),
    /// A request to an **event source**: re-send where it stands, from this
    /// event's frame, as a resumed segment. What a sink that
    /// starts reading a source late (a control lane whose program newly
    /// reads a held note envelope) needs, since a source sends only when it
    /// moves. Delivered by [`Editor::schedule`](crate::Editor::schedule),
    /// which waits for the commit sent before it, so the re-send reaches the
    /// edges that commit placed. A node that sends nothing a sink could
    /// miss ignores it; so does every param port.
    Resync,
}

/// Whether a [`Harmony`] is a chord or a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HarmonyKind {
    /// The chord sounding from here: a root, a bass and the degrees.
    Chord,
    /// The scale (key) in force from here: a tonic and the degrees.
    Scale,
}

/// A harmonic context change — a chord or a scale taking effect, as a
/// sequencer's chord and scale lanes say — for a node that follows them (a
/// hosted VST3 plugin's chord and scale events).
///
/// Not MIDI: MIDI 2.0's Flex Data names a chord by type and a key by its
/// signature, which cannot carry an arbitrary degree set (a scale is any of
/// the 4096). Pitches are MIDI note numbers (`0..=127`); `degrees` is a 12-bit
/// mask relative to the root, bit `n` set when semitone `n` belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Harmony {
    kind: HarmonyKind,
    root: u8,
    bass: u8,
    degrees: u16,
}

impl Harmony {
    /// A chord on `root`, `bass` in the bass (equal to `root` in root
    /// position), sounding `degrees`. Pitches past 127 and mask bits past the
    /// twelfth are dropped.
    pub const fn chord(root: u8, bass: u8, degrees: u16) -> Self {
        Self {
            kind: HarmonyKind::Chord,
            root: root & 0x7f,
            bass: bass & 0x7f,
            degrees: degrees & 0xfff,
        }
    }

    /// A scale on the tonic `root` with `degrees`. As [`chord`](Self::chord);
    /// its bass is its root.
    pub const fn scale(root: u8, degrees: u16) -> Self {
        Self {
            kind: HarmonyKind::Scale,
            root: root & 0x7f,
            bass: root & 0x7f,
            degrees: degrees & 0xfff,
        }
    }

    /// Chord or scale.
    pub const fn kind(&self) -> HarmonyKind {
        self.kind
    }

    /// The root (a chord's) or tonic (a scale's), as a MIDI note number.
    pub const fn root(&self) -> u8 {
        self.root
    }

    /// The bass note, as a MIDI note number: a scale's is its root.
    pub const fn bass(&self) -> u8 {
        self.bass
    }

    /// The degrees, a 12-bit mask relative to the root.
    pub const fn degrees(&self) -> u16 {
        self.degrees
    }
}

/// A MIDI packet at its position in the current block: what a MIDI edge
/// (`MidiOut`, a plugin's input) carries.
///
/// An alias, not a type: both halves are already checked ([`Offset`] against
/// a block, [`Ump`] as the packet), so the pair adds no rule of its own, and a
/// site destructures it (`for &(at, ump) in events`). Inside the graph the same
/// pair is an [`Event`] with [`EventKind::Midi`]. A position not yet checked
/// against a block (a plugin's report, a ring crossing threads) is a raw `u32`,
/// not an `Offset`, and so not an `UmpAt`.
pub type UmpAt = (Offset, Ump);

/// One event on an event port: an offset into the current block, and what
/// happens there.
///
/// `offset` is an [`Offset`] — a position inside the block the event is
/// delivered in, never an absolute [`Frame`](tutti_units::Frame) (see
/// [`Offset`]). Slices of events handed to a node are
/// sorted by it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Where in the block.
    pub offset: Offset,
    /// The payload.
    pub kind: EventKind,
}

// An event is what every event buffer, merge and delay FIFO holds by value,
// preallocated on the control side and copied on the audio thread: it must
// stay `Copy` and small, whatever payload is added (a curve segment packs
// its address and shape tag for this).
// Four UMP words cover every MIDI 2.0 message, 128-bit ones included, so a
// MIDI payload never needs to grow it. Asserted here rather than discovered
// as a slower merge.
const _: () = {
    const fn copy<T: Copy>() {}
    copy::<Event>();
    assert!(std::mem::size_of::<Event>() <= 32);
};

impl Event {
    /// A MIDI event at `offset`.
    pub fn midi(offset: Offset, ump: impl Into<Ump>) -> Self {
        Self {
            offset,
            kind: EventKind::Midi(ump.into()),
        }
    }

    /// Whether this is a MIDI note-off: a MIDI 1.0 channel-voice note-off, a
    /// note-on at velocity 0 (the MIDI 1.0 spelling of the same thing), or a
    /// MIDI 2.0 channel-voice note-off. The one event the graph refuses to
    /// lose — see the delay FIFO's overflow policy.
    #[must_use]
    pub fn is_note_off(&self) -> bool {
        matches!(self.kind, EventKind::Midi(u) if u.is_note_off())
    }

    /// A [`Resync`](EventKind::Resync) request at `offset`.
    pub const fn resync(offset: Offset) -> Self {
        Self {
            offset,
            kind: EventKind::Resync,
        }
    }

    /// A chord or scale taking effect at `offset`.
    pub const fn harmony(offset: Offset, harmony: Harmony) -> Self {
        Self {
            offset,
            kind: EventKind::Harmony(harmony),
        }
    }
}

/// Why a slice is not a valid [`SortedEvents`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EventOrderError {
    /// The event at this index has a smaller offset than the one before it.
    #[error("event {at} is out of order: its offset is smaller than that of the event before it")]
    Unsorted {
        /// Index into the slice.
        at: usize,
    },
    /// The event at this index is at or past the end of the block.
    #[error("event {at} is at or past the end of the block")]
    OutOfBlock {
        /// Index into the slice.
        at: usize,
    },
}

/// A block's events for one port: **sorted by offset, every offset inside the
/// block** — by construction.
///
/// It holds the port's [`Event`]s (MIDI and the rest: what iterating it, its
/// `len` and `is_empty` and its deref to a slice are about) and, beside them,
/// its [`ParamEvent`]s ([`params`](Self::params)), each list sorted
/// separately. [`sub_blocks`](crate::Io::sub_blocks) cuts the block at both.
///
/// The only ways to get one are [`new`](Self::new), which checks,
/// [`sort`](Self::sort), which sorts (stably, so equal offsets keep their
/// order; control side — the sort may allocate), and the executor, whose event
/// slots are sorted and in-block by the rules of [`EventWriter`] and the
/// merge. A node reading a `SortedEvents` never re-checks either property.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SortedEvents<'a> {
    events: &'a [Event],
    params: &'a [ParamEvent],
}

impl<'a> SortedEvents<'a> {
    /// No events.
    pub const EMPTY: SortedEvents<'static> = SortedEvents {
        events: &[],
        params: &[],
    };

    /// `events`, if they are sorted by offset and every offset is below
    /// `frames`.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`] or [`EventOrderError::Unsorted`],
    /// naming the first offending index.
    pub fn new(events: &'a [Event], frames: usize) -> Result<Self, EventOrderError> {
        for (i, e) in events.iter().enumerate() {
            if e.offset.index() >= frames {
                return Err(EventOrderError::OutOfBlock { at: i });
            }
            if i > 0 && events[i - 1].offset > e.offset {
                return Err(EventOrderError::Unsorted { at: i });
            }
        }
        Ok(Self {
            events,
            params: &[],
        })
    }

    /// `events` with the param events `params` beside them, if each list is
    /// sorted by offset and every offset is below `frames`.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`] or [`EventOrderError::Unsorted`],
    /// naming the first offending index of the list at fault (the events
    /// first).
    pub fn with_both(
        events: &'a [Event],
        params: &'a [ParamEvent],
        frames: usize,
    ) -> Result<Self, EventOrderError> {
        let this = Self::new(events, frames)?;
        for (i, e) in params.iter().enumerate() {
            if e.offset.index() >= frames {
                return Err(EventOrderError::OutOfBlock { at: i });
            }
            if i > 0 && params[i - 1].offset > e.offset {
                return Err(EventOrderError::Unsorted { at: i });
            }
        }
        Ok(Self { params, ..this })
    }

    /// Sorts `events` in place by offset (stably), then views them. Fails only
    /// when an offset is outside the block. Control side: may allocate.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`], naming the first event at or past
    /// `frames`; `events` is left unsorted.
    pub fn sort(events: &'a mut [Event], frames: usize) -> Result<Self, EventOrderError> {
        if let Some(at) = events.iter().position(|e| e.offset.index() >= frames) {
            return Err(EventOrderError::OutOfBlock { at });
        }
        events.sort_by_key(|e| e.offset);
        Ok(Self {
            events,
            params: &[],
        })
    }

    /// Wrap slices the executor's own rules keep sorted and in-block.
    /// Checked in debug builds.
    pub(crate) fn trusted(events: &'a [Event], frames: usize) -> Self {
        debug_assert_eq!(Self::new(events, frames).map(|_| ()), Ok(()));
        Self {
            events,
            params: &[],
        }
    }

    /// These events with `params` beside them, which the executor's own rules
    /// keep sorted and in-block. Checked in debug builds.
    pub(crate) fn trusted_params(self, params: &'a [ParamEvent], frames: usize) -> Self {
        debug_assert_eq!(
            Self::with_both(self.events, params, frames).map(|_| ()),
            Ok(())
        );
        Self { params, ..self }
    }

    /// The events.
    #[must_use]
    pub fn as_slice(&self) -> &'a [Event] {
        self.events
    }

    /// The param events (ramps and segments) of the port, sorted by offset.
    #[must_use]
    pub fn params(&self) -> &'a [ParamEvent] {
        self.params
    }

    /// Number of events, the param events not counted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether there are no events. The param events are
    /// [`params`](Self::params), and not counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The events, in order.
    pub fn iter(&self) -> std::slice::Iter<'a, Event> {
        self.events.iter()
    }
}

/// A block split at its event offsets: the iterator
/// [`Io::sub_blocks`](crate::Io::sub_blocks) returns.
///
/// Yields `(range, events)` pairs that tile the block in order: `range` is a
/// run of frames (slice indices into the block's buffers), and `events` are
/// exactly the events, and the param events, at `range.start` — so a node that
/// applies `events`, then renders `range`, applies every event on its own
/// frame. A block with no events is one chunk; several events on one offset
/// arrive together, in their delivered order; an event on the last frame gets
/// a one-frame chunk.
///
/// Zero allocation: it walks the event slices it was built from.
#[derive(Clone, Debug)]
pub struct SubBlocks<'a> {
    events: &'a [Event],
    params: &'a [ParamEvent],
    pos: usize,
    frames: usize,
}

impl<'a> SortedEvents<'a> {
    /// Split a block of `frames` at these events' offsets. `frames` is the
    /// length these events were checked against.
    pub(crate) fn sub_blocks(self, frames: Samples) -> SubBlocks<'a> {
        SubBlocks {
            events: self.events,
            params: self.params,
            pos: 0,
            frames: frames.get(),
        }
    }
}

impl<'a> Iterator for SubBlocks<'a> {
    type Item = (std::ops::Range<usize>, SortedEvents<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.frames {
            return None;
        }
        // Every event before `pos` was yielded with an earlier chunk, so the
        // ones at `pos` are the prefix; `<=` rather than `==` only so a
        // malformed slice could never stall the walk.
        let here = self
            .events
            .iter()
            .take_while(|e| e.offset.index() <= self.pos)
            .count();
        let here_params = self
            .params
            .iter()
            .take_while(|e| e.offset.index() <= self.pos)
            .count();
        let (now, rest) = self.events.split_at(here);
        let (now_params, rest_params) = self.params.split_at(here_params);
        let next = [
            rest.first().map(|e| e.offset.index()),
            rest_params.first().map(|e| e.offset.index()),
        ]
        .into_iter()
        .flatten()
        .min();
        let end = next.unwrap_or(self.frames).min(self.frames);
        let range = self.pos..end;
        self.events = rest;
        self.params = rest_params;
        self.pos = end;
        Some((
            range,
            SortedEvents {
                events: now,
                params: now_params,
            },
        ))
    }
}

impl std::iter::FusedIterator for SubBlocks<'_> {}

impl<'a> IntoIterator for SortedEvents<'a> {
    type Item = &'a Event;
    type IntoIter = std::slice::Iter<'a, Event>;
    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

impl std::ops::Deref for SortedEvents<'_> {
    type Target = [Event];
    fn deref(&self) -> &[Event] {
        self.events
    }
}

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

/// Merge `sources` (each sorted by offset) into `out`, ordered by
/// `(offset, source index)`. At most `cap` events land; the rest are counted.
///
/// A k-way merge rather than concatenate-and-sort: the stable sort that would
/// give the same order allocates, and the unstable one does not give it.
///
/// When the sources hold more than `cap`, the note-offs keep their room: the
/// others are cut (the later ones first) so that every note-off lands, up to
/// `cap`. Order is that of the merge either way. A merge slot holds all its
/// sources unless the merge is a node's input under its declared bound
/// (`Shape::event_input_capacity`), which is the one merge that cuts; the
/// caller charges the returned count.
pub(crate) fn merge_into(sources: &[&[Event]], out: &mut Vec<Event>, cap: usize) -> u32 {
    let room = cap.saturating_sub(out.len());
    merge_walk(sources, room, |_, e| out.push(e))
}

/// Merge the param events of `sources` (each sorted by offset) into `out`,
/// ordered by `(offset, source index)`: the merge of [`merge_into`] with no
/// note-off to keep. At most `cap` land; the rest are counted, the later
/// ones first.
pub(crate) fn merge_params(
    sources: &[&[ParamEvent]],
    out: &mut Vec<ParamEvent>,
    cap: usize,
) -> u32 {
    let room = cap.saturating_sub(out.len());
    let mut heads = [0usize; 2 * crate::MAX_PORTS];
    let heads = &mut heads[..sources.len()];
    let (mut kept, mut dropped) = (0usize, 0u32);
    loop {
        let mut best: Option<(Offset, usize)> = None;
        for (i, src) in sources.iter().enumerate() {
            if let Some(e) = src.get(heads[i]) {
                // Strict `<`: on a tie the lower source index keeps it.
                if best.is_none_or(|(o, _)| e.offset < o) {
                    best = Some((e.offset, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let e = sources[i][heads[i]];
        heads[i] += 1;
        if kept < room {
            kept += 1;
            out.push(e);
        } else {
            dropped += 1;
        }
    }
    dropped
}

/// Walks `sources` (each sorted by offset) in `(offset, source index)` order
/// and calls `keep(source index, event)` for each of the first `room` that
/// land, with the note-off rule of [`merge_into`]. Returns how many it
/// dropped. At most [`crate::MAX_PORTS`] `* 2` sources.
pub(crate) fn merge_walk(
    sources: &[&[Event]],
    room: usize,
    mut keep: impl FnMut(usize, Event),
) -> u32 {
    let total: usize = sources.iter().map(|s| s.len()).sum();
    // How many events that are not note-offs may land: all of them while all
    // fit, else what the note-offs leave of the room.
    let mut others = usize::MAX;
    if total > room {
        let offs: usize = sources
            .iter()
            .map(|s| s.iter().filter(|e| e.is_note_off()).count())
            .sum();
        others = room.saturating_sub(offs);
    }
    let mut heads = [0usize; 2 * crate::MAX_PORTS];
    let heads = &mut heads[..sources.len()];
    let (mut kept, mut dropped) = (0usize, 0u32);
    loop {
        let mut best: Option<(Offset, usize)> = None;
        for (i, src) in sources.iter().enumerate() {
            if let Some(e) = src.get(heads[i]) {
                // Strict `<`: on a tie the lower source index keeps it.
                if best.is_none_or(|(o, _)| e.offset < o) {
                    best = Some((e.offset, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let e = sources[i][heads[i]];
        heads[i] += 1;
        let fits = kept < room;
        let lands = if e.is_note_off() {
            fits
        } else {
            fits && others > 0
        };
        if lands {
            if !e.is_note_off() {
                others = others.saturating_sub(1);
            }
            kept += 1;
            keep(i, e);
        } else {
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    use tutti_buffers::MaxBlock;
    use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Voice1};

    fn ev(offset: u32, tag: u32) -> Event {
        Event::midi(Offset::raw(offset), Ump([tag, 0, 0, 0]))
    }

    /// Ties go to the earlier source; otherwise offset order.
    #[test]
    fn merge_orders_by_offset_then_source() {
        let a = [ev(0, 1), ev(3, 2), ev(9, 3)];
        let b = [ev(3, 10), ev(4, 11)];
        let mut out = Vec::with_capacity(8);
        let dropped = merge_into(&[&a, &b], &mut out, 8);
        assert_eq!(dropped, 0);
        let tags: Vec<u32> = out
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => w[0],
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(tags, vec![1, 2, 10, 11, 3]);
    }

    /// The merge refuses past capacity and says how many it refused.
    #[test]
    fn merge_counts_what_did_not_fit() {
        let a = [ev(0, 1), ev(1, 2), ev(2, 3)];
        let b = [ev(0, 4), ev(5, 5)];
        let mut out = Vec::with_capacity(8);
        assert_eq!(merge_into(&[&a, &b], &mut out, 3), 2);
        assert_eq!(out.len(), 3);
    }

    /// `SortedEvents::new` refuses what is unsorted or out of the block;
    /// `sort` repairs order (stably) and still refuses out-of-block.
    #[test]
    fn sorted_events_are_sorted_and_in_block() {
        let unsorted = [ev(3, 0), ev(1, 1)];
        assert_eq!(
            SortedEvents::new(&unsorted, 8),
            Err(EventOrderError::Unsorted { at: 1 })
        );
        let late = [ev(1, 0), ev(8, 1)];
        assert_eq!(
            SortedEvents::new(&late, 8),
            Err(EventOrderError::OutOfBlock { at: 1 })
        );
        assert!(SortedEvents::new(&[ev(0, 0), ev(0, 1), ev(7, 2)], 8).is_ok());

        let mut v: Vec<Event> = (0..64).map(|i| ev(i % 3, i)).collect();
        let sorted = SortedEvents::sort(&mut v, 8).expect("in block");
        let tags: Vec<u32> = sorted
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => w[0],
                _ => unreachable!(),
            })
            .collect();
        let mut want: Vec<u32> = (0..64).collect();
        want.sort_by_key(|i| i % 3);
        assert_eq!(tags, want, "stable: equal offsets keep their order");
        let mut late = vec![ev(9, 0)];
        assert!(SortedEvents::sort(&mut late, 8).is_err());
    }

    /// The writer enforces the three invariants a consumer relies on.
    #[test]
    fn writer_rejects_out_of_order_out_of_block_and_overflow() {
        let mut buf = Vec::with_capacity(4);
        let dropped = PortCount::default();
        let mut params = Vec::new();
        let mut w = EventWriter::new(&mut buf, &mut params, 2, 16, &dropped);
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(4, 0)), Err(EventRejected::OutOfOrder));
        assert_eq!(w.push(ev(16, 0)), Err(EventRejected::OutOfBlock));
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(6, 0)), Err(EventRejected::Full));
        assert_eq!(w.len(), 2);
        assert_eq!(dropped.refused.get(), 3);
    }

    /// The splitter under test, written the obvious way: every distinct
    /// offset (and 0) starts a chunk, each chunk runs to the next start, and
    /// its events are those whose offset equals its start.
    fn hand_split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
        let mut starts: Vec<usize> = std::iter::once(0)
            .chain(events.iter().map(|e| e.offset.index()))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let ends = starts
            .iter()
            .skip(1)
            .copied()
            .chain(std::iter::once(frames));
        starts
            .iter()
            .zip(ends)
            .map(|(&a, b)| {
                let here = events
                    .iter()
                    .filter(|e| e.offset.index() == a)
                    .copied()
                    .collect();
                (a..b, here)
            })
            .collect()
    }

    fn split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
        SortedEvents::new(events, frames)
            .expect("sorted and in the block")
            .sub_blocks(Samples::new(frames))
            .map(|(r, e)| (r, e.to_vec()))
            .collect()
    }

    /// The edges `sub_blocks` has to get right, spelled out: an event at
    /// offset 0 (no empty leading chunk), several on one offset (one chunk,
    /// all of them, in order), one on the last frame (a one-frame chunk), and
    /// no events at all (the whole block).
    #[test]
    fn sub_blocks_split_at_every_offset() {
        let evs = [ev(0, 1), ev(5, 2), ev(5, 3), ev(7, 4)];
        let got = split(&evs, 8);
        assert_eq!(
            got,
            vec![
                (0..5, vec![ev(0, 1)]),
                (5..7, vec![ev(5, 2), ev(5, 3)]),
                (7..8, vec![ev(7, 4)]),
            ]
        );
        assert_eq!(split(&[], 8), vec![(0..8, vec![])]);
        assert_eq!(
            split(&[ev(3, 9)], 8),
            vec![(0..3, vec![]), (3..8, vec![ev(3, 9)])]
        );
        assert_eq!(split(&evs, 8), hand_split(&evs, 8));
    }

    proptest::proptest! {
        /// Against the hand-rolled splitter, on random sorted offsets — with
        /// offset 0, the last frame and repeats all drawn often — and random
        /// block lengths including 1.
        #[test]
        fn sub_blocks_match_a_hand_rolled_splitter(
            frames in 1usize..40,
            raw in proptest::collection::vec(0u32..6, 0..24),
        ) {
            // Bias toward the edges: 0 → offset 0, 5 → the last frame,
            // anything else → a frame in between (repeats are frequent).
            let last = frames as u32 - 1;
            let mut offsets: Vec<u32> = raw
                .iter()
                .enumerate()
                .map(|(i, &r)| match r {
                    0 => 0,
                    5 => last,
                    _ => (r * 7 + i as u32) % frames as u32,
                })
                .collect();
            offsets.sort_unstable();
            let evs: Vec<Event> = offsets
                .iter()
                .enumerate()
                .map(|(i, &o)| ev(o, i as u32))
                .collect();
            let got = split(&evs, frames);
            proptest::prop_assert_eq!(&got, &hand_split(&evs, frames));
            // The chunks tile the block and every event comes out once.
            let covered: usize = got.iter().map(|(r, _)| r.len()).sum();
            proptest::prop_assert_eq!(covered, frames);
            let n: usize = got.iter().map(|(_, e)| e.len()).sum();
            proptest::prop_assert_eq!(n, evs.len());
        }
    }

    /// An event is a note-off when its packet is (`Ump::is_note_off` pins the
    /// spellings); a note-on and a resync are not.
    #[test]
    fn only_a_midi_note_off_event_is_a_note_off() {
        let (c, n, v) = (MidiChannel::FIRST, NoteNumber::of(60), U7::of(0x40));
        assert!(Event::midi(Offset::ZERO, Message::midi1(c, Voice1::note_off(n, v))).is_note_off());
        assert!(!Event::midi(Offset::ZERO, Message::midi1(c, Voice1::note_on(n, v))).is_note_off());
        assert!(!Event::resync(Offset::ZERO).is_note_off());
    }

    fn off(offset: u32) -> Event {
        Event::midi(
            Offset::raw(offset),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::note_off(NoteNumber::of(60), U7::of(0)),
            ),
        )
    }

    /// A merge that must cut keeps the note-offs and cuts the others, later
    /// ones first, and keeps the merge order. Sources `a = [on@0, on@1,
    /// off@2]` and `b = [on@0, off@3]` into room for 3: both note-offs land
    /// (2), leaving one other, which is the first in merge order (`a`'s
    /// note-on at 0). The three others cut are counted: 2.
    #[test]
    fn a_merge_that_must_cut_keeps_its_note_offs() {
        let a = [ev(0, 1), ev(1, 2), off(2)];
        let b = [ev(0, 3), off(3)];
        let mut out = Vec::with_capacity(3);
        let dropped = merge_into(&[&a, &b], &mut out, 3);
        assert_eq!(dropped, 2);
        assert_eq!(out, vec![ev(0, 1), off(2), off(3)]);
    }

    /// A merge that fits is the plain merge, note-offs or not.
    #[test]
    fn a_merge_that_fits_cuts_nothing() {
        let a = [ev(0, 1), off(2)];
        let b = [off(1), ev(3, 3)];
        let mut out = Vec::with_capacity(4);
        assert_eq!(merge_into(&[&a, &b], &mut out, 4), 0);
        assert_eq!(out, vec![ev(0, 1), off(1), off(2), ev(3, 3)]);
    }

    /// A writer at its capacity refuses a note-on and takes a note-off, up
    /// to `note_off_room`, and every refusal and every skipped event is
    /// counted once.
    #[test]
    fn a_writer_at_capacity_takes_note_offs_to_its_room() {
        let mut buf = Vec::with_capacity(16);
        let dropped = PortCount::default();
        let mut params = Vec::new();
        let mut w = EventWriter::new(&mut buf, &mut params, 2, 16, &dropped);
        w.push(ev(0, 1)).unwrap();
        w.push(ev(1, 2)).unwrap();
        assert_eq!(w.push(ev(2, 3)), Err(EventRejected::Full));
        // Room for 2 + 0 + 8: eight note-offs.
        for k in 0..8 {
            assert_eq!(w.push(off(3 + k)), Ok(()), "note-off {k}");
        }
        assert_eq!(w.push(off(12)), Err(EventRejected::Full));
        w.count_skipped();
        w.recovered();
        assert_eq!((dropped.refused.get(), dropped.recovered.get()), (3, 1));
        assert_eq!(w.len(), 10);
    }

    /// The capacity a density asks for: the floor on a short block, one per
    /// frame on a long one, the cap beyond that, rounded up.
    #[test]
    fn a_density_sizes_a_port_by_the_block() {
        let at = |density: EventDensity, block| {
            density.capacity(&crate::Prepare::new(
                tutti_units::SampleRate::SR_48K,
                MaxBlock::of(Samples::new(block)),
            ))
        };
        let frame = EventDensity::per_frame(1);
        assert_eq!(at(frame, 1), MIN_EVENT_PORT_CAPACITY);
        assert_eq!(at(frame, 512).get(), 512);
        assert_eq!(at(frame, 8192).get(), 8192);
        assert_eq!(at(frame, 1 << 20), MAX_EVENT_PORT_CAPACITY);
        // A quarter per frame, rounded up: 1000 frames hold 250.
        let quarter = EventDensity::per_kiloframe(256);
        assert_eq!(at(quarter, 4096).get(), 1024);
        assert_eq!(at(quarter, 1001), MIN_EVENT_PORT_CAPACITY);
        assert_eq!(
            at(EventDensity::per_kiloframe(1), 4097),
            MIN_EVENT_PORT_CAPACITY
        );
    }
}
