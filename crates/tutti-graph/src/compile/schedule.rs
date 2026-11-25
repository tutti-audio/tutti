//! Compiler passes 2 and 3 over the units: the direct dependencies with the
//! cycles refused, then the one serial order.

use crate::compile::ids::PlanUnitId;
use crate::compile::order;
use crate::compile::shapes::Units;
use crate::compile::{CompileError, CycleEdge};
use crate::model::spec::{EventEdge, ValidGraph};
use crate::{
    Edge, EdgeGain, EdgeId, EventInputPort, EventOutputPort, InputPort, NodeKey, ParamIn, PortSink,
    PortSource,
};
use std::collections::BTreeMap;

/// The direct dependencies of a graph: what must run before each node, with
/// multiplicity.
///
/// A feedback edge is not a dependency (it reads the past). The positions are
/// the indices [`dependencies`] was given: the cycle search is generic over
/// `usize`.
pub(crate) struct Dependencies {
    preds: Vec<Vec<usize>>,
}

/// The wiring a dependency is read from: the audio, event, param and gain
/// edges of a spec.
pub(crate) struct Wiring<'a> {
    pub(crate) edges: &'a BTreeMap<InputPort, Vec<Edge>>,
    pub(crate) events: &'a BTreeMap<EventInputPort, Vec<EventEdge>>,
    pub(crate) params: &'a BTreeMap<ParamIn, EventOutputPort>,
    pub(crate) gains: &'a BTreeMap<EdgeId, EdgeGain>,
}

/// The direct audio, event, param and gain dependencies of `wiring` over
/// `count` nodes, `index` naming a node's position (`None`: not a node of the
/// graph, its edges are not read), and the edges of a cycle no feedback edge
/// breaks, sorted, empty when there is none.
///
/// The one search both [`acyclic`] (the compiler's, over its units) and
/// `GraphSpec::cycle` (over a spec's nodes) run.
pub(crate) fn dependencies(
    wiring: &Wiring<'_>,
    count: usize,
    index: impl Fn(NodeKey) -> Option<usize>,
) -> (Dependencies, Vec<CycleEdge>) {
    // Direct dependencies, with multiplicity (for Kahn) and labelled (for the
    // error).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut labelled: Vec<(usize, usize, CycleEdge)> = Vec::new();
    let mut depend = |from: NodeKey, at: NodeKey, edge| {
        if let (Some(s), Some(d)) = (index(from), index(at)) {
            preds[d].push(s);
            labelled.push((s, d, edge));
        }
    };
    for (&at, edge) in wiring
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
        if let Edge::Direct(PortSource::Node(p)) = *edge {
            depend(p.node, at.node, CycleEdge::Audio(at));
        }
    }
    for (&at, sources) in wiring.events {
        for e in sources {
            if let EventEdge::Direct(from) = *e {
                depend(from.node, at.node, CycleEdge::Event { at, from });
            }
        }
    }
    for (&at, &from) in wiring.params {
        depend(from.node, at.node, CycleEdge::Param { at, from });
    }
    // A lane that drives the gain of an edge into a node's port runs before
    // the sum at that port, so before the node. (One into an output channel
    // feeds nothing a node reads.)
    for (&id, gain) in wiring.gains {
        if let (PortSink::Port(at), Some(from)) = (id.sink, gain.source) {
            depend(from.node, at.node, CycleEdge::Gain { at, from });
        }
    }
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (d, ps) in preds.iter().enumerate() {
        for &s in ps {
            succ[s].push(d);
        }
    }
    let comp = order::scc(&succ);
    let mut comp_size = vec![0usize; count];
    for &c in &comp {
        comp_size[c] += 1;
    }
    let mut cycle: Vec<CycleEdge> = labelled
        .iter()
        .filter(|(s, d, _)| comp[*s] == comp[*d] && (s == d || comp_size[comp[*s]] > 1))
        .map(|&(_, _, e)| e)
        .collect();
    cycle.sort();
    (Dependencies { preds }, cycle)
}

/// The direct audio, event, param and gain dependencies of `graph` over
/// `units`, or the edges of a cycle no feedback edge breaks.
///
/// Returns the [`Dependencies`]; the result has no cycle, so [`order()`] is
/// total over it.
pub(super) fn acyclic(graph: &ValidGraph, units: &Units) -> Result<Dependencies, CompileError> {
    let wiring = Wiring {
        edges: &graph.topology().edges,
        events: graph.events(),
        params: graph.params(),
        gains: graph.gains(),
    };
    let (dependencies, cycle) = dependencies(&wiring, units.keys.len(), |k| {
        units.get(k).map(PlanUnitId::index)
    });
    if cycle.is_empty() {
        Ok(dependencies)
    } else {
        Err(CompileError::Cycle { edges: cycle })
    }
}

/// The units in the one deterministic topological order, dependencies first.
///
/// Takes the acyclic [`Dependencies`] and the `units` they are over; returns
/// every unit exactly once, each after all of its predecessors.
pub(super) fn order(dependencies: &Dependencies, units: &Units) -> Vec<PlanUnitId> {
    let sorted = order::kahn(&dependencies.preds);
    debug_assert!(sorted.cyclic.is_empty(), "the SCC pass left a cycle");
    sorted
        .order
        .into_iter()
        .map(|i| units.keys.id(i).expect("a unit of the graph"))
        .collect()
}
