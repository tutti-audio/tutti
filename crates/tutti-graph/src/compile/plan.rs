//! The compiled form: [`Plan`] (immutable, structure-of-arrays) and [`Delta`]
//! (which units a runtime must insert, retire or replace to run it).
//!
//! Everything here is plain data — `Vec`s of typed ids and a closed
//! [`Op`] enum, never `Box<dyn Fn>` — so a `Plan` is `Send + Sync` and can be
//! published to the audio thread as one value, and a
//! verifier or a test can read every decision the compiler made.
//!
//! # Slots
//!
//! Every buffer a plan names is a **slot**: a typed index into one of two
//! arenas, one per port kind. The layout of each arena is fixed:
//!
//! ```text
//! audio:  [0: ZERO][1..=F: feedback reads][F+1..: coloured, per block]
//! event:  [0: EMPTY][1..=G: feedback reads][G+1..: coloured, per block]
//! ```
//!
//! Slot 0 is never written. A feedback slot is filled by the executor before
//! the first op of each block, from the feedback's delay state, and is never
//! written by an op. Event slots hold [`Plan::event_slot_capacity`] events:
//! a node's output port what its shape declares
//! ([`Shape::event_capacity`]), a delay's output (and a feedback slot) all
//! its FIFO can hold at its source's declared rate, and a merge's output as
//! many as all its inputs together, so a merge can never drop one. Coloured slots are shared between values whose lifetimes
//! cannot overlap under *any* schedule that respects the op DAG — see
//! `compile`'s colouring pass.
//!
//! # Keys: one rule for delay and feedback state
//!
//! Every piece of state that outlives a block is keyed so that a recompile
//! carries it exactly when it is still the same wire:
//!
//! - **A PDC delay** is keyed by **(sink port, source port)** — [`AudioDelayKey`] for
//!   audio, [`EventDelayKey`] for events.
//!   Rewiring a sink to another source starts a fresh (silent) ring: the
//!   previous source's past audio is never played out of the new wire. Pending events
//!   of an event delay whose key disappears are flushed to the sink at offset
//!   0 of the next block if the sink survives (a dropped note-off is a stuck
//!   note), and dropped with it otherwise.
//! - **A feedback edge** delays by exactly the `delay` its edge declares (see
//!   [`AudioFeedbackKey`], [`EventFeedbackKey`]). Audio feedback is keyed by the source port, *its unit
//!   generation* and the delay, so a replaced unit's last output does not
//!   feed the new loop and a changed delay starts a fresh ring. Event
//!   feedback is keyed per edge (sink, source, generation, delay), and a
//!   disappearing key flushes like a PDC event delay.

use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::NodeKey;
use crate::{EdgeId, InputPort, OutputPort, PortSource};
use tutti_units::{Latency, Samples, UnitAddr};

use crate::compile::VerifyError;
use crate::compile::carry::Carry;
use crate::compile::coarsen::Tasks;
use crate::compile::ids::{
    AudioDelayId, AudioFeedbackId, AudioSlotId, EventDelayId, EventFeedbackId, EventSlotId, Id,
    OpId, PlanEventTapId, PlanMeterId, PlanTapId, PlanUnitId, Table, TableSlice, TaskId,
};
use crate::compile::program::{BySlot, Program};
use crate::compile::timing::Timing;
use crate::exec::arena::Role;
use crate::model::fade::Fade;
use crate::model::spec::{EventInputPort, EventOutputPort, ValidGraph};
use crate::node::io::PortKind;
use crate::node::param::ParamIn;
use crate::node::shape::Shape;
use crate::node::{InPlaceMask, Prepare, Wired};

/// The audio slot every unconnected or `PortSource::Zero` input reads.
pub(crate) const ZERO_SLOT: AudioSlotId = AudioSlotId::ZERO;
/// The event slot every unconnected event input reads.
pub(crate) const EMPTY_SLOT: EventSlotId = EventSlotId::EMPTY;

/// A dense index into the runtime's unit store.
///
/// Resolved from a [`NodeKey`] on the **control** side by `compile`, so the
/// audio thread never hashes a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitIdx(pub u32);

/// A contiguous run of the elements `T` of one of a plan's flattened lists.
///
/// Only the list it indexes makes one (appending a run to it returns the run),
/// and the lowering renames one as the list's elements are renamed, so a span
/// names a run of a list of that element, in range for the plan it came from.
/// It has no public constructor (the empty span is `Default`).
pub struct Span<T> {
    start: u32,
    len: u32,
    list: PhantomData<fn() -> T>,
}

impl<T> Span<T> {
    /// Appends `items` to `list` and returns the run they occupy.
    pub(crate) fn extend(list: &mut Vec<T>, items: impl IntoIterator<Item = T>) -> Self {
        let start = list.len();
        list.extend(items);
        Self {
            start: start as u32,
            len: (list.len() - start) as u32,
            list: PhantomData,
        }
    }

    /// The run `start..end` of a list, for a test that corrupts one.
    #[cfg(test)]
    pub(crate) fn of(start: usize, end: usize) -> Self {
        Self {
            start: start as u32,
            len: (end - start) as u32,
            list: PhantomData,
        }
    }

    /// The positions the run occupies in its list.
    #[must_use]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }

    /// How many elements the run holds.
    #[must_use]
    pub fn len(self) -> usize {
        self.len as usize
    }

    /// Whether the run holds nothing.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The same positions of a list whose elements `U` replace `T` one for one.
    /// The lowering's one rename: [`Op::map`] calls it, because `colour` maps
    /// every element of each operand list in place and keeps its positions.
    pub(crate) fn retype<U>(self) -> Span<U> {
        Span {
            start: self.start,
            len: self.len,
            list: PhantomData,
        }
    }
}

impl<T> Clone for Span<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Span<T> {}

impl<T> Default for Span<T> {
    fn default() -> Self {
        Self {
            start: 0,
            len: 0,
            list: PhantomData,
        }
    }
}

impl<T> PartialEq for Span<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.start, self.len) == (other.start, other.len)
    }
}

impl<T> Eq for Span<T> {}

impl<T> std::hash::Hash for Span<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.start, self.len).hash(state);
    }
}

impl<T> std::fmt::Debug for Span<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Span")
            .field("start", &self.start)
            .field("len", &self.len)
            .finish()
    }
}

/// Which PDC delay of audio a ring belongs to.
///
/// The ring's **state is keyed by this**, so it survives any recompile that
/// keeps the key: an unrelated edit does not click. The key names both ends
/// of the wire, so rewiring a sink to another source starts a fresh (silent)
/// ring and the previous source's past audio is never played out of the new
/// wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AudioDelayKey {
    /// An audio input port, delayed to align with the node's latest input.
    Edge {
        /// The sink port.
        at: InputPort,
        /// What feeds it: a node port or a main input channel.
        from: PortSource,
    },
    /// A main output channel, delayed to align with the slowest channel.
    Output {
        /// The graph output channel.
        channel: u16,
        /// What feeds it.
        from: PortSource,
    },
    /// One lane of an aligned meter, delayed like an output channel.
    Meter {
        /// The meter.
        meter: crate::MeterId,
        /// The lane within it.
        lane: usize,
        /// What feeds it.
        from: PortSource,
    },
    /// One lane of an aligned tap, delayed like an output channel.
    Tap {
        /// The tap.
        tap: crate::TapId,
        /// The lane within it.
        lane: usize,
        /// What feeds it.
        from: PortSource,
    },
}

/// Which PDC delay of events a FIFO belongs to.
///
/// The FIFO's state is keyed by this, as an [`AudioDelayKey`]'s ring is.
/// Pending events of a delay whose key disappears are flushed to the sink at
/// the start of its next block if the sink survives (a dropped note-off would
/// be a stuck note), and dropped with it otherwise; a
/// [`ParamEvent`](Self::ParamEvent), an [`EdgeGain`](Self::EdgeGain) and an
/// [`EventTap`](Self::EventTap) are not flushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventDelayKey {
    /// One source of an event input port. Each source of a fan-in port can
    /// need a different delay.
    Edge {
        /// The sink.
        at: EventInputPort,
        /// The source.
        from: EventOutputPort,
    },
    /// The event source of a driven param, delayed to the param's node's
    /// arrival (see [`GraphSpec::connect_param`](crate::GraphSpec::connect_param)).
    /// Its pending events are not flushed when the key disappears: the
    /// source was disconnected, and the port crossfades away from it.
    ParamEvent {
        /// The param port.
        at: ParamIn,
        /// The event output that drives it.
        from: EventOutputPort,
    },
    /// The event source of a driven edge gain, delayed to the edge's sink
    /// like a param's source ([`ParamEvent`](Self::ParamEvent), whose rules
    /// it has): to the sink node's arrival, or, for a main output channel,
    /// to the output time (see
    /// [`GraphSpec::connect_edge_gain`](crate::GraphSpec::connect_edge_gain)).
    /// Its pending events are not flushed when the key disappears.
    EdgeGain {
        /// The edge whose gain it drives.
        id: crate::EdgeId,
        /// The event output that drives it.
        from: EventOutputPort,
    },
    /// An aligned event tap's source, delayed to the output time as an event
    /// edge into an output-time sink would be.
    EventTap {
        /// The event tap.
        tap: crate::EventTapId,
        /// The event port copied.
        from: crate::EventTapPort,
    },
}

impl EventDelayKey {
    /// The event port whose stream this delay carries.
    pub(crate) fn origin(self) -> crate::EventTapPort {
        match self {
            Self::Edge { from, .. }
            | Self::ParamEvent { from, .. }
            | Self::EdgeGain { from, .. } => crate::EventTapPort::Out(from),
            Self::EventTap { from, .. } => from,
        }
    }
}

/// One PDC delay of audio in a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioDelay {
    /// Its state key.
    pub key: AudioDelayKey,
    /// Its length. Never zero — a zero delay is no op at all.
    pub len: Samples,
}

/// One PDC delay of events in a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventDelay {
    /// Its state key.
    pub key: EventDelayKey,
    /// Its length. Never zero — a zero delay is no op at all.
    pub len: Samples,
}

/// Which audio feedback delay a slot is read from: an audio output port of a
/// unit generation, shared by every reader.
///
/// **A feedback edge delays by exactly the `delay` its edge declares**,
/// whatever length the current block is: a ring of that length for audio, a
/// FIFO for events. The delay is part of the graph, not of the runtime, so a
/// bounce prepared at a larger `MaxBlock` loops exactly like live playback —
/// and `compile` refuses a delay shorter than the `MaxBlock` it is compiling
/// for, because a block cannot read samples it has not yet produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AudioFeedbackKey {
    /// The source port.
    pub from: OutputPort,
    /// The source unit's generation.
    pub generation: u32,
    /// The edge's delay.
    pub delay: Samples,
}

/// Which event feedback delay a slot is read from: one event feedback edge,
/// delayed as an [`AudioFeedbackKey`]'s is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventFeedbackKey {
    /// The sink port.
    pub at: EventInputPort,
    /// The source port.
    pub from: EventOutputPort,
    /// The source unit's generation.
    pub generation: u32,
    /// The edge's delay.
    pub delay: Samples,
}

/// One feedback slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeedbackSpec<K, S> {
    /// What it carries.
    pub key: K,
    /// Its slot, in the arena of its kind.
    pub slot: S,
}

/// A node's declared event-input bound, as its merge holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputBound {
    /// The most events the merged input keeps ([`Shape::event_input_capacity`]).
    pub capacity: NonZeroU32,
    /// The plan unit whose input it is: what is cut is charged to it.
    pub unit: PlanUnitId,
}

/// How many events one event slot holds per block, in the two currencies a
/// plan knows: events that ports **declared**
/// ([`Shape::event_capacity`]), and ports that declared nothing, each worth
/// the executor's default capacity — which the compiler does not know (it is
/// the editor's, [`Editor::with_event_capacity`](crate::Editor::with_event_capacity)).
/// [`events`](Self::events) prices it once the default is known.
///
/// A slot holding a node's output port holds that port's capacity; a PDC
/// delay's output or a feedback slot, everything its FIFO can hold
/// (priced from the FIFO bound: what can fall due in one block); a merge's
/// output, the **sum** of its
/// inputs' — so a merge never drops. A slot shared by several values (the
/// colouring pass) holds the largest of each currency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EventSlotCapacity {
    /// Events declared by the ports the slot may hold, summed.
    pub declared: u32,
    /// Ports the slot may hold that declared no capacity.
    pub defaults: u32,
}

impl EventSlotCapacity {
    /// Holds nothing: the empty slot.
    pub const NONE: Self = Self {
        declared: 0,
        defaults: 0,
    };

    /// What one event output port declaring `cap` fills.
    pub const fn port(cap: Option<NonZeroU32>) -> Self {
        match cap {
            Some(n) => Self {
                declared: crate::event::writer::note_off_room(n.get() as usize) as u32,
                defaults: 0,
            },
            None => Self {
                declared: 0,
                defaults: 1,
            },
        }
    }

    /// What the output of an event delay (a PDC delay, or a feedback edge)
    /// of `len` frames fed by a port declaring `cap` can hold: everything its
    /// FIFO can (`EventFifo::bound`), since that is what
    /// can fall due in one block — events the source wrote across several
    /// of its blocks, or a backlog a retune made overdue. Pricing it at the
    /// source's one block would deliver the rest a block late.
    pub(crate) fn fifo(cap: Option<NonZeroU32>, len: Samples, max_block: usize) -> Self {
        let fifo_bound = |n: usize| crate::exec::rings::EventFifo::bound(len.get(), n, max_block);
        match cap {
            Some(n) => Self {
                declared: u32::try_from(fifo_bound(crate::event::writer::note_off_room(
                    n.get() as usize
                )))
                .unwrap_or(u32::MAX),
                defaults: 0,
            },
            // `default × blocks`, whatever the default turns out to be.
            None => Self {
                declared: 0,
                defaults: u32::try_from(crate::exec::rings::EventFifo::blocks(
                    len.get(),
                    max_block,
                ))
                .unwrap_or(u32::MAX),
            },
        }
    }

    /// The FIFO of a stream merged from ports declaring `caps`: each one's
    /// [`fifo`](Self::fifo) bound, added.
    pub(crate) fn fifo_sum(
        caps: impl IntoIterator<Item = Option<NonZeroU32>>,
        len: Samples,
        max_block: usize,
    ) -> Self {
        caps.into_iter().fold(Self::NONE, |acc, cap| {
            acc.plus(Self::fifo(cap, len, max_block))
        })
    }

    /// This slot's room under an input bound: a slot that cannot hold more
    /// than `bound` stays as it is; any other holds `bound`. (A slot that
    /// may hold ports that declared nothing is priced at the executor's
    /// default, which the compiler does not know, so it holds `bound`.)
    #[must_use]
    pub(crate) fn bounded(self, bound: Option<NonZeroU32>) -> Self {
        match bound {
            Some(n) if self.defaults != 0 || self.declared > n.get() => Self {
                declared: n.get(),
                defaults: 0,
            },
            _ => self,
        }
    }

    /// Room for both: a merge of the two.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            declared: self.declared.saturating_add(other.declared),
            defaults: self.defaults.saturating_add(other.defaults),
        }
    }

    /// Room for either: a slot shared by the two.
    #[must_use]
    pub fn covering(self, other: Self) -> Self {
        Self {
            declared: self.declared.max(other.declared),
            defaults: self.defaults.max(other.defaults),
        }
    }

    /// Whether this holds everything `need` does, in both currencies.
    #[must_use]
    pub const fn holds(self, need: Self) -> bool {
        self.declared >= need.declared && self.defaults >= need.defaults
    }

    /// Events, with each undeclared port worth `default` (and the note-off
    /// room past it, [`note_off_room`](crate::note_off_room)).
    #[must_use]
    pub fn events(self, default: usize) -> usize {
        (self.declared as usize).saturating_add(
            (self.defaults as usize).saturating_mul(crate::event::writer::note_off_room(default)),
        )
    }
}

/// The writers of what a delayed copy of an event input holds, over a delay
/// of `len` frames at blocks of `block`: `sources`' declared capacities, then
/// one writer for the commands scheduled to the input. Its rate is the room
/// for one block's commands (`COMMAND_CAPACITY`) spread over the delay's
/// blocks, so the FIFO holds that once, not once a block; a command past it
/// is counted lost at the tap like any other refusal.
pub(crate) fn input_writers(
    sources: impl IntoIterator<Item = Option<NonZeroU32>>,
    len: Samples,
    block: usize,
) -> Vec<Option<NonZeroU32>> {
    let blocks = crate::exec::rings::EventFifo::blocks(len.get(), block);
    let commands = crate::COMMAND_CAPACITY.div_ceil(blocks);
    sources
        .into_iter()
        .chain([NonZeroU32::new(u32::try_from(commands).unwrap_or(u32::MAX))])
        .collect()
}

/// What an event input's view holds at most of its source's events: the
/// source's `price`, and under the node's declared input bound `bound`, no
/// more than that. The one price `emit` gives a view and `verify` demands of
/// it; the room for the commands and flushed events a block adds is staged
/// with the executor (`stage.rs`).
pub(crate) fn view_capacity(
    price: EventSlotCapacity,
    bound: Option<NonZeroU32>,
) -> EventSlotCapacity {
    price.bounded(bound)
}

/// One node of the plan, resolved to its place in the unit store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanUnit {
    /// The author's key.
    pub key: NodeKey,
    /// The unit generation this plan expects at `idx`.
    pub generation: u32,
    /// Where the unit lives.
    pub idx: UnitIdx,
    /// The shape it was compiled against.
    pub shape: Shape,
    /// Compiled arrival latency at its inputs (handed to it as `Cx::arrival`).
    pub arrival: Latency,
    /// Its audio ports with a source or a consumer in this plan
    /// ([`Io::wired_inputs`](crate::Io::wired_inputs)).
    pub wired: Wired,
}

impl PlanUnit {
    /// Where the unit goes in the store.
    pub(crate) fn placement(&self) -> Placement {
        Placement {
            key: self.key,
            generation: self.generation,
            idx: self.idx,
        }
    }
}

/// One step of the plan.
///
/// Slot fields name a slot of the arena of the op's kind, tables name a row by
/// its typed id, and `Span`s are runs of [`Plan::audio_list`],
/// [`Plan::event_list`] or another list of the plan. An op names its audio and
/// event operands by `A` and `E`: a plan's are [`AudioSlotId`] and
/// [`EventSlotId`] (what this documents); the compiler's `emit` names values
/// until `colour` gives them slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op<A, E> {
    /// Copy a main input channel into a slot.
    MainInput {
        /// The main input channel.
        channel: u16,
        /// Audio slot written.
        dst: A,
    },
    /// Delay one audio channel by a PDC ring. `src == dst` is the in-place
    /// form.
    Delay {
        /// Index into [`Plan::audio_delays`].
        delay: AudioDelayId,
        /// Audio slot read.
        src: A,
        /// Audio slot written.
        dst: A,
    },
    /// Sum several audio slots into one: the fan-in of a port or an output
    /// channel. The value is the left fold
    /// `((+0.0 + a_0) + a_1) + ..` in the graph's float `F`, one rounding per add, over the
    /// addends in source order; an addend flagged silent adds nothing and is
    /// skipped. `dst` may be the first addend's own slot (the in-place form,
    /// when the colouring allows it), and no other addend's.
    Sum {
        /// Audio slots read, in source order (into [`Plan::audio_list`]): at
        /// least 2, at most [`MAX_PORTS`](crate::MAX_PORTS). A wider fan-in is
        /// a chain whose next op reads the last result first.
        srcs: Span<A>,
        /// Audio slot written.
        dst: A,
    },
    /// A [`Sum`](Self::Sum) with at least one **weighted lane**: the value is
    /// the left fold `((+0.0 + w_0 * a_0) + w_1 * a_1) + ..` in the graph's float `F`, a
    /// multiply and an add per lane, never fused. A lane whose weight is
    /// exactly `1.0` runs the plain add. Silent addends are skipped whatever
    /// their weight, and a weight of `0.0` is not skipped by value. Unlike
    /// `Sum`, one addend is a valid op: a one-edge sink with a gain entry runs
    /// `+0.0 + w * a`, where a plain one aliases.
    ///
    /// `Sum` is exactly this op with every weight `1.0`; it stays its own
    /// variant so the plain path carries no lane state.
    WeightedSum {
        /// Audio slots read, in source order (into [`Plan::audio_list`]): at
        /// least 1, at most [`MAX_PORTS`](crate::MAX_PORTS).
        srcs: Span<A>,
        /// Audio slot written; the first addend's own slot in the in-place
        /// form, as for `Sum`.
        dst: A,
        /// One entry per addend, in the same order (into
        /// the plan's sum lanes): the edge whose gain weights it, or `None`
        /// for a plain addend (an edge with no entry, or the previous result
        /// of a chain).
        lanes: Span<Option<EdgeId>>,
        /// The driven lanes among them, in lane order (into
        /// [`Plan::gain_ports`]): the sum reads each one's event source.
        /// Empty when no entry of this op has a source.
        gains: Span<GainPortOp<E>>,
    },
    /// Delay one event stream.
    EventDelay {
        /// Index into [`Plan::event_delays`].
        delay: EventDelayId,
        /// Event slot read.
        src: E,
        /// Event slot written.
        dst: E,
    },
    /// Merge several event streams by `(offset, source order)` — event
    /// fan-in. PortSource order is the source port's
    /// `(NodeKey, port)`.
    EventMerge {
        /// Event slots read, in source order (into [`Plan::event_list`]).
        srcs: Span<E>,
        /// Event slot written.
        dst: E,
        /// The node's [`Shape::event_input_capacity`] when `dst` is its
        /// input: the merge keeps at most this many events and charges what
        /// it cuts to the node.
        bound: Option<InputBound>,
    },
    /// Run a unit.
    Node {
        /// Index into [`Plan::units`].
        unit: PlanUnitId,
        /// Audio slot per input channel (into [`Plan::audio_list`]).
        audio_in: Span<A>,
        /// Audio slot per output channel.
        audio_out: Span<A>,
        /// Event slot per input port: what each input is merged from (into
        /// [`Plan::event_list`]): the merge of its edges, a feedback slot or
        /// the empty slot. Read by this op.
        event_src: Span<E>,
        /// Event slot per input port: the view of each input, the events the
        /// node is handed there this block. This op writes it from
        /// `event_src`, the unit's flushed events, its scheduled commands, its
        /// declared input bound and, when it is bypassed, the closes it owes;
        /// the node, the held-key tables and the input taps read it.
        event_in: Span<E>,
        /// Event slot per output port.
        event_out: Span<E>,
        /// Channels whose output slot *is* their input slot.
        in_place: InPlaceMask,
        /// The node's driven params, in port order (into
        /// [`Plan::param_ports`]); a declared param not listed reads its
        /// base. Their sources are read by this op, before the node runs.
        params: Span<ParamPortOp<E>>,
    },
    /// Write a main output channel, through its alignment ring if it has one.
    Output {
        /// The graph output channel.
        channel: u16,
        /// Audio slot read.
        src: A,
        /// Index into [`Plan::audio_delays`], when the channel is delayed.
        delay: Option<AudioDelayId>,
    },
    /// Push a block of every lane of a tap into its rings, together.
    Tap {
        /// Index into `Plan::taps`.
        tap: PlanTapId,
        /// Audio slot per lane (into [`Plan::audio_list`]).
        srcs: Span<A>,
    },
    /// Push every MIDI event of an event slot into an event tap's ring, each
    /// stamped with its frame. The slot is an output port's, or the view a
    /// node is handed at an input (or that view delayed).
    EventTap {
        /// Index into `Plan::event_taps`.
        tap: PlanEventTapId,
        /// Event slot read.
        src: E,
    },
    /// Fold a block of every lane of a meter into its cells.
    Meter {
        /// Index into `Plan::meters`.
        meter: PlanMeterId,
        /// Audio slot per lane (into [`Plan::audio_list`]).
        srcs: Span<A>,
    },
    /// Push an audio port's block into its feedback ring.
    Capture {
        /// Index into [`Plan::audio_feedback`].
        feedback: AudioFeedbackId,
        /// Audio slot read.
        src: A,
    },
    /// Queue an event port's events into its feedback delay.
    EventCapture {
        /// Index into [`Plan::event_feedback`].
        feedback: EventFeedbackId,
        /// Event slot read.
        src: E,
    },
}

impl<A, E> Op<A, E> {
    /// This op with its audio operands `a` and event operands `e` renamed;
    /// spans, indices and the in-place mask are kept.
    pub(crate) fn map<A2, E2>(self, a: impl Fn(A) -> A2, e: impl Fn(E) -> E2) -> Op<A2, E2> {
        match self {
            Op::MainInput { channel, dst } => Op::MainInput {
                channel,
                dst: a(dst),
            },
            Op::Delay { delay, src, dst } => Op::Delay {
                delay,
                src: a(src),
                dst: a(dst),
            },
            Op::Sum { srcs, dst } => Op::Sum {
                srcs: srcs.retype(),
                dst: a(dst),
            },
            Op::WeightedSum {
                srcs,
                dst,
                lanes,
                gains,
            } => Op::WeightedSum {
                srcs: srcs.retype(),
                dst: a(dst),
                lanes,
                gains: gains.retype(),
            },
            Op::EventDelay { delay, src, dst } => Op::EventDelay {
                delay,
                src: e(src),
                dst: e(dst),
            },
            Op::EventMerge { srcs, dst, bound } => Op::EventMerge {
                srcs: srcs.retype(),
                dst: e(dst),
                bound,
            },
            Op::Node {
                unit,
                audio_in,
                audio_out,
                event_src,
                event_in,
                event_out,
                in_place,
                params,
            } => Op::Node {
                unit,
                audio_in: audio_in.retype(),
                audio_out: audio_out.retype(),
                event_src: event_src.retype(),
                event_in: event_in.retype(),
                event_out: event_out.retype(),
                in_place,
                params: params.retype(),
            },
            Op::Output {
                channel,
                src,
                delay,
            } => Op::Output {
                channel,
                src: a(src),
                delay,
            },
            Op::Tap { tap, srcs } => Op::Tap {
                tap,
                srcs: srcs.retype(),
            },
            Op::EventTap { tap, src } => Op::EventTap { tap, src: e(src) },
            Op::Meter { meter, srcs } => Op::Meter {
                meter,
                srcs: srcs.retype(),
            },
            Op::Capture { feedback, src } => Op::Capture {
                feedback,
                src: a(src),
            },
            Op::EventCapture { feedback, src } => Op::EventCapture {
                feedback,
                src: e(src),
            },
        }
    }
}

/// One driven param port of a node op: the fused param step the op
/// runs before its node (see
/// [`GraphSpec::connect_param`](crate::GraphSpec::connect_param)).
///
/// It carries no range: the value is clamped to the port's live range, a
/// control the executor reads per block
/// ([`Editor::set_param_range`](crate::Editor::set_param_range)), so moving a
/// range never changes the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamPortOp<E> {
    /// Its index in the node's declared params
    /// ([`Shape::params`](crate::Shape::params)), or [`MIX_PORT`](Self::MIX_PORT):
    /// the node's [`Mix`](tutti_units::Mix), which no node declares and the
    /// mode state runs.
    pub port: u16,
    /// The param.
    pub param: UnitAddr,
    /// The event output that drives it: what its running state (the
    /// source's ramp) is kept by, so the executor crossfades a port whose
    /// source changed and leaves one that only moved slots alone.
    pub from: EventOutputPort,
    /// The event slot it reads its source's events from.
    pub slot: E,
}

impl<E> ParamPortOp<E> {
    /// The sentinel [`port`](Self::port) of a node's mix, [`NodeMix::ADDR`](crate::NodeMix::ADDR):
    /// past every declared index, so it is the last entry of an op's span.
    pub const MIX_PORT: u16 = u16::MAX;

    /// Whether this is the node's mix lane, not a declared param.
    #[must_use]
    pub const fn is_mix(&self) -> bool {
        self.port == Self::MIX_PORT
    }
}

/// One driven lane of an [`Op::WeightedSum`]: the fused step that gives an
/// edge's gain its value at each frame (see
/// [`GraphSpec::connect_edge_gain`](crate::GraphSpec::connect_edge_gain)),
/// run by the sum before it adds, the sibling of [`ParamPortOp`].
///
/// It carries no range, as a param port does not: the value is clamped to the
/// lane's live range, a control the executor reads per block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GainPortOp<E> {
    /// The lane's index among the op's addends (its entry of the op's
    /// `lanes`).
    pub lane: u16,
    /// The event output that drives it: what its running state (the
    /// source's ramp) is kept by.
    pub from: EventOutputPort,
    /// The event slot it reads its source's events from.
    pub slot: E,
}

/// How the executor borrows one node op's buffers — decided at compile time,
/// so a call does not re-derive it from the port counts.
///
/// The direct forms cover the shapes most nodes have: zero or one audio
/// input and one audio output ([`Source`](Self::Source),
/// [`Split`](Self::Split), [`InPlace`](Self::InPlace)), and the stereo
/// shapes ([`Direct`](Self::Direct)), all with no event ports. Their slots
/// are borrowed with no request table, and each gets its own specialised
/// call path. Everything else walks the op's presorted borrow requests
/// ([`NodePorts::borrows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// No audio input, one audio output, no event ports.
    Source {
        /// Audio slot written.
        out: AudioSlotId,
    },
    /// One audio input and one output in different slots, no event ports.
    /// The verifier proves the slots differ: an op that reads and writes one
    /// slot must declare it in place.
    Split {
        /// Audio slot read.
        input: AudioSlotId,
        /// Audio slot written.
        out: AudioSlotId,
    },
    /// One audio channel aliased in place, no event ports.
    InPlace {
        /// The slot that holds the input and receives the output.
        slot: AudioSlotId,
    },
    /// One of the fixed small shapes in [`Direct::SHAPES`], no event ports.
    Direct(Direct),
    /// Any audio width, no event ports: the audio borrow walk only.
    Audio,
    /// Event ports too.
    General,
}

/// The borrow of a [`Form::Direct`] node: its distinct slots in ascending
/// order, what each one is, and where each input channel finds its slot.
///
/// Distinct, because two input channels may read one slot (two ports on the
/// zero slot, say) and a slot can be split off the arena only once. Sorted,
/// so the executor peels them off with `split_at_mut` in one pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Direct {
    /// Audio input channels.
    pub(crate) ins: u8,
    /// Audio output channels.
    pub(crate) outs: u8,
    /// How many of `slots` are used.
    pub(crate) count: u8,
    /// The distinct slots, ascending; unused entries are the null slot.
    pub(crate) slots: [AudioSlotId; 4],
    /// Per slot: the output channel it is, or a slot only read.
    pub(crate) role: [DirectRole; 4],
    /// Per input channel: the index into `slots` it reads, or `None` when
    /// the channel is aliased in place (it is then in its output's buffer).
    pub(crate) input: [Option<u8>; 2],
}

/// What a [`Direct`] node does with one of its slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DirectRole {
    /// A slot only read.
    Read,
    /// The slot is this output channel.
    Out(u8),
}

impl Direct {
    /// `(inputs, outputs)` shapes that take the direct form: a stereo
    /// source, mono to stereo, stereo to mono, and stereo to stereo. The
    /// one-output shapes with at most one input have their own forms.
    pub(crate) const SHAPES: [(usize, usize); 4] = [(0, 2), (1, 2), (2, 1), (2, 2)];

    /// The direct borrow for these ports, if the shape has one.
    fn lower(ain: &[AudioSlotId], aout: &[AudioSlotId], in_place: InPlaceMask) -> Option<Self> {
        if !Self::SHAPES.contains(&(ain.len(), aout.len())) {
            return None;
        }
        let mut entries: Vec<(AudioSlotId, DirectRole)> = aout
            .iter()
            .enumerate()
            .map(|(c, &s)| (s, DirectRole::Out(c as u8)))
            .collect();
        for (c, &s) in ain.iter().enumerate() {
            if !in_place.get(c) && !entries.contains(&(s, DirectRole::Read)) {
                entries.push((s, DirectRole::Read));
            }
        }
        entries.sort_unstable();
        let mut d = Self {
            ins: ain.len() as u8,
            outs: aout.len() as u8,
            count: entries.len() as u8,
            slots: [ZERO_SLOT; 4],
            role: [DirectRole::Read; 4],
            input: [None; 2],
        };
        for (i, &(s, r)) in entries.iter().enumerate() {
            d.slots[i] = s;
            d.role[i] = r;
        }
        for (c, &s) in ain.iter().enumerate() {
            if !in_place.get(c) {
                d.input[c] = Some(
                    entries
                        .iter()
                        .position(|&e| e == (s, DirectRole::Read))
                        .expect("every read was entered") as u8,
                );
            }
        }
        Some(d)
    }
}

/// What a node call reads that only lowering knows: its port slots, its
/// buffer borrows and how it takes them. One row per plan unit
/// ([`Plan::ports`]), found by the unit's index; everything the unit's shape
/// or place already says ([`PlanUnit`]) is read from there.
///
/// Derived from the unit's [`Op::Node`], the program's lists and its shape
/// ([`NodeTables::lower`]), and checked against them by `verify`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NodePorts {
    /// Channels aliased in place.
    pub(crate) in_place: InPlaceMask,
    /// The ports its [`Io`](crate::Io)'s main views cover
    /// ([`Shape::main_ports`](crate::Shape)): cached because the shape walks
    /// its buses to find them.
    pub(crate) main: crate::node::shape::MainPorts,
    /// Start of this op's audio slots in [`NodeTables::audio_slots`]: inputs,
    /// then outputs.
    pub(crate) audio_ports: u32,
    /// Start of this op's event slots in [`NodeTables::event_slots`].
    pub(crate) event_ports: u32,
    /// Audio input channels.
    pub(crate) ain: u16,
    /// Audio output channels.
    pub(crate) aout: u16,
    /// Event input ports.
    pub(crate) ein: u16,
    /// Event output ports.
    pub(crate) eout: u16,
    /// The audio borrow requests, sorted (into [`NodeTables::audio_borrows`]):
    /// every input not aliased in place, and every output.
    pub(crate) borrows: Span<(AudioSlotId, Role)>,
    /// The event borrow requests, sorted (into [`NodeTables::event_borrows`]):
    /// every input's view and every output.
    pub(crate) event_borrows: Span<(EventSlotId, Role)>,
    /// The requests of the step that fills the views, sorted (into
    /// [`NodeTables::event_assemble`]): each input's source read as
    /// `Role::Read(port)`, each view written as `Role::Write(port)`.
    pub(crate) event_assemble: Span<(EventSlotId, Role)>,
    /// How the call borrows its buffers.
    pub(crate) form: Form,
    /// The node's driven params (into [`Plan::param_ports`]).
    pub(crate) params: Span<ParamPortOp<EventSlotId>>,
}

/// The flat lists the [`NodePorts`] rows index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NodeTables {
    /// Audio port slots, per row: inputs, then outputs.
    pub(crate) audio_slots: Vec<AudioSlotId>,
    /// Event port slots, per row: inputs, then outputs.
    pub(crate) event_slots: Vec<EventSlotId>,
    /// Audio borrow requests, each row's run sorted: the order
    /// `arena::borrow_sorted` walks without sorting.
    pub(crate) audio_borrows: Vec<(AudioSlotId, Role)>,
    /// Event borrow requests, each row's run sorted.
    pub(crate) event_borrows: Vec<(EventSlotId, Role)>,
    /// The requests that fill each row's views, each run sorted.
    pub(crate) event_assemble: Vec<(EventSlotId, Role)>,
}

/// The slots of one node op's ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PortSlots<'t> {
    pub(crate) ain: &'t [AudioSlotId],
    pub(crate) aout: &'t [AudioSlotId],
    pub(crate) ein: &'t [EventSlotId],
    pub(crate) eout: &'t [EventSlotId],
}

impl<'t> PortSlots<'t> {
    /// A node with audio ports only.
    pub(crate) const fn audio(ain: &'t [AudioSlotId], aout: &'t [AudioSlotId]) -> Self {
        Self {
            ain,
            aout,
            ein: &[],
            eout: &[],
        }
    }
}

impl NodePorts {
    /// This op's audio in, audio out, event in and event out slots.
    #[inline]
    pub(crate) fn slots<'t>(&self, tables: &'t NodeTables) -> PortSlots<'t> {
        let (ain, aout, ein, eout) = (
            usize::from(self.ain),
            usize::from(self.aout),
            usize::from(self.ein),
            usize::from(self.eout),
        );
        let audio = &tables.audio_slots[self.audio_ports as usize..][..ain + aout];
        let event = &tables.event_slots[self.event_ports as usize..][..ein + eout];
        let (ain, aout) = audio.split_at(ain);
        let (ein, eout) = event.split_at(ein);
        PortSlots {
            ain,
            aout,
            ein,
            eout,
        }
    }
}

impl NodeTables {
    /// Lower every unit's [`Op::Node`], found through `program.node_op`:
    /// the [`NodePorts`] column, one row per unit, and the lists it indexes.
    pub(crate) fn lower(
        program: &Program<BySlot>,
        units: &TableSlice<PlanUnitId, PlanUnit>,
    ) -> (Table<PlanUnitId, NodePorts>, Self) {
        let (audio_list, event_list) = (&program.audio_list, &program.event_list);
        let mut tables = Self::default();
        let ports = units
            .iter_enumerated()
            .map(|(unit, pu)| {
                let Op::Node {
                    audio_in,
                    audio_out,
                    event_src,
                    event_in,
                    event_out,
                    in_place,
                    params,
                    ..
                } = program.ops[program.node_op[unit]]
                else {
                    unreachable!("a unit's node op is a node op")
                };
                let ain = &audio_list[audio_in.range()];
                let aout = &audio_list[audio_out.range()];
                let ein = &event_list[event_in.range()];
                let eout = &event_list[event_out.range()];

                let audio_ports = tables.audio_slots.len() as u32;
                tables.audio_slots.extend_from_slice(ain);
                tables.audio_slots.extend_from_slice(aout);
                let event_ports = tables.event_slots.len() as u32;
                tables.event_slots.extend_from_slice(ein);
                tables.event_slots.extend_from_slice(eout);

                // Built and sorted once here, so the executor never sorts per
                // call.
                let audio = Span::extend(
                    &mut tables.audio_borrows,
                    ain.iter()
                        .enumerate()
                        .filter(|&(c, _)| !in_place.get(c))
                        .map(|(c, &s)| (s, Role::Read(c as u8)))
                        .chain(
                            aout.iter()
                                .enumerate()
                                .map(|(c, &s)| (s, Role::Write(c as u8))),
                        ),
                );
                tables.audio_borrows[audio.range()].sort_unstable();
                let event = Span::extend(
                    &mut tables.event_borrows,
                    ein.iter()
                        .enumerate()
                        .map(|(c, &s)| (s, Role::Read(c as u8)))
                        .chain(
                            eout.iter()
                                .enumerate()
                                .map(|(c, &s)| (s, Role::Write(c as u8))),
                        ),
                );
                tables.event_borrows[event.range()].sort_unstable();
                let assemble = Span::extend(
                    &mut tables.event_assemble,
                    event_list[event_src.range()]
                        .iter()
                        .enumerate()
                        .map(|(c, &s)| (s, Role::Read(c as u8)))
                        .chain(
                            ein.iter()
                                .enumerate()
                                .map(|(c, &s)| (s, Role::Write(c as u8))),
                        ),
                );
                tables.event_assemble[assemble.range()].sort_unstable();

                let form = match (ain, aout, ein.is_empty() && eout.is_empty()) {
                    (_, _, false) => Form::General,
                    (&[], &[out], true) => Form::Source { out },
                    (&[slot], &[_], true) if in_place.get(0) => Form::InPlace { slot },
                    (&[input], &[out], true) => Form::Split { input, out },
                    (_, _, true) => {
                        Direct::lower(ain, aout, in_place).map_or(Form::Audio, Form::Direct)
                    }
                };
                NodePorts {
                    main: pu.shape.main_ports(),
                    in_place,
                    audio_ports,
                    event_ports,
                    ain: ain.len() as u16,
                    aout: aout.len() as u16,
                    ein: ein.len() as u16,
                    eout: eout.len() as u16,
                    borrows: audio,
                    event_borrows: event,
                    event_assemble: assemble,
                    form,
                    params,
                }
            })
            .collect();
        (ports, tables)
    }
}

/// Compressed sparse rows: row `i` is `targets[offsets[i]..offsets[i + 1]]`.
///
/// A row's targets are `T`, the typed id of the table they index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Csr<T> {
    pub(crate) offsets: Vec<u32>,
    pub(crate) targets: Vec<T>,
}

impl<T> Default for Csr<T> {
    fn default() -> Self {
        Self {
            offsets: Vec::new(),
            targets: Vec::new(),
        }
    }
}

impl<T> Csr<T> {
    /// Number of rows.
    pub fn rows(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Row `i`.
    pub fn row(&self, i: usize) -> &[T] {
        &self.targets[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
}

impl<T: Copy> Csr<T> {
    /// The rows of a table of typed lists, in the table's order.
    pub(crate) fn from_table<R: Id>(rows: &TableSlice<R, Vec<T>>) -> Self {
        let mut offsets = Vec::with_capacity(rows.len() + 1);
        let mut targets = Vec::new();
        offsets.push(0);
        for row in rows {
            targets.extend_from_slice(row);
            offsets.push(targets.len() as u32);
        }
        Self { offsets, targets }
    }

    /// Row `r`.
    pub(crate) fn of<R: Id>(&self, r: R) -> &[T] {
        self.row(r.index())
    }

    /// Every row as a typed list, in row order.
    pub(crate) fn to_table<R: Id>(&self) -> Table<R, Vec<T>> {
        (0..self.rows()).map(|i| self.row(i).to_vec()).collect()
    }
}

impl Csr<OpId> {
    /// The rows the other way round: row `a` of the result lists the rows
    /// that name `a`, ascending.
    pub(crate) fn inverse(&self) -> Self {
        let mut rows: Table<OpId, Vec<OpId>> = (0..self.rows()).map(|_| Vec::new()).collect();
        for (b, row) in self.to_table::<OpId>().iter_enumerated() {
            for &a in row {
                rows[a].push(b);
            }
        }
        Self::from_table(&rows)
    }
}

/// One buffer's worth of data the plan moves: who writes it, who reads it,
/// and the slot the colouring gave it. Kept in the plan so the verifier — and
/// a test — can check the colouring against the op DAG without recomputing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value<S> {
    /// The slot it lives in.
    pub slot: S,
    /// The op that writes it.
    pub writer: OpId,
    /// The ops that read it (into [`Plan::value_readers`]).
    pub readers: Span<OpId>,
}

/// The compiled, immutable form of a graph, produced by
/// [`compile`](fn@crate::compile) and run by the [`Executor`](crate::Executor).
///
/// Plain data (`Send + Sync`, `Vec`s of indices and a closed [`Op`] enum),
/// so it is shared with the audio thread as one `Arc` and a test or the
/// [`Plan::verify`] pass can read every decision the compiler made.
/// Most hosts never look inside one: [`Editor::commit`](crate::Editor::commit)
/// compiles and ships it.
///
/// Every buffer it names is a **slot**, an index into one of two arenas
/// (audio and event). Slot 0 of each (`ZERO_SLOT`, `EMPTY_SLOT`) is
/// never written and is what an unconnected input reads; feedback slots
/// come next, filled from the feedback state before the first op of each
/// block; the rest are shared between values whose lifetimes cannot
/// overlap.
///
/// `ops` is one **serial schedule** (a topological order of the op DAG); the
/// DAG itself is kept as CSR successor lists so a parallel executor can run
/// any order consistent with it — and the buffer colouring is correct under
/// every such order, not just the one-participant one.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub(crate) prepare: Prepare,
    /// The ops and everything they name: lists, values, dependencies, keyed
    /// state and the arenas.
    pub(crate) program: Program<BySlot>,
    /// Every event edge, `(source port, sink port)`, direct and feedback,
    /// sorted: what the executor compares across a commit to close the held
    /// keys of an edge that ended (`held.rs`), and the sources an input tap's
    /// FIFO is sized for. Built from the same `Graph::events` the compiler
    /// sizes that FIFO from, so the two agree; verify re-derives the size
    /// from this list and refuses a plan where they do not.
    pub(crate) event_edges: Vec<(EventOutputPort, EventInputPort)>,
    pub(crate) tasks: Table<TaskId, Span<OpId>>,
    pub(crate) task_ops: Vec<OpId>,
    pub(crate) task_succ: Csr<TaskId>,
    pub(crate) task_activation: Table<TaskId, u32>,
    pub(crate) units: Table<PlanUnitId, PlanUnit>,
    /// Which plan this is: a [`Delta`] is compiled against one.
    pub(crate) id: Identity,
    pub(crate) main_inputs: u16,
    pub(crate) compensation: Vec<Samples>,
    /// The main output generation the spec carried when this plan was committed.
    pub(crate) main_output_id: crate::MainOutputId,
    /// The old main output's outputs this plan renders after its own channels,
    /// while a main output change crossfades.
    pub(crate) main_output_bank: Option<crate::MainOutputBank>,
    /// Output channels the plan renders: the main output's, then the bank's.
    pub(crate) rendered_outputs: usize,
    pub(crate) total_latency: Latency,
    /// The output time the bank's channels align to (read only when the plan
    /// has a bank): the total of the plan the main output change was made from, kept
    /// by a plan that continues the same crossfade.
    pub(crate) bank_total: Latency,
    /// What each unit's node call reads that only lowering knows, by unit.
    /// Derived from the program and the units; `verify` checks it against
    /// them.
    pub(crate) ports: Table<PlanUnitId, NodePorts>,
    /// The lists [`ports`](Self::ports) indexes.
    pub(crate) nodes: NodeTables,
}

// The editor publishes a `Plan` to the audio thread. Asserted here rather
// than discovered there.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Plan>();
    send_sync::<Delta>();
};

impl Plan {
    /// The plan of `program`, whose ops `tasks` partition, over `units`.
    pub(super) fn new(
        prepare: &Prepare,
        program: Program<BySlot>,
        tasks: Tasks,
        units: Table<PlanUnitId, PlanUnit>,
        graph: &ValidGraph,
        timing: Timing,
    ) -> Self {
        let (ports, nodes) = NodeTables::lower(&program, &units);
        let mut event_edges: Vec<(EventOutputPort, EventInputPort)> = graph
            .events()
            .iter()
            .flat_map(|(&at, edges)| edges.iter().map(move |e| (e.from(), at)))
            .collect();
        event_edges.sort();
        Self {
            prepare: *prepare,
            program,
            event_edges,
            tasks: tasks.tasks,
            task_ops: tasks.task_ops,
            task_succ: tasks.succ,
            task_activation: tasks.activation,
            units,
            id: Identity(PlanId::mint()),
            main_inputs: graph.topology().inputs.count(),
            compensation: timing.compensation,
            main_output_id: crate::MainOutputId::GENESIS,
            main_output_bank: graph.main_output_bank(),
            rendered_outputs: timing.rendered_outputs,
            total_latency: timing.total,
            bank_total: timing.bank_total,
            ports,
            nodes,
        }
    }

    /// Checks the plan against its op DAG: no slot shared by ops that may run
    /// concurrently, every read of the value it was meant to read, feedback
    /// read before it is captured. [`compile()`](fn@crate::compile) runs this in
    /// every debug build.
    ///
    /// # Errors
    ///
    /// Returns the first [`VerifyError`] found.
    pub fn verify(&self) -> Result<(), VerifyError> {
        crate::compile::verify::verify(self)
    }

    /// Checks the crossfades `delta` carries into this plan, from `prev` (the
    /// plan running before it): each names a key the delta replaces, once, and
    /// the unit it fades from has the shape of the one it fades to in
    /// everything but its tail: ports, latency, in-place acceptance, event
    /// resolution, event capacity, declared params.
    /// [`Editor::package`](crate::Editor::package) runs this on every delta it
    /// is handed.
    ///
    /// # Errors
    ///
    /// Returns the first [`VerifyError`] found.
    pub fn verify_fades(&self, prev: Option<&Plan>, delta: &Delta) -> Result<(), VerifyError> {
        crate::compile::verify::verify_fades(prev, self, delta)
    }

    /// The audio slots of `span` in [`audio_list`](Self::audio_list).
    pub(crate) fn audio_slots(&self, span: Span<AudioSlotId>) -> &[AudioSlotId] {
        &self.program.audio_list[span.range()]
    }

    /// The event slots of `span` in [`event_list`](Self::event_list).
    pub(crate) fn event_slots(&self, span: Span<EventSlotId>) -> &[EventSlotId] {
        &self.program.event_list[span.range()]
    }

    /// The ops, in serial order.
    pub fn ops(&self) -> &[Op<AudioSlotId, EventSlotId>] {
        self.program.ops.as_slice()
    }

    /// The flattened audio slot lists [`Op`] spans index.
    pub fn audio_list(&self) -> &[AudioSlotId] {
        &self.program.audio_list
    }

    /// The flattened event slot lists [`Op`] spans index.
    pub fn event_list(&self) -> &[EventSlotId] {
        &self.program.event_list
    }

    /// The ops each op waits for: the dependency DAG, each edge from an
    /// earlier op to a later one.
    pub fn op_dependencies(&self) -> &Csr<OpId> {
        &self.program.deps
    }

    /// Tasks after coarsening (chains fused, cheap siblings packed): each is
    /// a run of op indices (into [`task_ops`](Self::task_ops)), ascending,
    /// that execute back to back on one thread.
    pub fn tasks(&self) -> &[Span<OpId>] {
        self.tasks.as_slice()
    }

    /// Op indices, grouped by task.
    pub fn task_ops(&self) -> &[OpId] {
        &self.task_ops
    }

    /// Distinct predecessor tasks of each task — the initial activation count
    /// a parallel executor's per-task counter starts from.
    pub fn task_activation(&self) -> &[u32] {
        self.task_activation.as_slice()
    }

    /// Arena size in slots for `kind`, including the null slot (0) and the
    /// feedback slots.
    pub fn slots(&self, kind: PortKind) -> u32 {
        match kind {
            PortKind::Audio => self.program.sizing.audio.len() as u32,
            PortKind::Event => self.program.sizing.events.len() as u32,
        }
    }

    /// How many events each event slot holds per block (see the module
    /// docs), by slot index.
    pub fn event_slot_capacity(&self) -> &[EventSlotCapacity] {
        self.program.sizing.events.as_slice()
    }

    /// What event output port `port` declared it writes per block
    /// ([`Shape::event_capacity`]): `None` for the executor's default, or
    /// for a port this plan does not have.
    pub fn event_port_capacity(&self, port: EventOutputPort) -> Option<NonZeroU32> {
        self.unit(port.node)
            .filter(|u| port.port < u.shape.event_out)
            .and_then(|u| u.shape.event_capacity)
    }

    /// Whether event output port `port` writes param events
    /// ([`Shape::param_events`]).
    pub(crate) fn event_port_params(&self, port: EventOutputPort) -> bool {
        self.unit(port.node)
            .is_some_and(|u| port.port < u.shape.event_out && u.shape.param_events)
    }

    /// Whether what an event tap's source carries includes param events: the
    /// port itself writes them, or, for an input, its node takes them (a
    /// scheduled command can carry one) or a port with an edge into it
    /// writes them.
    pub(crate) fn event_tap_params(&self, source: crate::EventTapPort) -> bool {
        match source {
            crate::EventTapPort::Out(port) => self.event_port_params(port),
            crate::EventTapPort::In(at) => {
                self.unit(at.node).is_some_and(|u| u.shape.param_events)
                    || self
                        .event_edges
                        .iter()
                        .any(|&(from, to)| to == at && self.event_port_params(from))
            }
        }
    }

    /// The declared capacity of each writer of what an event tap's source
    /// carries over a delay of `len` frames at blocks of `block`: the output
    /// port itself, or, for an input, every port with an edge into it and
    /// the commands scheduled to it ([`input_writers`]).
    pub(crate) fn event_tap_ports(
        &self,
        source: crate::EventTapPort,
        len: Samples,
        block: usize,
    ) -> Vec<Option<NonZeroU32>> {
        match source {
            crate::EventTapPort::Out(port) => vec![self.event_port_capacity(port)],
            crate::EventTapPort::In(at) => input_writers(
                self.event_edges
                    .iter()
                    .filter(|&&(_, to)| to == at)
                    .map(|&(from, _)| self.event_port_capacity(from)),
                len,
                block,
            ),
        }
    }

    /// What the delay FIFO of an event tap's source holds: the `fifo` bound of
    /// each writer, added.
    pub(crate) fn event_tap_fifo(
        &self,
        source: crate::EventTapPort,
        len: Samples,
        block: usize,
    ) -> EventSlotCapacity {
        EventSlotCapacity::fifo_sum(self.event_tap_ports(source, len, block), len, block)
    }

    /// The rate an event tap's delay ring is sized from: the note-off room of
    /// each writer's capacity (or `default`), added.
    pub(crate) fn event_tap_rate(
        &self,
        source: crate::EventTapPort,
        len: Samples,
        block: usize,
        default: usize,
    ) -> usize {
        self.event_tap_ports(source, len, block)
            .into_iter()
            .map(|c| crate::event::writer::note_off_room(c.map_or(default, |n| n.get() as usize)))
            .sum()
    }

    /// The audio feedback slots.
    pub fn audio_feedback(&self) -> &[FeedbackSpec<AudioFeedbackKey, AudioSlotId>] {
        self.program.audio_feedback.as_slice()
    }

    /// The event feedback slots.
    pub fn event_feedback(&self) -> &[FeedbackSpec<EventFeedbackKey, EventSlotId>] {
        self.program.event_feedback.as_slice()
    }

    /// Every PDC delay of audio.
    pub fn audio_delays(&self) -> &[AudioDelay] {
        self.program.audio_delays.as_slice()
    }

    /// Every PDC delay of events.
    pub fn event_delays(&self) -> &[EventDelay] {
        self.program.event_delays.as_slice()
    }

    /// Every node, in key order.
    pub fn units(&self) -> &[PlanUnit] {
        self.units.as_slice()
    }

    /// Main input width.
    pub fn main_inputs(&self) -> u16 {
        self.main_inputs
    }

    /// The generation of the main output this plan renders into: what the engine
    /// looks its output folds up by.
    pub fn main_output_id(&self) -> crate::MainOutputId {
        self.main_output_id
    }

    /// Main output width: the main output's channels.
    pub fn main_outputs(&self) -> usize {
        self.compensation.len()
    }

    /// The output channels the plan renders: [`main_outputs`](Self::main_outputs)
    /// and, during a main output change's crossfade, the old main output's channels after
    /// them ([`main_output_bank`](Self::main_output_bank)). The slices a render is handed.
    pub fn rendered_outputs(&self) -> usize {
        self.rendered_outputs
    }

    /// The old main output this plan renders beside its own while the main output change
    /// that made it crossfades: its channels are
    /// `main_outputs()..rendered_outputs()`.
    pub fn main_output_bank(&self) -> Option<crate::MainOutputBank> {
        self.main_output_bank
    }

    /// Per output channel, how far a source *outside* the graph must pre-roll
    /// to stay aligned with the slowest channel — the same figure
    /// `crate::Compensation::for_channel` reports.
    pub fn compensation(&self) -> &[Samples] {
        &self.compensation
    }

    /// Worst-case latency across all outputs.
    pub fn total_latency(&self) -> Latency {
        self.total_latency
    }

    /// Every audio value: the buffers the plan moves, each with its slot,
    /// writer and readers.
    pub fn audio_values(&self) -> &[Value<AudioSlotId>] {
        self.program.audio_values.as_slice()
    }

    /// Every event value.
    pub fn event_values(&self) -> &[Value<EventSlotId>] {
        self.program.event_values.as_slice()
    }

    /// The flattened reader lists [`Value::readers`] spans index.
    pub fn value_readers(&self) -> &[OpId] {
        &self.program.value_readers
    }

    /// Every modulated param port, grouped by node op (the `params` span of
    /// [`Op::Node`] indexes this).
    pub fn param_ports(&self) -> &[ParamPortOp<EventSlotId>] {
        &self.program.param_ports
    }

    /// The flattened driven lanes of every [`Op::WeightedSum`]: its `gains`
    /// span indexes this.
    pub fn gain_ports(&self) -> &[GainPortOp<EventSlotId>] {
        &self.program.gain_ports
    }

    /// The unit compiled for `key`.
    pub fn unit(&self, key: NodeKey) -> Option<&PlanUnit> {
        self.unit_id(key).map(|id| &self.units[id])
    }

    /// Where the unit compiled for `key` sits in [`units`](Self::units).
    pub(crate) fn unit_id(&self, key: NodeKey) -> Option<PlanUnitId> {
        self.units.search_by_key(&key, |u| u.key)
    }

    /// The PDC delay of audio on `key`, or zero.
    pub fn audio_delay(&self, key: AudioDelayKey) -> Samples {
        self.program
            .audio_delays
            .iter()
            .find(|d| d.key == key)
            .map_or(Samples::ZERO, |d| d.len)
    }

    /// Which channels of `key` were aliased in place.
    pub fn in_place(&self, key: NodeKey) -> InPlaceMask {
        self.unit_id(key)
            .map_or(InPlaceMask::NONE, |unit| self.ports[unit].in_place)
    }
}

/// Which plan a [`Delta`] was compiled against: minted once for each plan, from
/// one counter, so two plans never share one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanId(u64);

impl PlanId {
    fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A plan's [`PlanId`], which is not part of what the plan says: two plans
/// compiled alike are equal, whichever of them was compiled first.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Identity(pub(crate) PlanId);

impl PartialEq for Identity {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// A node whose audio wiring changed in a commit: a port gained its first
/// source or consumer, or lost its last.
///
/// From the plans, not the spec: the old and the new plan's
/// [`PlanUnit::wired`](crate::PlanUnit::wired) for the key differ. A node
/// the commit placed with no wiring, a node it removed and a rewire that
/// leaves each port's answer as it was are not listed. A re-insert at the
/// same key with a different shape but the same masks reports nothing: bus
/// layout changes reach a plugin through its own re-activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rewired {
    /// The node.
    pub node: NodeKey,
    /// Its wired ports in the plan sent before (none for a node new to it).
    pub was: Wired,
    /// Its wired ports in the plan this commit sent.
    pub now: Wired,
}

/// Where one unit goes in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    /// The node.
    pub key: NodeKey,
    /// Its unit generation.
    pub generation: u32,
    /// Its store index.
    pub idx: UnitIdx,
}

/// What a runtime must change to go from the previous plan to this one.
///
/// Edits ship as deltas, O(changed) — never a copy of every unit.
/// `compile` is pure, so this lists *placements*; the boxes themselves are
/// attached on the control side (see `Editor::commit`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    insert: Vec<Placement>,
    retire: Vec<Placement>,
    replace: Vec<(Placement, Placement)>,
    fades: Vec<(NodeKey, Fade)>,
    cuts: Vec<Placement>,
    store_len: u32,
    /// The nodes whose wiring moved, in key order.
    rewired: Vec<Rewired>,
    /// What the plan takes over from the one it replaces.
    pub(crate) carry: Carry,
}

impl Delta {
    /// The delta `compile` found: the placements and what carries.
    pub(super) fn new(
        insert: Vec<Placement>,
        retire: Vec<Placement>,
        replace: Vec<(Placement, Placement)>,
        store_len: u32,
        rewired: Vec<Rewired>,
        carry: Carry,
    ) -> Self {
        Self {
            insert,
            retire,
            replace,
            fades: Vec::new(),
            cuts: Vec::new(),
            store_len,
            rewired,
            carry,
        }
    }

    /// The nodes whose wiring moved, in key order: the commit's report.
    pub(crate) fn take_rewired(&mut self) -> Vec<Rewired> {
        std::mem::take(&mut self.rewired)
    }

    /// New keys.
    #[must_use]
    pub fn insert(&self) -> &[Placement] {
        &self.insert
    }

    /// Keys that are gone. Their units come back to the control thread.
    #[must_use]
    pub fn retire(&self) -> &[Placement] {
        &self.retire
    }

    /// Keys whose generation changed: `(old, new)`, same index.
    #[must_use]
    pub fn replace(&self) -> &[(Placement, Placement)] {
        &self.replace
    }

    /// Replaces that crossfade rather than swap: each names a key in
    /// [`replace`](Self::replace), at most once. `compile` never fills this;
    /// the editor attaches it (see [`Editor::replace`](crate::Editor::replace)),
    /// and [`Plan::verify_fades`] checks it.
    #[must_use]
    pub fn fades(&self) -> &[(NodeKey, Fade)] {
        &self.fades
    }

    /// Kept units whose crossfade, running or waiting, is **cut**: every
    /// unit at the key but the newest retires. A hard edit that is not a new
    /// generation ([`Editor::set_latency`](crate::Editor::set_latency))
    /// attaches it. `compile` never fills it; [`Plan::verify_fades`] checks it.
    #[must_use]
    pub fn cuts(&self) -> &[Placement] {
        &self.cuts
    }

    /// The store length the new plan needs.
    #[must_use]
    pub fn store_len(&self) -> u32 {
        self.store_len
    }

    /// Sets the [`fades`](Self::fades) and [`cuts`](Self::cuts) of a delta
    /// `compile` found, replacing any set before. [`Editor::package`](crate::Editor::package)
    /// verifies them.
    pub fn attach(&mut self, fades: Vec<(NodeKey, Fade)>, cuts: Vec<Placement>) {
        self.fades = fades;
        self.cuts = cuts;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every stereo shape, over each way its slots can fall: a lowered
    /// direct form lists its distinct slots ascending, gives each output
    /// channel exactly one entry holding that channel's slot, and sends each
    /// input channel to a read entry holding its slot (or marks it in place).
    /// The expected table is read off the ports, not off `Direct::lower`.
    #[test]
    fn a_direct_lowering_borrows_exactly_the_ports_buffers_in_ascending_slot_order() {
        const OUT: [[u32; 2]; 4] = [[10, 20], [20, 10], [30, 5], [7, 8]];
        const IN: [[u32; 2]; 4] = [[50, 60], [60, 50], [50, 50], [4, 40]];
        let slot = |s: u32| crate::compile::ids::id_at::<AudioSlotId>(s as usize);
        let mut lowered = 0;
        for (ins, outs) in Direct::SHAPES {
            for out in OUT.map(|o| o.map(slot)) {
                for input in IN.map(|i| i.map(slot)) {
                    for mask in 0..1u8 << ins {
                        let mut ain = input[..ins].to_vec();
                        let mut in_place = InPlaceMask::NONE;
                        for c in (0..ins).filter(|&c| mask >> c & 1 == 1) {
                            // An in-place channel reads the slot it writes.
                            if c >= outs {
                                continue;
                            }
                            ain[c] = out[c];
                            in_place = in_place.with(c);
                        }
                        let aout = &out[..outs];
                        let d = Direct::lower(&ain, aout, in_place).expect("a stereo shape");
                        lowered += 1;
                        let count = usize::from(d.count);
                        let (slots, role) = (&d.slots[..count], &d.role[..count]);
                        assert!(slots.windows(2).all(|w| w[0] < w[1]), "{d:?}");
                        for (c, &s) in aout.iter().enumerate() {
                            let mine: Vec<usize> = (0..count)
                                .filter(|&i| role[i] == DirectRole::Out(c as u8))
                                .collect();
                            assert_eq!(mine.len(), 1, "{d:?}");
                            assert_eq!(slots[mine[0]], s, "{d:?}");
                        }
                        let mut reads: Vec<AudioSlotId> = ain
                            .iter()
                            .enumerate()
                            .filter(|&(c, _)| !in_place.get(c))
                            .map(|(_, &s)| s)
                            .collect();
                        reads.sort_unstable();
                        reads.dedup();
                        let read_slots: Vec<AudioSlotId> = (0..count)
                            .filter(|&i| role[i] == DirectRole::Read)
                            .map(|i| slots[i])
                            .collect();
                        assert_eq!(read_slots, reads, "no read is missing or extra: {d:?}");
                        for (c, &s) in ain.iter().enumerate() {
                            if in_place.get(c) {
                                assert_eq!(d.input[c], None, "{d:?}");
                            } else {
                                let r = usize::from(d.input[c].expect("a read input"));
                                assert_eq!((role[r], slots[r]), (DirectRole::Read, s), "{d:?}");
                            }
                        }
                    }
                }
            }
        }
        assert!(lowered > 100, "the enumeration ran: {lowered}");
        assert_eq!(
            Direct::lower(&[1, 2, 3].map(slot), &[4, 5].map(slot), InPlaceMask::NONE),
            None
        );
    }

    /// Every unit's row of `Plan::ports` carries the shape's port counts, the
    /// op's sorted borrow runs, and the form fixed for its node. A table row
    /// is one node: a source, an in-place node, a split, an event source and
    /// sink (the general form), a stereo node (direct) and a three-channel
    /// one (the audio walk). The expected values are read off the shapes and
    /// the plan's ops, never off the lowering.
    #[test]
    fn every_unit_lowers_to_the_form_and_borrows_its_ports_name() {
        use crate::NodeKey;
        use crate::compile::{Shapes, compile};
        use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort, GraphSpec};
        use crate::node::shape::Shape;
        use crate::{NodeSpec, OutputPort, PortSource, Topology};
        use tutti_buffers::ChannelLayout;
        use tutti_units::{Latency, Samples, Tail};
        let mono = ChannelLayout::MONO;
        let latent = Latency::new(Samples::new(4));
        let wide = ChannelLayout::from_count(3);
        // (key, shape, the audio source feeding it, if any).
        let rows = [
            (
                1,
                Shape::source_unassigned(mono)
                    .latency(latent)
                    .tail(Tail::Finite(Samples::new(9))),
                None,
            ),
            (
                2,
                Shape::through_unassigned(mono)
                    .in_place()
                    .tail(Tail::Unbounded),
                Some(1),
            ),
            (3, Shape::through_unassigned(mono), Some(2)),
            (
                4,
                Shape::EMPTY
                    .events(0, 1)
                    .event_capacity(crate::testing::cap_of(5)),
                None,
            ),
            (5, Shape::source_unassigned(mono).events(1, 0), None),
            (6, Shape::source_unassigned(ChannelLayout::STEREO), None),
            (7, Shape::through_unassigned(ChannelLayout::STEREO), Some(6)),
            (8, Shape::source_unassigned(wide), None),
            (9, Shape::through_unassigned(wide), Some(8)),
        ];
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        for &(k, shape, from) in &rows {
            let key = NodeKey(k);
            let mut spec = NodeSpec::new("n", shape.audio_in(), shape.audio_out());
            spec.latency = shape.latency.samples();
            spec.tail = shape.tail;
            t.nodes.insert(key, spec);
            shapes.insert(key, shape);
            for port in 0..shape.audio_in().count() {
                let from = NodeKey(from.expect("a node with inputs is fed"));
                t.edges.insert(
                    crate::InputPort::new(key, port),
                    vec![crate::Edge::Direct(PortSource::Node(OutputPort::new(
                        from, port,
                    )))],
                );
            }
        }
        t.set_outputs(
            [3, 5, 7, 7, 9, 9, 9]
                .into_iter()
                .zip([0, 0, 0, 1, 0, 1, 2])
                .map(|(k, p)| PortSource::Node(OutputPort::new(NodeKey(k), p)))
                .collect::<Vec<_>>(),
        );
        let mut g = GraphSpec::new(t);
        g.connect_events(
            EventInputPort::new(NodeKey(5), 0),
            EventEdge::Direct(EventOutputPort::new(NodeKey(4), 0)),
        );
        let prep = crate::node::Prepare::new(
            tutti_units::SampleRate::SR_48K,
            tutti_buffers::MaxBlock::of(Samples::new(64)),
        );
        let plan = compile(&g.validate().expect("valid"), &shapes, &prep, None)
            .expect("compiles")
            .0;

        assert_eq!(plan.ports.len(), plan.units.len(), "a row a unit");
        assert_eq!(plan.units.len(), rows.len());
        let mut forms = Vec::new();
        for op in &plan.program.ops {
            let Op::Node {
                unit,
                audio_in,
                audio_out,
                event_src,
                event_in,
                event_out,
                in_place,
                ..
            } = *op
            else {
                continue;
            };
            let rec = &plan.ports[unit];
            let pu = &plan.units[unit];
            let shape = shapes[&pu.key];
            let at = format!("node {:?}", pu.key);
            assert_eq!(
                in_place.get(0),
                pu.key == NodeKey(2),
                "only the in-place node is aliased: {at}"
            );

            let ain = &plan.program.audio_list[audio_in.range()];
            let aout = &plan.program.audio_list[audio_out.range()];
            let ein = &plan.program.event_list[event_in.range()];
            let esrc = &plan.program.event_list[event_src.range()];
            let eout = &plan.program.event_list[event_out.range()];
            let counts = [rec.ain, rec.aout, rec.ein, rec.eout].map(usize::from);
            assert_eq!(
                counts,
                [
                    usize::from(shape.audio_in().count()),
                    usize::from(shape.audio_out().count()),
                    usize::from(shape.event_in),
                    usize::from(shape.event_out)
                ],
                "{at}"
            );
            assert_eq!(
                counts,
                [ain.len(), aout.len(), ein.len(), eout.len()],
                "{at}"
            );

            // The requests the op makes, each run sorted.
            fn sorted<S: Ord>(mut v: Vec<(S, Role)>) -> Vec<(S, Role)> {
                v.sort_unstable();
                v
            }
            fn reads<S: Copy>(slots: &[S], skip: InPlaceMask) -> Vec<(S, Role)> {
                slots
                    .iter()
                    .enumerate()
                    .filter(|&(c, _)| !skip.get(c))
                    .map(|(c, &s)| (s, Role::Read(c as u8)))
                    .collect()
            }
            fn writes<S: Copy>(slots: &[S]) -> Vec<(S, Role)> {
                slots
                    .iter()
                    .enumerate()
                    .map(|(c, &s)| (s, Role::Write(c as u8)))
                    .collect()
            }
            assert_eq!(
                plan.nodes.audio_borrows[rec.borrows.range()],
                sorted([reads(ain, in_place), writes(aout)].concat()),
                "{at}"
            );
            assert_eq!(
                plan.nodes.event_borrows[rec.event_borrows.range()],
                sorted([reads(ein, InPlaceMask::NONE), writes(eout)].concat()),
                "{at}"
            );
            assert_eq!(
                plan.nodes.event_assemble[rec.event_assemble.range()],
                sorted([reads(esrc, InPlaceMask::NONE), writes(ein)].concat()),
                "{at}"
            );

            let width = (ain.len(), aout.len());
            let want = match pu.key.0 {
                1 => "Source",
                2 => "InPlace",
                3 => "Split",
                4 | 5 => "General",
                6 | 7 => "Direct",
                8 | 9 => "Audio",
                k => unreachable!("node {k}"),
            };
            let got = match rec.form {
                Form::General => "General",
                Form::Source { out } => {
                    assert_eq!(aout, [out], "{at}");
                    "Source"
                }
                Form::Split { input, out } => {
                    assert_eq!((ain, aout), (&[input][..], &[out][..]), "{at}");
                    assert_ne!(input, out, "a split never reads and writes one slot");
                    "Split"
                }
                Form::InPlace { slot } => {
                    assert_eq!((ain, aout), (&[slot][..], &[slot][..]), "{at}");
                    "InPlace"
                }
                Form::Direct(d) => {
                    assert_eq!((usize::from(d.ins), usize::from(d.outs)), width, "{at}");
                    "Direct"
                }
                Form::Audio => "Audio",
            };
            assert_eq!(got, want, "{at}");
            forms.push(got);
        }
        forms.sort_unstable();
        forms.dedup();
        assert_eq!(
            forms,
            ["Audio", "Direct", "General", "InPlace", "Source", "Split"],
            "the table holds a node of each form"
        );
    }
}
