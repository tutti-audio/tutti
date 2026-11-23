//! Random graphs through the compiler: a cycle no feedback edge breaks is
//! refused, and every plan the compiler produces passes `Plan::verify`, so
//! no schedule of its tasks can run two ops that touch one slot together
//! (rules 2 and 3), run a unit twice (rule 6) or run a task before its
//! predecessors (rule 5). The verifier's own rejections of a bad plan are
//! pinned in `src/compile/verify.rs`.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use tutti_graph::testing as common;
use tutti_graph::{
    Edge, EventEdge, EventInputPort, EventOutputPort, InputPort, NodeKey, OutputPort, PortSource,
};

use crate::generator::{Desc, MAX_BLOCK, Rng, random_graph, shape};

/// Declared costs for `desc`'s nodes, a pure function of the seed: about
/// two in three declare `Cost::Light`, a few `Cost::Moderate`, the rest
/// nothing (`Cost::Heavy`) — so cheap siblings pack beside heavy tasks that keep the plan parallel.
fn random_costs(desc: &Desc, seed: u64) -> BTreeMap<NodeKey, tutti_graph::Cost> {
    let mut rng = Rng::new(seed ^ 0xC057);
    desc.kinds
        .keys()
        .filter_map(|&k| match rng.below(6) {
            0..=3 => Some((k, tutti_graph::Cost::Light)),
            4 => Some((k, tutti_graph::Cost::Moderate)),
            _ => None,
        })
        .collect()
}

/// Add a direct back edge — audio or event — from a node to one created no
/// later than it, closing a cycle no feedback edge breaks.
fn close_a_cycle(desc: &mut Desc, rng: &mut Rng) -> bool {
    let n = desc.order.len();
    for _ in 0..32 {
        let (i, j) = (rng.below(n as u64) as usize, rng.below(n as u64) as usize);
        let (early, late) = (desc.order[i.min(j)], desc.order[i.max(j)]);
        let (se, sl) = (shape(&desc.kinds[&early]), shape(&desc.kinds[&late]));
        // `late` must depend on `early` for the back edge to close a cycle.
        let reaches = |d: &Desc| {
            let mut stack = vec![early];
            let mut seen = BTreeSet::new();
            while let Some(k) = stack.pop() {
                if k == late {
                    return true;
                }
                if !seen.insert(k) {
                    continue;
                }
                for (at, e) in d
                    .spec
                    .topology
                    .edges
                    .iter()
                    .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
                {
                    if let Edge::Direct(PortSource::Node(p)) = e
                        && p.node() == k
                    {
                        stack.push(at.node());
                    }
                }
                for (at, v) in &d.spec.events {
                    if v.iter()
                        .any(|e| matches!(e, EventEdge::Direct(f) if f.node() == k))
                    {
                        stack.push(at.node());
                    }
                }
            }
            false
        };
        if !reaches(desc) {
            continue;
        }
        if se.audio_in().count() > 0 && sl.audio_out().count() > 0 {
            desc.spec.topology.edges.insert(
                InputPort::new(early, 0),
                vec![Edge::Direct(PortSource::Node(OutputPort::new(late, 0)))],
            );
            return true;
        }
        let from = EventOutputPort::new(late, 0);
        let listed = desc
            .spec
            .events
            .get(&EventInputPort::new(early, 0))
            .is_some_and(|v| v.iter().any(|e| e.from() == from));
        if se.event_in > 0 && sl.event_out > 0 && !listed {
            desc.spec.connect_events(
                EventInputPort::new(early, 0),
                EventEdge::Direct(EventOutputPort::new(late, 0)),
            );
            return true;
        }
    }
    false
}

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(256), ..ProptestConfig::default() })]

    /// A cycle no feedback edge breaks is rejected — by `validate` when it is
    /// all audio, by `compile` (naming its edges) when it runs through an
    /// event edge — and never reaches an executor.
    #[test]
    fn unbroken_cycles_are_rejected(seed in any::<u64>()) {
        let mut desc = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0xC1C1E);
        prop_assume!(close_a_cycle(&mut desc, &mut rng));
        let rejected = match desc.spec.validate() {
            Err(errs) => errs.iter().any(|e| matches!(
                e,
                tutti_graph::GraphInvalid::Topology(tutti_graph::Invalid::Cycle { .. })
            )),
            Ok(valid) => matches!(
                tutti_graph::compile(
                    &valid,
                    &common::shapes_of(&desc.kinds),
                    &common::prepare(MAX_BLOCK),
                    None,
                ),
                Err(tutti_graph::CompileError::Cycle { .. })
            ),
        };
        prop_assert!(rejected, "a direct cycle got through");
    }
}

/// Every pair of values that share a slot, checked against reachability
/// computed here from scratch (a DFS over the plan's successor lists) rather
/// than by the compiler's bitsets or its verifier: either every op touching
/// one happens-before the other's writer, or the second is an in-place
/// overwrite by the first's last reader. Anything else could be run
/// concurrently by a parallel executor and would clobber a live buffer.
fn brute_force_interference(plan: &tutti_graph::Plan) {
    use tutti_graph::PortKind;
    let n = plan.ops().len();
    let succ = plan.op_successors();
    // reach[a] = every op reachable from a (strictly).
    let reach: Vec<BTreeSet<u32>> = (0..n)
        .map(|a| {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<u32> = succ.row(a).to_vec();
            while let Some(x) = stack.pop() {
                if seen.insert(x) {
                    stack.extend_from_slice(succ.row(x as usize));
                }
            }
            seen
        })
        .collect();
    let before = |a: u32, b: u32| reach[a as usize].contains(&b);
    let readers = |v: &tutti_graph::Value| {
        let r = v.readers;
        &plan.value_readers()[r.start as usize..(r.start + r.len) as usize]
    };
    for kind in [PortKind::Audio, PortKind::Event] {
        let values = plan.values(kind);
        for (i, u) in values.iter().enumerate() {
            for v in &values[i + 1..] {
                if u.slot != v.slot {
                    continue;
                }
                let (first, second) = if before(u.writer, v.writer) {
                    (u, v)
                } else {
                    (v, u)
                };
                let clean = std::iter::once(first.writer)
                    .chain(readers(first).iter().copied())
                    .all(|t| before(t, second.writer));
                let in_place = readers(first).contains(&second.writer)
                    && readers(first)
                        .iter()
                        .all(|&r| r == second.writer || before(r, second.writer));
                assert!(
                    clean || in_place,
                    "{kind:?} values written by ops {} and {} share slot {} but may overlap",
                    first.writer,
                    second.writer,
                    u.slot
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(512), ..ProptestConfig::default() })]

    /// Colouring is correct under any parallel schedule: the verifier accepts
    /// every plan, and an independent brute-force check agrees.
    #[test]
    fn colouring_is_safe_under_any_schedule(seed in any::<u64>()) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) = tutti_graph::compile(&valid, &common::shapes_of(&desc.kinds), &common::prepare(MAX_BLOCK), None)
            .expect("compiles");
        plan.verify().expect("the verifier accepts it");
        brute_force_interference(&plan);
    }

    /// The same under declared costs, which pack cheap siblings into one
    /// task: every packed plan still verifies and still has no overlap.
    #[test]
    fn a_costed_plan_is_safe_under_any_schedule(seed in any::<u64>()) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) = tutti_graph::compile(
            &valid,
            &common::shapes_costed(&desc.kinds, &random_costs(&desc, seed)),
            &common::prepare(MAX_BLOCK),
            None,
        )
        .expect("compiles");
        plan.verify().expect("the verifier accepts it");
        brute_force_interference(&plan);
    }
}

/// The generator is not vacuous: over a fixed range of seeds, plans have
/// several tasks, packed tasks (declared costs fuse siblings), a sum op, and
/// ops that overlap in no order, so `verify` has something to reject.
#[test]
fn the_generated_plans_have_tasks_fan_in_and_packing() {
    let (mut multi, mut summed, mut packed) = (0, 0, 0);
    for seed in 0..256u64 {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let compile = |costs: &BTreeMap<NodeKey, tutti_graph::Cost>| {
            tutti_graph::compile(
                &valid,
                &common::shapes_costed(&desc.kinds, costs),
                &common::prepare(MAX_BLOCK),
                None,
            )
            .expect("compiles")
            .0
        };
        let plain = compile(&BTreeMap::new());
        let costed = compile(&random_costs(&desc, seed));
        multi += usize::from(plain.tasks().len() > 1);
        summed += usize::from(
            plain
                .ops()
                .iter()
                .any(|o| matches!(o, tutti_graph::Op::Sum { .. })),
        );
        packed += usize::from(costed.tasks().len() < plain.tasks().len());
    }
    assert!(multi > 100, "only {multi} plans have several tasks");
    assert!(summed > 25, "only {summed} plans sum a fan-in");
    assert!(packed > 25, "only {packed} plans pack further under costs");
}
