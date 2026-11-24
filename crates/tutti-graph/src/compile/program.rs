//! [`Program`]: the one description of what a plan runs, in the two forms the
//! compiler passes it through.
//!
//! `emit` writes a `Program<ByValue>`: ops name the values they write and read,
//! and no buffer exists yet. `group` prices and partitions those ops without
//! reading a slot. `colour` is the lowering: it gives every value a slot and
//! returns the `Program<BySlot>` a [`Plan`](crate::Plan) holds.
//!
//! The forms differ in three things, each an associated type of [`Refs`]: what
//! an op's operands and the flattened operand lists hold (a value reference or
//! a slot), the slot a value records (none, or its own), and the arena sizing
//! (how many events each value can hold, or each slot's capacity). The ops'
//! shape, the dependency rows and the keyed state tables are one definition.

use std::fmt::Debug;

use crate::EdgeId;
use crate::compile::ids::{
    AudioFeedbackId, AudioSlotId, AudioValueId, DelayId, EventFeedbackId, EventSlotId,
    EventValueId, OpId, PlanEventTapId, PlanMeterId, PlanTapId, Table,
};
use crate::compile::plan::{
    Csr, DelaySpec, EventSlotCapacity, FeedbackKey, FeedbackSpec, GainPortOp, Op, ParamPortOp,
    Value,
};

/// Where an audio operand reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ARef {
    /// An audio value an op writes.
    Val(AudioValueId),
    /// Silence: the null slot.
    Zero,
    /// A feedback read, filled before the block.
    Fb(AudioFeedbackId),
}

/// Where an event operand reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ERef {
    /// An event value an op writes.
    Val(EventValueId),
    /// No events: the empty slot.
    Empty,
    /// A feedback read, filled before the block.
    Fb(EventFeedbackId),
}

impl From<AudioValueId> for ARef {
    fn from(v: AudioValueId) -> Self {
        Self::Val(v)
    }
}

impl From<EventValueId> for ERef {
    fn from(v: EventValueId) -> Self {
        Self::Val(v)
    }
}

impl ARef {
    /// The value this reads, if it is one an op writes.
    pub(crate) fn value(self) -> Option<AudioValueId> {
        match self {
            Self::Val(v) => Some(v),
            Self::Zero | Self::Fb(_) => None,
        }
    }
}

impl ERef {
    /// The value this reads, if it is one an op writes.
    pub(crate) fn value(self) -> Option<EventValueId> {
        match self {
            Self::Val(v) => Some(v),
            Self::Empty | Self::Fb(_) => None,
        }
    }
}

/// How a [`Program`] names the buffers its ops touch: what the two forms of a
/// program differ in.
pub(crate) trait Refs {
    /// An op's audio operand.
    type Audio: Copy + Debug + PartialEq;
    /// An op's event operand.
    type Event: Copy + Debug + PartialEq;
    /// What an audio value records of its buffer.
    type AudioSlot: Copy + Debug + PartialEq;
    /// What an event value records of its buffer.
    type EventSlot: Copy + Debug + PartialEq;
    /// A row of the audio feedback table.
    type AudioFeedback: Copy + Debug + PartialEq;
    /// A row of the event feedback table.
    type EventFeedback: Copy + Debug + PartialEq;
    /// What the arena needs: per-value event capacities before colouring, the
    /// arena's slots after.
    type Sizing: Clone + Debug + PartialEq;
}

/// A program whose ops name values: what `emit` writes and `group` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ByValue;

/// A program whose ops name arena slots: what a plan runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BySlot;

/// How many events each event value and each event feedback must hold: what
/// `emit` knows from the ports, and `colour` folds into per-slot capacities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Capacities {
    /// Per event value.
    pub(crate) values: Table<EventValueId, EventSlotCapacity>,
    /// Per event feedback slot.
    pub(crate) feedback: Table<EventFeedbackId, EventSlotCapacity>,
}

/// The arenas a coloured program needs, one row per slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sizes {
    /// The audio slots: the null slot, the feedback slots, then the coloured
    /// ones.
    pub(crate) audio: Table<AudioSlotId, ()>,
    /// The event slots, each with how many events it holds per block.
    pub(crate) events: Table<EventSlotId, EventSlotCapacity>,
}

impl Refs for ByValue {
    type Audio = ARef;
    type Event = ERef;
    type AudioSlot = ();
    type EventSlot = ();
    type AudioFeedback = FeedbackKey;
    type EventFeedback = FeedbackKey;
    type Sizing = Capacities;
}

impl Refs for BySlot {
    type Audio = AudioSlotId;
    type Event = EventSlotId;
    type AudioSlot = AudioSlotId;
    type EventSlot = EventSlotId;
    type AudioFeedback = FeedbackSpec<AudioSlotId>;
    type EventFeedback = FeedbackSpec<EventSlotId>;
    type Sizing = Sizes;
}

/// What a plan runs, before and after its values get slots.
///
/// Every op's dependencies precede it (`deps` row `i` holds ids below `i`
/// only), every value's writer precedes its readers, and each keyed state
/// table is the one its ops index.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Program<S: Refs> {
    /// The ops, in the serial order.
    pub(crate) ops: Table<OpId, Op<S::Audio, S::Event>>,
    /// The audio operands lists the ops' spans index.
    pub(crate) audio_list: Vec<S::Audio>,
    /// The event operands lists the ops' spans index.
    pub(crate) event_list: Vec<S::Event>,
    /// The driven params of every node op, flattened.
    pub(crate) param_ports: Vec<ParamPortOp<S::Event>>,
    /// The driven lanes of every weighted sum, flattened.
    pub(crate) gain_ports: Vec<GainPortOp<S::Event>>,
    /// The lanes of every weighted sum, flattened.
    pub(crate) sum_lanes: Vec<Option<EdgeId>>,
    /// Every audio value.
    pub(crate) audio_values: Table<AudioValueId, Value<S::AudioSlot>>,
    /// Every event value.
    pub(crate) event_values: Table<EventValueId, Value<S::EventSlot>>,
    /// The readers of every value, flattened.
    pub(crate) value_readers: Vec<OpId>,
    /// The ops each op waits for: sorted, without duplicates.
    pub(crate) deps: Csr<OpId>,
    /// The PDC delays.
    pub(crate) delays: Table<DelayId, DelaySpec>,
    /// The audio feedback edges, ascending: slots after the null slot.
    pub(crate) audio_feedback: Table<AudioFeedbackId, S::AudioFeedback>,
    /// The event feedback edges, ascending.
    pub(crate) event_feedback: Table<EventFeedbackId, S::EventFeedback>,
    /// The taps the topology names.
    pub(crate) taps: Table<PlanTapId, crate::TapId>,
    /// The event taps the topology names.
    pub(crate) event_taps: Table<PlanEventTapId, crate::EventTapId>,
    /// The meters, with their lane counts.
    pub(crate) meters: Table<PlanMeterId, (crate::MeterId, usize)>,
    /// The arena sizing.
    pub(crate) sizing: S::Sizing,
}
