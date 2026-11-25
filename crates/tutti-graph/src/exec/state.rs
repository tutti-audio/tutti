//! What the executor holds for a plan: its units, delay rings and
//! injections, and the state a plan's ops run against.

use crate::node::shape::Shape;
use tutti_buffers::Float;
use tutti_rt::CappedVec;
use tutti_units::{Samples, Tail};

use super::commit::{Crossfade, NodeBox};
use super::node_op::Flags;
use super::tail_elapsed;
use crate::compile::ids::{Generation, PlanUnitId, Table};
use crate::compile::plan::{NodePorts, UnitIdx};
use crate::compile::tail::Owed;
use crate::edges::event_tap::EventTapWriter;
use crate::edges::meter::MeterWriter;
use crate::edges::tap::TapWriter;
use crate::event::types::Event;
use crate::exec::arena::Arena;
use crate::exec::held::{Keys, Tables};
use crate::exec::par::Par;
use crate::exec::rings::{AudioRing, EventFifo};
use crate::exec::slots::EventSlot;
use crate::model::gain::SumLane;
use crate::model::mode::ModeState;
use crate::node::param::ParamState;

pub(crate) struct Unit<F: Float = f32> {
    pub(super) generation: Generation,
    pub(super) node: NodeBox<F>,
    /// Consecutive frames of fully silent input, saturating.
    pub(super) quiet: Samples,
    /// Whether the last call (or skip) left every output silent and wrote no
    /// event.
    pub(super) last_quiet: bool,
    /// Whether the last call returned [`Status::Idle`](crate::Status::Idle).
    pub(super) last_idle: bool,
    /// The tail its last call narrowed its ceiling to
    /// ([`Io::report_tail`](crate::Io::report_tail), clamped to the
    /// ceiling); `None` is the ceiling. Written only by the task that runs
    /// the unit, so the skip it decides is a function of the unit's own
    /// inputs and reports.
    pub(super) reported: Option<Tail>,
    /// The crossfade running here: `node` is its incoming unit.
    pub(super) fade: Option<Box<Crossfade<F>>>,
    /// A crossfade waiting for `fade` to end.
    pub(super) queued: Option<Box<Crossfade<F>>>,
    /// Its declared params' modulation state and per-frame buffers.
    pub(super) params: ParamState<F>,
    /// The keys its event outputs hold or owe (`held.rs`).
    pub(super) keys: Keys,
    /// Its mode and the state that applies it (`mode.rs`).
    pub(super) mode: ModeState<F>,
}

impl<F: Float> Unit<F> {
    pub(super) fn new(
        generation: Generation,
        node: NodeBox<F>,
        params: ParamState<F>,
        mode: ModeState<F>,
        keys: Keys,
    ) -> Self {
        Self {
            generation,
            node,
            quiet: Samples::ZERO,
            last_quiet: false,
            last_idle: false,
            reported: None,
            fade: None,
            queued: None,
            params,
            keys,
            mode,
        }
    }
}

impl<F: Float> Unit<F> {
    /// Whether the executor skips this unit on a block whose inputs are
    /// silent: a node without event inputs when its last call left every
    /// output silent and its latency plus its tail (the last report, else the
    /// declared tail) has elapsed in quiet input; one with event inputs when
    /// its last call said [`Status::Idle`](crate::Status::Idle). A node
    /// with no audio inputs is never parked on its tail.
    pub(super) fn at_rest(&self, np: &NodePorts, shape: &Shape) -> bool {
        if np.ein == 0 {
            np.ain != 0
                && self.last_quiet
                && tail_elapsed(
                    match self.reported {
                        None => shape.park_after(),
                        Some(t) => Tail::Finite(shape.latency.samples()).then(t),
                    },
                    self.quiet,
                )
        } else {
            self.last_idle
        }
    }

    /// What this unit adds to the graph's tail between blocks.
    ///
    /// `fed` is its declared ceiling: a report is about the input already
    /// heard, so it says nothing of what an upstream node's remaining output
    /// will excite. `alone` is what it owes if nothing more arrives:
    ///
    /// - a node with event inputs: nothing once its last call said
    ///   [`Status::Idle`](crate::Status::Idle), else its ceiling;
    /// - a node with no audio inputs: its last report, else its ceiling;
    /// - otherwise nothing when [at rest](Self::at_rest), else the part of its
    ///   latency and report (else ceiling) that quiet input has not covered,
    ///   never more than the tail itself. `quiet` counts whole blocks of quiet
    ///   input, so the figure errs long by less than a block. A unit not at
    ///   rest whose figure is covered (its last output was not silent) owes the
    ///   whole tail.
    pub(super) fn owed(&self, np: &NodePorts, shape: &Shape) -> Owed {
        let full = self.reported.unwrap_or(shape.tail);
        let alone = if np.ein != 0 {
            if self.last_idle {
                Tail::None
            } else {
                shape.tail
            }
        } else if np.ain == 0 {
            full
        } else if self.at_rest(np, shape) {
            Tail::None
        } else {
            match full {
                Tail::Finite(t) => {
                    let left = (shape.latency.samples() + t)
                        .remaining_after(self.quiet)
                        .min(t);
                    Tail::Finite(if left == Samples::ZERO { t } else { left })
                }
                other => other,
            }
        };
        Owed {
            alone,
            fed: shape.tail,
        }
    }
}

/// What a commit takes away from the keys held at a node: the tables of the
/// units retired or replaced, by the store index they held.
#[derive(Default)]
pub(super) struct Gone {
    pub(super) held: CappedVec<(UnitIdx, Tables)>,
    /// Emptied tables to free on the control side.
    pub(super) spare: CappedVec<Tables>,
    /// What each replaced node wrote for its successor (`Node::chase`):
    /// the successor's row, its span of `chase_events` and what its buffer
    /// refused.
    pub(super) chased: CappedVec<(PlanUnitId, std::ops::Range<usize>, u32)>,
    /// The executor's chase buffer, lent for the commit.
    pub(super) chase_events: CappedVec<Event>,
    /// What a chase held for a successor behind a running fade refused,
    /// counted once the new plan is in place.
    pub(super) held_refused: CappedVec<(PlanUnitId, u32)>,
}

impl Gone {
    /// Items its lists refused: the ones the apply leaked.
    pub(super) fn refused(&self) -> usize {
        [
            self.held.refused(),
            self.spare.refused(),
            self.chased.refused(),
            self.held_refused.refused(),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }
}

/// Events flushed from a delay that went away, waiting for their sink's next
/// call, at their offsets relative to the earliest (see the module docs).
/// Built on the control side with room for everything that can flow in
/// (`stage.rs`); `live` is whether anything was flushed into it.
pub(crate) struct Inject {
    pub(crate) unit: PlanUnitId,
    pub(super) port: u16,
    pub(super) events: CappedVec<Event>,
    pub(super) live: bool,
    /// While a commit's flush is written: how many of `events` came before
    /// the closes of the edges it cut.
    pub(super) closed_from: usize,
    /// What the edges this commit cut held and `events` had no room for: one
    /// table for each edge, built with the commit. Drained into the sink's
    /// events at the end of each block, as much as the block takes, until
    /// empty; a table never overflows, so no note off is lost. With several
    /// cut edges into one sink each table drains in turn, its pedal up
    /// first.
    pub(super) owed: CappedVec<crate::exec::held::Held>,
}

impl Inject {
    /// Owe the sink the closes in `table`. It takes the slot made for it; a
    /// buffer with none free merges it into the first table instead, so no
    /// key is forgotten, and the table goes to `spare` to be freed on the
    /// control side. Nothing allocates or frees. Returns how many closes had
    /// nowhere to go (none: room is made for every table owed).
    pub(super) fn owe(
        &mut self,
        table: crate::exec::held::Held,
        spare: &mut CappedVec<crate::exec::held::Held>,
    ) -> u32 {
        // A table placed here always has a slot: staging counted every cut
        // edge into this sink port and every table that moves to it
        // (`Room::closers`, `Room::carried`), so the buffer has a table to
        // merge into whenever its slots are taken, and `owed.capacity() == 0`
        // is never reached with a table in hand.
        let mut lost = 0u32;
        let Err(left_over) = self.owed.push_within_capacity(table) else {
            return 0;
        };
        if let Some(first) = self.owed.first_mut() {
            first.merge(&left_over);
        } else {
            debug_assert!(false, "room was made for every table owed");
            left_over.close_each(|_| lost = lost.saturating_add(1));
        }
        // Room for every table is made with the commit; were it short, the
        // table leaks rather than frees on the audio thread.
        super::place(spare, left_over);
        lost
    }

    /// Whether closes are still owed to the sink.
    pub(super) fn draining(&self) -> bool {
        self.owed.iter().any(|h| !h.is_empty())
    }

    /// Appends what is owed to `events`, as far as it takes, at the offset of
    /// the last event it holds: after everything flushed to the sink, and
    /// ahead of what its sources write at that offset, as the closes of a cut
    /// edge are (a close never passes the note it closes).
    pub(super) fn drain_owed(&mut self) {
        let end = self
            .events
            .last()
            .map_or(crate::node::time::Offset::ZERO, |e| e.offset);
        let start = self.events.len();
        for h in self.owed.iter_mut() {
            h.drain_into(&mut self.events);
        }
        for e in &mut self.events[start..] {
            e.offset = end;
        }
    }

    /// What the sink's next call has consumed: the flushed events go, the
    /// sink stays live while closes are owed.
    pub(super) fn consumed(&mut self) {
        self.events.clear();
        self.live = self.draining();
    }

    /// The sink is parked: what was flushed and owed is discarded with the
    /// events it would have been sent.
    pub(super) fn discard(&mut self) {
        self.owed
            .iter_mut()
            .for_each(crate::exec::held::Held::clear);
        self.consumed();
    }
}

/// Everything the executor rebuilds when applying: the arenas, the delay and
/// feedback state, and pending injections. Retired into the commit box
/// whole, so the old one is freed on the control side.
pub(super) struct State<F: Float = f32> {
    pub(super) arena: Arena<F>,
    /// Per audio slot, what is known about its block: [`Flags`]. The parallel executor runs on a shared atomic copy (`par.rs`).
    pub(super) flags: Vec<Flags>,
    pub(super) events: Vec<EventSlot>,
    /// The PDC rings, by [`AudioDelayId`](crate::compile::ids::AudioDelayId).
    pub(super) audio_delays: Vec<Option<AudioRing<F>>>,
    /// The PDC FIFOs, by [`EventDelayId`](crate::compile::ids::EventDelayId).
    pub(super) event_delays: Vec<Option<EventFifo>>,
    pub(super) audio_fb: Vec<Option<AudioRing<F>>>,
    pub(super) event_fb: Vec<Option<EventFifo>>,
    /// One producer per planned tap lane (`None`: nobody opened it).
    pub(super) taps: Vec<Option<TapWriter<F>>>,
    /// One producer per planned event tap (`None`: nobody opened it).
    pub(super) event_taps: Vec<Option<EventTapWriter>>,
    /// One set of cells per planned meter (`None`: nobody opened it).
    pub(super) meters: Vec<Option<MeterWriter>>,
    /// One state per entry of the plan's sum lanes: a weighted lane's weight
    /// across blocks, or a plain addend's none.
    pub(super) lanes: Vec<SumLane<F>>,
    pub(super) inject: Vec<Inject>,
    pub(super) has_inject: Table<PlanUnitId, bool>,
    /// Plan units with a scheduled command landing this block.
    pub(super) has_due: Table<PlanUnitId, bool>,
    /// The executor's per-plan state for running a block's tasks on its
    /// participants (`par.rs`); `None` while there is no plan.
    pub(super) par: Option<Par>,
}

impl<F: Float> State<F> {
    pub(super) fn empty(max_block: usize) -> Self {
        Self {
            arena: Arena::new(1, max_block),
            flags: vec![Flags::SILENCE],
            events: Vec::new(),
            audio_delays: Vec::new(),
            event_delays: Vec::new(),
            audio_fb: Vec::new(),
            event_fb: Vec::new(),
            taps: Vec::new(),
            event_taps: Vec::new(),
            meters: Vec::new(),
            lanes: Vec::new(),
            inject: Vec::new(),
            has_inject: Table::default(),
            has_due: Table::default(),
            par: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::ids::id_at;
    use crate::event::types::Event;
    use crate::exec::held::Held;
    use crate::node::time::Offset;
    use tutti_fixtures::midi;
    use tutti_ump::{MidiChannel, U7};

    fn holding(note: u8) -> Held {
        let mut h = Held::new();
        let on = midi::m1_note_on(MidiChannel::FIRST, note, U7::of(100));
        h.observe(&[Event::midi(Offset::ZERO, on)]);
        h
    }

    fn inject(room: usize) -> Inject {
        Inject {
            unit: id_at(0),
            port: 0,
            events: CappedVec::default(),
            live: false,
            closed_from: 0,
            owed: CappedVec::with_capacity(room),
        }
    }

    /// A table goes into the slot made for it; one with no slot free merges
    /// into the first, so no key is forgotten and nothing allocates.
    #[test]
    fn a_table_with_no_slot_free_merges_into_the_first() {
        let mut i = inject(1);
        let mut spare = CappedVec::with_capacity(1);
        assert_eq!(i.owe(holding(60), &mut spare), 0);
        let capacity = i.owed.capacity();
        assert_eq!(i.owe(holding(61), &mut spare), 0, "merged, not lost");
        assert_eq!(spare.len(), 1, "the merged table goes back");
        assert_eq!(i.owed.len(), 1);
        assert_eq!(i.owed.capacity(), capacity, "and the buffer did not grow");
        let mut closes = Vec::new();
        i.owed[0].close_each(|e| closes.push(e));
        assert_eq!(closes.len(), 2, "both keys are owed");
    }
}
