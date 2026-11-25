//! Buffer colouring over the **partial** order (compiler step 7), and the
//! lowering it makes: a `Program<ByValue>` in, a `Program<BySlot>` out.
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
//!
//! The slots are laid out as the plan documents: the null slot, the feedback
//! slots, then the coloured ones. The same pass turns every operand of every
//! op into its value's slot, sizes the event slots from the capacities `emit`
//! priced, and records which node channels came out in place.

use super::ids::{
    AudioFeedbackId, AudioSlotId, AudioValueId, EventFeedbackId, EventSlotId, Id, OpId, Table,
    TableSlice,
};
use super::plan::{Csr, EventSlotCapacity, FeedbackSpec, GainPortOp, Op, ParamPortOp, Value};
use super::program::{ARef, BySlot, ByValue, ERef, Program, Sizes};
use super::shapes::Units;
use crate::node::InPlaceMask;

/// Strict reachability over the op DAG.
pub(crate) struct Reach {
    words: usize,
    /// Per op, the ops that happen before it.
    bits: Vec<u64>,
}

impl Reach {
    /// `deps` row `b` names the ops `b` waits for; every edge must point
    /// backward.
    pub(crate) fn new(deps: &Csr<OpId>) -> Self {
        let n = deps.rows();
        let words = n.div_ceil(64);
        let mut bits = vec![0u64; n * words];
        for b in 0..n {
            for a in deps.row(b).iter().map(|a| a.index()) {
                debug_assert!(a < b, "op DAG edges point forward");
                bits[b * words + a / 64] |= 1 << (a % 64);
                for w in 0..words {
                    let from_a = bits[a * words + w];
                    bits[b * words + w] |= from_a;
                }
            }
        }
        Self { words, bits }
    }

    /// Whether `a` happens-before `b` (strictly: `a != b` and a path exists).
    pub(crate) fn before(&self, a: OpId, b: OpId) -> bool {
        let (a, b) = (a.index(), b.index());
        (self.bits[b * self.words + a / 64] >> (a % 64)) & 1 == 1
    }
}

/// A colouring: slot per value, and how many slots.
pub(crate) struct Colouring<V> {
    pub(crate) slot: Table<V, usize>,
    pub(crate) count: usize,
}

/// The ops that touch one value: the one that writes it, and the ones that
/// read it.
pub(crate) struct Life<'a> {
    pub(crate) writer: OpId,
    pub(crate) readers: &'a [OpId],
}

/// Whether every op touching `u` happens-before `w`.
fn finished_before(u: &Life, w: OpId, reach: &Reach) -> bool {
    reach.before(u.writer, w) && u.readers.iter().all(|&r| reach.before(r, w))
}

/// Greedy colouring in writer order ([`colour`] lowers a whole program).
/// `in_place[v] = Some(u)` offers `v` the slot of `u`, taken when legal.
pub(crate) fn assign<V: Id>(
    lives: &TableSlice<V, Life>,
    reach: &Reach,
    in_place: &TableSlice<V, Option<V>>,
) -> Colouring<V> {
    let mut slot: Table<V, usize> = Table::filled(lives.len(), usize::MAX);
    // Last occupant of each slot. Values are visited in writer order, and a
    // value can only follow an occupant whose every op happens-before its
    // writer — so the occupants of one slot form a chain, and checking the
    // last one checks them all.
    let mut last: Vec<V> = Vec::new();

    for (v, life) in lives.iter_enumerated() {
        debug_assert!(
            v.index() == 0 || lives.as_slice()[v.index() - 1].writer <= life.writer,
            "values are created in writer order"
        );
        let w = life.writer;

        if let Some(u) = in_place[v] {
            let src = &lives[u];
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
            .position(|&occ| finished_before(&lives[occ], w, reach));
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

/// The lives of `values`, readers named by their spans of `readers`.
fn lives_of<'a, V: Id, S>(
    values: &TableSlice<V, Value<S>>,
    readers: &'a [OpId],
) -> Table<V, Life<'a>> {
    values
        .iter()
        .map(|v| Life {
            writer: v.writer,
            readers: &readers[v.readers.range()],
        })
        .collect()
}

/// `values` with the slot each was given: the coloured value's slot is the
/// `fixed` slots in front of the arena, then its colour.
fn slotted<V: Id, S: Id, R>(
    values: Table<V, Value<()>>,
    colouring: &Colouring<V>,
    fixed: usize,
    arena: &TableSlice<S, R>,
) -> Table<V, Value<S>> {
    values
        .into_iter()
        .zip(&colouring.slot)
        .map(|(v, &colour)| Value {
            slot: arena
                .id(fixed + colour)
                .expect("the arena holds every coloured slot"),
            writer: v.writer,
            readers: v.readers,
        })
        .collect()
}

/// Colours the values of `program` and names every operand by its slot.
///
/// Takes the program `emit` wrote and the units whose shapes say which nodes
/// run in place; returns the same program with slots: ops, lists and value
/// rows renamed, the node ops' in-place masks derived from the slots they
/// came out with, and the arenas sized.
pub(super) fn colour(program: Program<ByValue>, units: &Units) -> Program<BySlot> {
    let Program {
        ops,
        node_op,
        audio_list,
        event_list,
        param_ports,
        gain_ports,
        sum_lanes,
        audio_values,
        event_values,
        value_readers,
        deps,
        audio_delays,
        event_delays,
        audio_feedback,
        event_feedback,
        taps,
        event_taps,
        meters,
        sizing,
    } = program;
    let reach = Reach::new(&deps);

    // In-place candidates: (value written, value it may overwrite).
    let mut in_place: Table<AudioValueId, Option<AudioValueId>> =
        Table::filled(audio_values.len(), None);
    for op in &ops {
        match *op {
            Op::Delay { src, dst, .. } => {
                if let (Some(dst), Some(src)) = (dst.value(), src.value()) {
                    in_place[dst] = Some(src);
                }
            }
            // The sum accumulates into its first addend's slot when that
            // addend is read nowhere else later (the colouring's own test).
            Op::Sum { srcs, dst } | Op::WeightedSum { srcs, dst, .. } => {
                if let (Some(dst), Some(&ARef::Val(u))) =
                    (dst.value(), audio_list[srcs.range()].first())
                {
                    in_place[dst] = Some(u);
                }
            }
            Op::Node {
                unit,
                audio_in,
                audio_out,
                ..
            } if units.shapes[unit].in_place => {
                let ain = &audio_list[audio_in.range()];
                let aout = &audio_list[audio_out.range()];
                for (&r, &out) in ain.iter().zip(aout) {
                    let (ARef::Val(u), Some(out)) = (r, out.value()) else {
                        continue;
                    };
                    // Read by this node on exactly one port, or the other
                    // port would see the output instead of the input.
                    if ain.iter().filter(|&&x| x == r).count() != 1 {
                        continue;
                    }
                    in_place[out] = Some(u);
                }
            }
            _ => {}
        }
    }

    let audio_colour = assign(&lives_of(&audio_values, &value_readers), &reach, &in_place);
    let event_colour = assign(
        &lives_of(&event_values, &value_readers),
        &reach,
        &Table::filled(event_values.len(), None),
    );

    // Slot 0 of each arena is the null slot, then one slot per feedback.
    let audio_fixed = 1 + audio_feedback.len();
    let event_fixed = 1 + event_feedback.len();
    let audio_arena: Table<AudioSlotId, ()> = Table::filled(audio_fixed + audio_colour.count, ());
    let mut event_arena: Table<EventSlotId, EventSlotCapacity> =
        Table::filled(event_fixed + event_colour.count, EventSlotCapacity::NONE);
    let audio_feedback_at: Table<AudioFeedbackId, AudioSlotId> = audio_feedback
        .iter()
        .zip(audio_arena.ids().skip(1))
        .map(|(_, slot)| slot)
        .collect();
    let event_feedback_at: Table<EventFeedbackId, EventSlotId> = event_feedback
        .iter()
        .zip(event_arena.ids().skip(1))
        .map(|(_, slot)| slot)
        .collect();
    let audio_values = slotted(audio_values, &audio_colour, audio_fixed, &audio_arena);
    let event_values = slotted(event_values, &event_colour, event_fixed, &event_arena);

    // Event slot capacities: a feedback slot holds its FIFO, and a slot
    // shared by several values the most of each currency any of them needs.
    for (f, &cap) in sizing.feedback.iter_enumerated() {
        event_arena[event_feedback_at[f]] = cap;
    }
    for (v, &cap) in sizing.values.iter_enumerated() {
        let s = event_values[v].slot;
        event_arena[s] = event_arena[s].covering(cap);
    }

    let aref_slot = |r: ARef| match r {
        ARef::Val(v) => audio_values[v].slot,
        ARef::Zero => AudioSlotId::ZERO,
        ARef::Fb(f) => audio_feedback_at[f],
    };
    let eref_slot = |r: ERef| match r {
        ERef::Val(v) => event_values[v].slot,
        ERef::Empty => EventSlotId::EMPTY,
        ERef::Fb(f) => event_feedback_at[f],
    };
    let audio_list: Vec<AudioSlotId> = audio_list.iter().map(|&r| aref_slot(r)).collect();
    let event_list: Vec<EventSlotId> = event_list.iter().map(|&r| eref_slot(r)).collect();

    let ops: Table<OpId, Op<AudioSlotId, EventSlotId>> = ops
        .into_iter()
        .map(|op| match op.map(aref_slot, eref_slot) {
            // A channel is in place when its output came out in its input's
            // slot.
            Op::Node {
                unit,
                audio_in,
                audio_out,
                event_src,
                event_in,
                event_out,
                params,
                ..
            } => {
                let in_place = audio_list[audio_in.range()]
                    .iter()
                    .zip(&audio_list[audio_out.range()])
                    .enumerate()
                    .filter(|&(_, (input, output))| input == output)
                    .fold(InPlaceMask::NONE, |mask, (c, _)| mask.with(c));
                // Only a node that accepts in-place aliasing was offered it.
                debug_assert!(
                    in_place == InPlaceMask::NONE || units.shapes[unit].in_place,
                    "a node that does not alias in place shares an input and an output slot"
                );
                Op::Node {
                    unit,
                    audio_in,
                    audio_out,
                    event_src,
                    event_in,
                    event_out,
                    in_place,
                    params,
                }
            }
            op => op,
        })
        .collect();
    Program {
        ops,
        node_op,
        audio_list,
        event_list,
        param_ports: param_ports
            .into_iter()
            .map(|p| ParamPortOp {
                port: p.port,
                param: p.param,
                from: p.from,
                slot: eref_slot(p.slot),
            })
            .collect(),
        gain_ports: gain_ports
            .into_iter()
            .map(|g| GainPortOp {
                lane: g.lane,
                from: g.from,
                slot: eref_slot(g.slot),
            })
            .collect(),
        sum_lanes,
        audio_values,
        event_values,
        value_readers,
        deps,
        audio_delays,
        event_delays,
        audio_feedback: audio_feedback
            .into_iter()
            .zip(&audio_feedback_at)
            .map(|(key, &slot)| FeedbackSpec { key, slot })
            .collect(),
        event_feedback: event_feedback
            .into_iter()
            .zip(&event_feedback_at)
            .map(|(key, &slot)| FeedbackSpec { key, slot })
            .collect(),
        taps,
        event_taps,
        meters,
        sizing: Sizes {
            audio: audio_arena,
            events: event_arena,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use proptest::prelude::*;

    /// One op per value: op `i` writes value `i` and reads the values in
    /// `reads[i]` (all earlier).
    struct Dag {
        ops: Vec<OpId>,
        values: Vec<AudioValueId>,
        readers: Vec<Vec<OpId>>,
        reach: Reach,
    }

    impl Dag {
        fn new(reads: &[Vec<u32>]) -> Self {
            let ops: Vec<OpId> = Table::<OpId, ()>::filled(reads.len(), ()).ids().collect();
            let values: Vec<AudioValueId> = Table::<AudioValueId, ()>::filled(reads.len(), ())
                .ids()
                .collect();
            let mut readers = vec![Vec::new(); reads.len()];
            let mut deps: Table<OpId, Vec<OpId>> = Table::filled(reads.len(), Vec::new());
            for (op, rs) in reads.iter().enumerate() {
                let mut seen = BTreeSet::new();
                for &u in rs {
                    readers[u as usize].push(ops[op]);
                    if seen.insert(u) {
                        deps[ops[op]].push(ops[u as usize]);
                    }
                }
            }
            let reach = Reach::new(&Csr::from_table(&deps));
            Self {
                ops,
                values,
                readers,
                reach,
            }
        }

        fn lives(&self) -> Table<AudioValueId, Life<'_>> {
            self.ops
                .iter()
                .zip(&self.readers)
                .map(|(&writer, readers)| Life { writer, readers })
                .collect()
        }
    }

    /// The slots a `reads` graph needs, `in_place[v] = Some(u)` offering value
    /// `v` the slot of value `u` (both by position).
    fn slots(reads: &[Vec<u32>], in_place: &[Option<usize>]) -> usize {
        let dag = Dag::new(reads);
        let offers: Table<AudioValueId, Option<AudioValueId>> =
            in_place.iter().map(|u| u.map(|u| dag.values[u])).collect();
        assign(&dag.lives(), &dag.reach, &offers).count
    }

    /// A chain `0 → 1 → 2 → 3 → 4` needs two slots: a value lives until its
    /// successor has written, so neighbours never share and every third value
    /// reuses the first slot.
    #[test]
    fn a_chain_needs_two_slots() {
        let reads = vec![vec![], vec![0], vec![1], vec![2], vec![3]];
        assert_eq!(slots(&reads, &vec![None; reads.len()]), 2);
    }

    /// A diamond `0 → {1, 2} → 3`: the two arms are unordered, so three
    /// values are live at once; the join reuses the source's slot.
    #[test]
    fn a_diamond_needs_three_slots() {
        let reads = vec![vec![], vec![0], vec![0], vec![1, 2]];
        assert_eq!(slots(&reads, &vec![None; reads.len()]), 3);
    }

    /// A source fanning out to four unordered ops that fan in: the source and
    /// all four arms are live while the arms write, and the join takes the
    /// source's slot, so five slots.
    #[test]
    fn a_fan_out_fan_in_needs_width_plus_one_slots() {
        let reads = vec![vec![], vec![0], vec![0], vec![0], vec![0], vec![1, 2, 3, 4]];
        assert_eq!(slots(&reads, &vec![None; reads.len()]), 5);
    }

    /// A chain whose every op overwrites its input in place lives in one slot.
    #[test]
    fn an_in_place_chain_needs_one_slot() {
        let reads = vec![vec![], vec![0], vec![1], vec![2], vec![3]];
        let in_place: Vec<Option<usize>> = (0..5).map(|v: usize| v.checked_sub(1)).collect();
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
            let dag = Dag::new(&reads);
            let lives = dag.lives();
            let before = closure(&reads);
            let in_place: Table<AudioValueId, Option<AudioValueId>> = reads
                .iter()
                .enumerate()
                .map(|(v, rs)| rs.first().filter(|_| offer[v]).map(|&u| dag.values[u as usize]))
                .collect();
            let c = assign(&lives, &dag.reach, &in_place);
            let again = assign(&lives, &dag.reach, &in_place);
            prop_assert_eq!(&c.slot, &again.slot);
            prop_assert_eq!(c.count, again.count);
            prop_assert!(c.slot.iter().all(|&s| s < c.count));
            for v in lives.ids() {
                for u in lives.ids().take(v.index()) {
                    if c.slot[u] != c.slot[v] {
                        continue;
                    }
                    let w = lives[v].writer;
                    let src = &lives[u];
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
