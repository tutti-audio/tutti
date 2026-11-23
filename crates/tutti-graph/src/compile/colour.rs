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

use std::collections::BTreeMap;

use super::emit::Val;
use super::ids::{Id, OpId, Table, TableSlice};

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

/// Whether every op touching `u` happens-before `w`.
fn finished_before(u: &Val, w: OpId, reach: &Reach) -> bool {
    reach.before(u.writer, w) && u.readers.iter().all(|&r| reach.before(r, w))
}

/// Greedy colouring in writer order. `in_place[v] = u` offers `v` the slot of
/// `u`, taken when legal.
pub(crate) fn colour<V: Id>(
    vals: &TableSlice<V, Val>,
    reach: &Reach,
    in_place: &BTreeMap<V, V>,
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

        if let Some(&u) = in_place.get(&v) {
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

    fn slots(reads: &[Vec<u32>], in_place: &BTreeMap<AudioValueId, AudioValueId>) -> usize {
        let (vals, reach) = dag(reads);
        colour(&vals, &reach, in_place).count
    }

    /// A chain `0 → 1 → 2 → 3 → 4` needs two slots: a value lives until its
    /// successor has written, so neighbours never share and every third value
    /// reuses the first slot.
    #[test]
    fn a_chain_needs_two_slots() {
        let reads = vec![vec![], vec![0], vec![1], vec![2], vec![3]];
        assert_eq!(slots(&reads, &BTreeMap::new()), 2);
    }

    /// A diamond `0 → {1, 2} → 3`: the two arms are unordered, so three
    /// values are live at once; the join reuses the source's slot.
    #[test]
    fn a_diamond_needs_three_slots() {
        let reads = vec![vec![], vec![0], vec![0], vec![1, 2]];
        assert_eq!(slots(&reads, &BTreeMap::new()), 3);
    }

    /// A source fanning out to four unordered ops that fan in: the source and
    /// all four arms are live while the arms write, and the join takes the
    /// source's slot, so five slots.
    #[test]
    fn a_fan_out_fan_in_needs_width_plus_one_slots() {
        let reads = vec![vec![], vec![0], vec![0], vec![0], vec![0], vec![1, 2, 3, 4]];
        assert_eq!(slots(&reads, &BTreeMap::new()), 5);
    }

    /// A chain whose every op overwrites its input in place lives in one slot.
    #[test]
    fn an_in_place_chain_needs_one_slot() {
        let reads = vec![vec![], vec![0], vec![1], vec![2], vec![3]];
        let in_place: BTreeMap<AudioValueId, AudioValueId> = (1..5)
            .map(|v| (AudioValueId::new(v), AudioValueId::new(v - 1)))
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
            let mut in_place = BTreeMap::new();
            for (v, rs) in reads.iter().enumerate() {
                if offer[v] && let Some(&u) = rs.first() {
                    in_place.insert(AudioValueId::from_index(v), AudioValueId::new(u));
                }
            }
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
                    let hand_over = in_place.get(&v) == Some(&u)
                        && src.readers.contains(&w)
                        && precedes(src.writer)
                        && src.readers.iter().all(|&r| r == w || precedes(r));
                    prop_assert!(clear || hand_over, "values {u:?} and {v:?} overlap in slot {}", c.slot[u]);
                }
            }
        }
    }
}
