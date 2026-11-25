//! Compiler placement: where each unit lives in the executor's store, and
//! what changed against the previous plan.

use std::collections::{BTreeMap, BTreeSet};

use crate::compile::carry::Carry;
use crate::compile::ids::{PlanUnitId, Table};
use crate::compile::plan::{Delta, Placement, Plan, PlanUnit, Rewired, UnitIdx};
use crate::compile::program::{BySlot, Program};
use crate::compile::shapes::Units;
use crate::compile::timing::Timing;
use crate::model::spec::ValidGraph;
use crate::node::shape::Shape;
use crate::node::{Prepare, Wired};
use crate::{Edge, InputPort, NodeKey, OutputPort, PortSource};

/// The plan's units with their store indices, and the delta against the
/// previous plan.
///
/// Every unit has a distinct store index below `delta.store_len()`; a key the
/// previous plan had keeps its index. After this pass nothing is keyed:
/// the units are a table by [`PlanUnitId`].
pub(super) struct Placed {
    /// What the executor changes to go from the previous plan to this one.
    pub(super) delta: Delta,
    /// The units of the plan, in key order.
    pub(super) units: Table<PlanUnitId, PlanUnit>,
}

/// Places every unit of `units` in the store, diffed against `prev`.
///
/// Takes the graph, the checked units, the `timing` for each unit's arrival,
/// the `program` and the `prepare` it was built for, and the plan being
/// replaced; returns the [`Placed`] units. A surviving key
/// keeps its store index (a changed generation is a replace), a new key takes
/// the lowest free index, and a key gone from the graph retires.
///
/// Three maps stay keyed here, because this pass is where keys become dense:
/// `prev_units` reads the previous plan by `NodeKey`, `taken` is the set of
/// store indices (a sparse space of the previous plan's, not a unit id), and
/// `consumed` is the set of output ports something reads.
pub(super) fn place(
    graph: &ValidGraph,
    units: &Units,
    timing: &Timing,
    program: &Program<BySlot>,
    prepare: &Prepare,
    prev: Option<&Plan>,
) -> Placed {
    let topology = graph.topology();
    let prev_units: BTreeMap<NodeKey, PlanUnit> = prev
        .map(|p| p.units.iter().map(|u| (u.key, *u)).collect())
        .unwrap_or_default();
    let mut replace = Vec::new();
    let mut insert = Vec::new();
    let mut retire = Vec::new();
    let mut idx: Table<PlanUnitId, Option<UnitIdx>> = Table::filled(units.keys.len(), None);
    let mut taken: BTreeSet<UnitIdx> = BTreeSet::new();
    for (n, &key) in units.keys.iter_enumerated() {
        let generation = graph.generation(key);
        if let Some(old) = prev_units.get(&key) {
            idx[n] = Some(old.idx);
            taken.insert(old.idx);
            if old.generation != generation {
                replace.push((
                    Placement {
                        key,
                        generation: old.generation,
                        idx: old.idx,
                    },
                    Placement {
                        key,
                        generation,
                        idx: old.idx,
                    },
                ));
            }
        }
    }
    let mut next_free = UnitIdx(0);
    for (n, &key) in units.keys.iter_enumerated() {
        if idx[n].is_some() {
            continue;
        }
        while taken.contains(&next_free) {
            next_free = UnitIdx(next_free.0 + 1);
        }
        taken.insert(next_free);
        idx[n] = Some(next_free);
        insert.push(Placement {
            key,
            generation: graph.generation(key),
            idx: next_free,
        });
    }
    for (key, old) in &prev_units {
        if !topology.nodes.contains_key(key) {
            retire.push(Placement {
                key: *key,
                generation: old.generation,
                idx: old.idx,
            });
        }
    }
    let consumed = consumed_outputs(topology);
    let mut rewired = Vec::new();
    let placed = units
        .keys
        .iter_enumerated()
        .map(|(n, &key)| {
            let wired = wired_ports(topology, key, &units.shapes[n], &consumed);
            let was = prev_units
                .get(&key)
                .map_or_else(Wired::default, |o| o.wired);
            if was != wired {
                rewired.push(Rewired {
                    node: key,
                    was,
                    now: wired,
                });
            }
            PlanUnit {
                key,
                generation: graph.generation(key),
                idx: idx[n].expect("every unit is placed"),
                shape: units.shapes[n],
                arrival: timing.arrival[n],
                wired,
            }
        })
        .collect();
    let store_len = taken.last().map_or(0, |m| m.0 + 1);
    let carry = Carry::between(prev, program, prepare, graph.main_output_bank());
    Placed {
        delta: Delta::new(insert, retire, replace, store_len, rewired, carry),
        units: placed,
    }
}

/// The output ports something reads: an edge (direct or feedback), a main
/// output, a tap lane or a meter lane.
fn consumed_outputs(topology: &crate::Topology) -> BTreeSet<OutputPort> {
    let edges = topology.edges.values().flatten().filter_map(|e| match *e {
        Edge::Direct(PortSource::Node(p)) => Some(p),
        Edge::Feedback(f) => Some(f.from),
        Edge::Direct(_) => None,
    });
    let lanes = topology
        .taps
        .values()
        .filter_map(|t| match t {
            crate::TapSpec::Ports { sources, .. } => Some(sources),
            crate::TapSpec::Outputs { .. } => None,
        })
        .chain(topology.meters.values().filter_map(|m| match m {
            crate::MeterSpec::Ports { sources, .. } => Some(sources),
            crate::MeterSpec::Outputs => None,
        }))
        .chain(&topology.outputs)
        .flatten()
        .filter_map(|s| match *s {
            PortSource::Node(p) => Some(p),
            _ => None,
        });
    edges.chain(lanes).collect()
}

/// The audio ports of `key` with a source (an edge, direct or feedback) or a
/// consumer among `consumed`. A validated graph stores no silence edge.
fn wired_ports(
    topology: &crate::Topology,
    key: NodeKey,
    shape: &Shape,
    consumed: &BTreeSet<OutputPort>,
) -> Wired {
    let mut wired = Wired::default();
    for port in 0..shape.audio_in().count() {
        let at = InputPort::new(key, port);
        let fed = topology.edges.get(&at).is_some_and(|e| !e.is_empty());
        if fed {
            wired.inputs = wired.inputs.with(usize::from(port));
        }
    }
    for port in 0..shape.audio_out().count() {
        if consumed.contains(&OutputPort::new(key, port)) {
            wired.outputs = wired.outputs.with(usize::from(port));
        }
    }
    wired
}
