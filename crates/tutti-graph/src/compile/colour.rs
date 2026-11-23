//! Buffer colouring over the **partial** order (compiler step 6).
//!
//! Serial liveness colouring — "a slot is free once its last reader in the
//! serial order has run" (JUCE) — is wrong the moment two ops can run at once:
//! the serial order says reader `r` ran before writer `w`, but a parallel
//! executor may run them together, and `w` clobbers what `r` is reading.
//!
//! The rule here is stated over the op DAG instead. A value occupies its slot
//! from its writer to its last reader. Value `u` may hand its slot to a later
//! value `v` only if **every op touching `u` happens-before `v`'s writer** —
//! reachability, not position — so no schedule consistent with the DAG can
//! overlap them. Reachability is a bitset per op, which is `n²/64` words:
//! about 16 k for a thousand ops, well inside what a control-thread recompile
//! can afford.
//!
//! **In-place aliasing** is the one exception: an op may write `v` into `u`'s
//! slot when it is itself a reader of `u` and every *other* reader of `u`
//! happens-before it. The op reads before it writes, so nothing is lost —
//! which is why it is offered only to ops that promise to (PDC delay rings,
//! and nodes whose `Shape::in_place` is set).

use super::emit::{ARef, ERef, Emitted, Pre, Val};
use super::ids::{
    AudioSlotId, AudioValueId, EventSlotId, EventValueId, Id, OpId, Table, TableSlice,
};
use super::plan::{EMPTY_SLOT, EventSlotCapacity, FeedbackKey, ZERO_SLOT};
use super::shapes::Units;
use crate::model::spec::{EventOutputPort, ValidGraph};
use crate::node::Prepare;

/// Strict reachability over the op DAG.
pub(crate) struct Reach {
    words: usize,
    bits: Vec<u64>,
}

impl Reach {
    /// `succ[i]` are op `i`'s successors; every edge must point forward.
    pub(crate) fn new(succ: &TableSlice<OpId, Vec<OpId>>) -> Self {
        let n = succ.len();
        let words = n.div_ceil(64);
        let mut bits = vec![0u64; n * words];
        for a in succ.ids().rev() {
            let a = a.index();
            for b in succ[OpId::from_index(a)].iter().map(|b| b.index()) {
                debug_assert!(b > a, "op DAG edges point forward");
                bits[a * words + b / 64] |= 1 << (b % 64);
                for w in 0..words {
                    let from_b = bits[b * words + w];
                    bits[a * words + w] |= from_b;
                }
            }
        }
        Self { words, bits }
    }

    /// Whether `a` happens-before `b` (strictly: `a != b` and a path exists).
    pub(crate) fn before(&self, a: OpId, b: OpId) -> bool {
        let (a, b) = (a.index(), b.index());
        (self.bits[a * self.words + b / 64] >> (b % 64)) & 1 == 1
    }
}

/// A colouring: slot per value, and how many slots.
pub(crate) struct Colouring<V> {
    pub(crate) slot: Table<V, usize>,
    pub(crate) count: usize,
}

/// The colouring pass's result: the op successors, the slot of every value and
/// the capacity of every event slot.
///
/// Slot 0 of each arena is the null slot and the feedback slots follow it, so
/// every value's slot is past them. Two values share a slot only when every op
/// touching the first happens-before the second's writer (or the second is an
/// in-place overwrite of it), whatever parallel schedule runs the DAG.
pub(super) struct Coloured {
    /// The ops that read what each op writes; a plan's `op_succ`.
    pub(super) succ: Table<OpId, Vec<OpId>>,
    audio_slot: Table<AudioValueId, AudioSlotId>,
    event_slot: Table<EventValueId, EventSlotId>,
    /// The size of the audio arena.
    pub(super) audio_slots: AudioSlotId,
    /// The size of the event arena.
    pub(super) event_slots: EventSlotId,
    /// What each event slot holds at most, by slot.
    pub(super) event_slot_capacity: Table<EventSlotId, EventSlotCapacity>,
}

impl Coloured {
    /// The slot an audio value is written to.
    pub(super) fn aslot(&self, v: AudioValueId) -> AudioSlotId {
        self.audio_slot[v]
    }

    /// The slot an event value is written to.
    pub(super) fn eslot(&self, v: EventValueId) -> EventSlotId {
        self.event_slot[v]
    }

    /// The slot an audio input reads.
    pub(super) fn aref_slot(&self, r: ARef) -> AudioSlotId {
        match r {
            ARef::Val(v) => self.aslot(v),
            ARef::Zero => ZERO_SLOT,
            ARef::Fb(f) => f.slot(),
        }
    }

    /// The slot an event input reads.
    pub(super) fn eref_slot(&self, r: ERef) -> EventSlotId {
        match r {
            ERef::Val(v) => self.eslot(v),
            ERef::Empty => EMPTY_SLOT,
            ERef::Fb(f) => f.slot(),
        }
    }
}

/// The audio values an op may write over: `Some(u)` offers a value the slot
/// of `u`. Delay rings, a sum's first addend and the ports of an in-place
/// node qualify; the colouring takes each offer only when it is legal.
fn in_place_offers(emitted: &Emitted, units: &Units) -> Table<AudioValueId, Option<AudioValueId>> {
    let mut offers = Table::filled(emitted.audio.len(), None);
    for op in &emitted.pre {
        match op {
            Pre::Delay { src, dst, .. } => offers[*dst] = Some(*src),
            // The sum accumulates into its first addend's slot when that
            // addend is read nowhere else later (the colouring's own test).
            Pre::Sum { srcs, dst } | Pre::WeightedSum { srcs, dst, .. } => {
                if let Some(&ARef::Val(u)) = srcs.first() {
                    offers[*dst] = Some(u);
                }
            }
            Pre::Node {
                unit, ain, aout, ..
            } if units.shapes[*unit].in_place => {
                for (&r, &out) in ain.iter().zip(aout) {
                    let ARef::Val(u) = r else { continue };
                    // Read by this node on exactly one port, or the other
                    // port would see the output instead of the input.
                    if ain.iter().filter(|&&x| x == r).count() == 1 {
                        offers[out] = Some(u);
                    }
                }
            }
            _ => {}
        }
    }
    offers
}

/// What a feedback slot of the event arena holds: all its FIFO can hold at its
/// source's declared rate.
fn feedback_capacity(units: &Units, key: &FeedbackKey, block: usize) -> EventSlotCapacity {
    match *key {
        FeedbackKey::Event { from, delay, .. } => {
            EventSlotCapacity::fifo(units.shape_of(from.node).event_capacity, delay, block)
        }
        FeedbackKey::Audio { .. } => unreachable!("event feedback keys are events"),
    }
}

/// The capacity of every event value, in value order: a node output holds
/// what its shape declares, a delay output all its FIFO can hold at its
/// source's declared rate (what can fall due in one block), and a merge all
/// its inputs together, so a merge can never drop an event (a note-off least
/// of all). Emission is in topological order, so a merge's inputs are priced
/// first.
fn event_value_capacities(
    graph: &ValidGraph,
    units: &Units,
    block: usize,
    emitted: &Emitted,
) -> Table<EventValueId, EventSlotCapacity> {
    let mut cap: Table<EventValueId, EventSlotCapacity> =
        Table::filled(emitted.event.len(), EventSlotCapacity::NONE);
    for op in &emitted.pre {
        match op {
            Pre::Node { unit, eout, .. } => {
                let port = EventSlotCapacity::port(units.shapes[*unit].event_capacity);
                for &v in eout {
                    cap[v] = port;
                }
            }
            Pre::EventDelay { delay, dst, .. } => {
                let d = &emitted.delays[*delay];
                let Some(from) = d.key.event_origin() else {
                    unreachable!("an event delay is keyed as events")
                };
                // An output's own capacity, or, for an input, every port with
                // an edge into it.
                let ports: Vec<EventOutputPort> = match from {
                    crate::EventTapPort::Out(port) => vec![port],
                    crate::EventTapPort::In(at) => graph
                        .events()
                        .get(&at)
                        .map(|edges| edges.iter().map(|e| e.from()).collect())
                        .unwrap_or_default(),
                };
                cap[*dst] = EventSlotCapacity::fifo_sum(
                    ports.iter().map(|p| units.shape_of(p.node).event_capacity),
                    d.len,
                    block,
                );
            }
            Pre::EventMerge { srcs, dst, bound } => {
                cap[*dst] = srcs
                    .iter()
                    .fold(EventSlotCapacity::NONE, |acc, r| {
                        acc.plus(match *r {
                            ERef::Val(v) => cap[v],
                            ERef::Fb(f) => {
                                feedback_capacity(units, &emitted.event_feedback[f], block)
                            }
                            ERef::Empty => EventSlotCapacity::NONE,
                        })
                    })
                    .bounded(bound.map(|b| b.capacity));
            }
            _ => {}
        }
    }
    cap
}

/// Assigns every value a slot and every event slot a capacity.
///
/// Takes the graph, the checked units, the `Prepare` the plan is for and the
/// [`Emitted`] ops; returns the [`Coloured`] slots. Slots are coloured over
/// the op DAG's reachability, with in-place overwrites where legal, and past
/// the null and feedback slots of each arena. An event slot's capacity covers
/// every value that shares it.
pub(super) fn assign(
    graph: &ValidGraph,
    units: &Units,
    prepare: &Prepare,
    emitted: &Emitted,
) -> Coloured {
    let mut succ: Table<OpId, Vec<OpId>> = Table::filled(emitted.pre.len(), Vec::new());
    for (op, ps) in emitted.preds.iter_enumerated() {
        for &p in ps {
            debug_assert!(p < op, "ops are emitted in topological order");
            succ[p].push(op);
        }
    }
    let reach = Reach::new(&succ);
    let audio = colour(&emitted.audio, &reach, &in_place_offers(emitted, units));
    let event = colour(
        &emitted.event,
        &reach,
        &Table::filled(emitted.event.len(), None),
    );
    // Slot 0 of each arena is the null slot, then one slot per feedback.
    let audio_fixed = 1 + emitted.audio_feedback.len();
    let event_fixed = 1 + emitted.event_feedback.len();
    let audio_slot: Table<AudioValueId, AudioSlotId> = audio
        .slot
        .iter()
        .map(|&s| AudioSlotId::from_index(audio_fixed + s))
        .collect();
    let event_slot: Table<EventValueId, EventSlotId> = event
        .slot
        .iter()
        .map(|&s| EventSlotId::from_index(event_fixed + s))
        .collect();
    let block = prepare.max_block().get();
    let mut event_slot_capacity: Table<EventSlotId, EventSlotCapacity> =
        Table::filled(event_fixed + event.count, EventSlotCapacity::NONE);
    for (f, key) in emitted.event_feedback.iter_enumerated() {
        event_slot_capacity[f.slot()] = feedback_capacity(units, key, block);
    }
    for (v, &c) in event_value_capacities(graph, units, block, emitted).iter_enumerated() {
        let s = event_slot[v];
        event_slot_capacity[s] = event_slot_capacity[s].covering(c);
    }
    Coloured {
        succ,
        audio_slot,
        event_slot,
        audio_slots: AudioSlotId::from_index(audio_fixed + audio.count),
        event_slots: EventSlotId::from_index(event_fixed + event.count),
        event_slot_capacity,
    }
}

/// Whether every op touching `u` happens-before `w`.
fn finished_before(u: &Val, w: OpId, reach: &Reach) -> bool {
    reach.before(u.writer, w) && u.readers.iter().all(|&r| reach.before(r, w))
}

/// Greedy colouring in writer order. `in_place[v] = Some(u)` offers `v` the
/// slot of `u`, taken when legal.
pub(crate) fn colour<V: Id>(
    vals: &TableSlice<V, Val>,
    reach: &Reach,
    in_place: &TableSlice<V, Option<V>>,
) -> Colouring<V> {
    let mut slot: Table<V, usize> = Table::filled(vals.len(), usize::MAX);
    // Last occupant of each slot. Values are visited in writer order, and a
    // value can only follow an occupant whose every op happens-before its
    // writer — so the occupants of one slot form a chain, and checking the
    // last one checks them all.
    let mut last: Vec<V> = Vec::new();

    for (v, val) in vals.iter_enumerated() {
        debug_assert!(
            v.index() == 0 || vals[V::from_index(v.index() - 1)].writer <= val.writer,
            "values are created in writer order"
        );
        let w = val.writer;

        if let Some(u) = in_place[v] {
            let src = &vals[u];
            let s = slot[u];
            let legal = last[s] == u
                && src.readers.contains(&w)
                && src.readers.iter().all(|&r| r == w || reach.before(r, w));
            if legal {
                slot[v] = s;
                last[s] = v;
                continue;
            }
        }

        let reuse = last
            .iter()
            .position(|&occ| finished_before(&vals[occ], w, reach));
        let s = match reuse {
            Some(s) => s,
            None => {
                last.push(v);
                last.len() - 1
            }
        };
        slot[v] = s;
        last[s] = v;
    }

    Colouring {
        count: last.len(),
        slot,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::compile::ids::AudioValueId;
    use proptest::prelude::*;

    /// One op per value: op `i` writes value `i` and reads the values in
    /// `reads[i]` (all earlier). Returns the values and the reach.
    fn dag(reads: &[Vec<u32>]) -> (Table<AudioValueId, Val>, Reach) {
        let mut vals: Table<AudioValueId, Val> = (0..reads.len() as u32)
            .map(|w| Val {
                writer: OpId::new(w),
                readers: Vec::new(),
            })
            .collect();
        let mut preds = vec![BTreeSet::new(); reads.len()];
        let mut succ: Table<OpId, Vec<OpId>> = Table::filled(reads.len(), Vec::new());
        for (op, rs) in reads.iter().enumerate() {
            for &u in rs {
                vals[AudioValueId::new(u)]
                    .readers
                    .push(OpId::from_index(op));
                if preds[op].insert(u) {
                    succ[OpId::new(u)].push(OpId::from_index(op));
                }
            }
        }
        let reach = Reach::new(&succ);
        (vals, reach)
    }

    fn no_in_place(len: usize) -> Table<AudioValueId, Option<AudioValueId>> {
        Table::filled(len, None)
    }

    fn slots(
        reads: &[Vec<u32>],
        in_place: &TableSlice<AudioValueId, Option<AudioValueId>>,
    ) -> usize {
        let (vals, reach) = dag(reads);
        colour(&vals, &reach, in_place).count
    }

    /// A chain `0 → 1 → 2 → 3 → 4` needs two slots: a value lives until its
    /// successor has written, so neighbours never share and every third value
    /// reuses the first slot.
    #[test]
    fn a_chain_needs_two_slots() {
        let reads = vec![vec![], vec![0], vec![1], vec![2], vec![3]];
        assert_eq!(slots(&reads, &no_in_place(reads.len())), 2);
    }

    /// A diamond `0 → {1, 2} → 3`: the two arms are unordered, so three
    /// values are live at once; the join reuses the source's slot.
    #[test]
    fn a_diamond_needs_three_slots() {
        let reads = vec![vec![], vec![0], vec![0], vec![1, 2]];
        assert_eq!(slots(&reads, &no_in_place(reads.len())), 3);
    }

    /// A source fanning out to four unordered ops that fan in: the source and
    /// all four arms are live while the arms write, and the join takes the
    /// source's slot, so five slots.
    #[test]
    fn a_fan_out_fan_in_needs_width_plus_one_slots() {
        let reads = vec![vec![], vec![0], vec![0], vec![0], vec![0], vec![1, 2, 3, 4]];
        assert_eq!(slots(&reads, &no_in_place(reads.len())), 5);
    }

    /// A chain whose every op overwrites its input in place lives in one slot.
    #[test]
    fn an_in_place_chain_needs_one_slot() {
        let reads = vec![vec![], vec![0], vec![1], vec![2], vec![3]];
        let in_place: Table<AudioValueId, Option<AudioValueId>> = (0..5)
            .map(|v: u32| v.checked_sub(1).map(AudioValueId::new))
            .collect();
        assert_eq!(slots(&reads, &in_place), 1);
    }

    fn random_reads(masks: &[u64]) -> Vec<Vec<u32>> {
        masks
            .iter()
            .enumerate()
            .map(|(i, &m)| {
                (0..i as u32)
                    .filter(|&u| (m >> (u % 64)) & 1 == 1)
                    .collect()
            })
            .collect()
    }

    /// Happens-before over the ops, by transitive closure of the `reads`
    /// edges: built here rather than through `Reach`, so the property below
    /// checks the colouring against a relation the code under test did not
    /// compute.
    fn closure(reads: &[Vec<u32>]) -> Vec<Vec<bool>> {
        let n = reads.len();
        let mut before = vec![vec![false; n]; n];
        for (op, rs) in reads.iter().enumerate() {
            for &u in rs {
                before[u as usize][op] = true;
            }
        }
        for k in 0..n {
            let through = before[k].clone();
            for row in &mut before {
                if row[k] {
                    for (cell, &on) in row.iter_mut().zip(&through) {
                        *cell |= on;
                    }
                }
            }
        }
        before
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(256), ..ProptestConfig::default() })]

        /// No two values with overlapping lifetimes share a slot, judged by a
        /// happens-before closure built from the edges alone:
        /// for two values in one slot, every op touching the earlier one
        /// happens-before the later one's writer, except the declared in-place
        /// hand-over (the writer is a reader of the earlier value and every
        /// other reader precedes it). The same graph colours identically
        /// twice.
        #[test]
        fn no_two_overlapping_values_share_a_slot(
            masks in prop::collection::vec(any::<u64>(), 2..40),
            offer in prop::collection::vec(any::<bool>(), 40),
        ) {
            let reads = random_reads(&masks);
            let (vals, reach) = dag(&reads);
            let before = closure(&reads);
            let in_place: Table<AudioValueId, Option<AudioValueId>> = reads
                .iter()
                .enumerate()
                .map(|(v, rs)| rs.first().filter(|_| offer[v]).map(|&u| AudioValueId::new(u)))
                .collect();
            let c = colour(&vals, &reach, &in_place);
            let again = colour(&vals, &reach, &in_place);
            prop_assert_eq!(&c.slot, &again.slot);
            prop_assert_eq!(c.count, again.count);
            prop_assert!(c.slot.iter().all(|&s| s < c.count));
            for v in vals.ids() {
                for u in vals.ids().take(v.index()) {
                    if c.slot[u] != c.slot[v] {
                        continue;
                    }
                    let w = vals[v].writer;
                    let src = &vals[u];
                    let precedes = |op: OpId| before[op.index()][w.index()];
                    let clear = precedes(src.writer) && src.readers.iter().all(|&r| precedes(r));
                    let hand_over = in_place[v] == Some(u)
                        && src.readers.contains(&w)
                        && precedes(src.writer)
                        && src.readers.iter().all(|&r| r == w || precedes(r));
                    prop_assert!(clear || hand_over, "values {u:?} and {v:?} overlap in slot {}", c.slot[u]);
                }
            }
        }
    }
}
