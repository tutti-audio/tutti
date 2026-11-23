//! Compiler passes 2 and 3 over the units: the direct dependencies with the
//! cycles refused, then the one serial order.

use crate::compile::ids::{Id, PlanUnitId};
use crate::compile::order;
use crate::compile::shapes::Units;
use crate::compile::{CompileError, CycleEdge};
use crate::model::spec::{EventEdge, ValidGraph};
use crate::{Edge, PortSink, PortSource};

/// The direct dependencies of an acyclic graph: what must run before each
/// unit, with multiplicity.
///
/// A feedback edge is not a dependency (it reads the past). The positions are
/// [`PlanUnitId`] indices: the cycle search is generic over `usize`.
pub(super) struct Dependencies {
    preds: Vec<Vec<usize>>,
}

/// The direct audio, event, param and gain dependencies of `graph` over
/// `units`, or the edges of a cycle no feedback edge breaks.
///
/// Returns the [`Dependencies`]; the result has no cycle, so [`order()`] is
/// total over it.
pub(super) fn acyclic(graph: &ValidGraph, units: &Units) -> Result<Dependencies, CompileError> {
    let topology = graph.topology();
    // Direct dependencies, with multiplicity (for Kahn) and labelled (for the
    // error).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); units.keys.len()];
    let mut labelled: Vec<(usize, usize, CycleEdge)> = Vec::new();
    let mut depend = |from, at, edge| {
        let (s, d): (PlanUnitId, PlanUnitId) = (units.unit(from), units.unit(at));
        preds[d.index()].push(s.index());
        labelled.push((s.index(), d.index(), edge));
    };
    for (&at, edge) in topology
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
        if let Edge::Direct(PortSource::Node(p)) = *edge {
            depend(p.node, at.node, CycleEdge::Audio(at));
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Direct(from) = *e {
                depend(from.node, at.node, CycleEdge::Event { at, from });
            }
        }
    }
    for (&at, &from) in graph.params() {
        depend(from.node, at.node, CycleEdge::Param { at, from });
    }
    // A lane that drives the gain of an edge into a node's port runs before
    // the sum at that port, so before the node. (One into an output channel
    // feeds nothing a node reads.)
    for (&id, gain) in graph.gains() {
        if let (PortSink::Port(at), Some(from)) = (id.sink, gain.source) {
            depend(from.node, at.node, CycleEdge::Gain { at, from });
        }
    }
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); units.keys.len()];
    for (d, ps) in preds.iter().enumerate() {
        for &s in ps {
            succ[s].push(d);
        }
    }
    let comp = order::scc(&succ);
    let mut comp_size = vec![0usize; units.keys.len()];
    for &c in &comp {
        comp_size[c] += 1;
    }
    let mut cycle: Vec<CycleEdge> = labelled
        .iter()
        .filter(|(s, d, _)| comp[*s] == comp[*d] && (s == d || comp_size[comp[*s]] > 1))
        .map(|&(_, _, e)| e)
        .collect();
    if !cycle.is_empty() {
        cycle.sort();
        return Err(CompileError::Cycle { edges: cycle });
    }
    Ok(Dependencies { preds })
}

/// The units in the one deterministic topological order, dependencies first.
///
/// Takes the acyclic [`Dependencies`]; returns every unit exactly once, each
/// after all of its predecessors.
pub(super) fn order(dependencies: &Dependencies) -> Vec<PlanUnitId> {
    let sorted = order::kahn(&dependencies.preds);
    debug_assert!(sorted.cyclic.is_empty(), "the SCC pass left a cycle");
    sorted
        .order
        .into_iter()
        .map(PlanUnitId::from_index)
        .collect()
}
