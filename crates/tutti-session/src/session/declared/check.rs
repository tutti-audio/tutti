//! What a patch's declared nodes and wires are checked against before anything
//! changes: the wires the graph would take, the feedback and second edges, and
//! the cycles.

use std::collections::{BTreeMap, BTreeSet};

use tutti_buffers::Float;
use tutti_graph::{
    BusOutputPort, CycleEdge, Edge, EdgeId, GraphSpec, NodeKey, NodeSpec, PortSink, PortSource,
};
use tutti_units::Samples;

use super::wire::{KeptWire, Ports, Resolved};
use super::{Fate, Kept, PatchError, Planned};
use crate::key::BusEnd;
use crate::patch::WireId;
use crate::{AppKey, BusInputPort, ConnectError, Graph};

/// Refuses what the declared nodes and wires cannot make, with nothing changed,
/// and names the edges leaving the graph that would close a cycle with them.
pub(super) fn check<K: AppKey, F: Float>(
    graph: &Graph<F>,
    planned: &Planned<K, F>,
    wires: &BTreeMap<WireId<K>, KeptWire>,
    kept: &Kept<K, F>,
) -> Result<Cuts, PatchError<K>> {
    self::wires(graph, planned, wires)
        .and_then(|()| feedback(graph, planned, wires, kept))
        .and_then(|()| acyclic(graph, planned, wires, kept))
}

/// The edges leaving the graph that would close a cycle with the patch's
/// wiring: cut at the patch, not faded.
#[derive(Default)]
pub(super) struct Cuts {
    pub(super) edges: Vec<EdgeId>,
    pub(super) buses: Vec<(BusOutputPort, BusInputPort)>,
}

/// Takes `gone` out of `spec`: the nodes, and every edge into or out of them
/// (audio, event, param, gain and bus).
fn forget_nodes(spec: &mut GraphSpec, gone: &BTreeSet<NodeKey>) {
    let from_gone =
        |source: PortSource| matches!(source, PortSource::Node(p) if gone.contains(&p.node()));
    let edge_gone = |edge: &Edge| match *edge {
        Edge::Direct(source) => from_gone(source),
        Edge::Feedback(f) => gone.contains(&f.from.node()),
    };
    for node in gone {
        spec.topology.nodes.remove(node);
    }
    spec.topology.edges.retain(|at, edges| {
        edges.retain(|e| !edge_gone(e));
        !gone.contains(&at.node()) && !edges.is_empty()
    });
    for sources in &mut spec.topology.outputs {
        sources.retain(|source| !from_gone(*source));
    }
    spec.events.retain(|at, edges| {
        edges.retain(|e| !gone.contains(&e.from().node()));
        !gone.contains(&at.node()) && !edges.is_empty()
    });
    spec.params
        .retain(|at, from| !gone.contains(&at.node) && !gone.contains(&from.node()));
    spec.gains.retain(|id, _| {
        let sink = matches!(id.sink, PortSink::Port(at) if gone.contains(&at.node()));
        !sink && !edge_gone(&id.from)
    });
    let bus_gone = |source: &BusOutputPort| matches!(source, BusOutputPort::Node(bus) if gone.contains(&bus.node()));
    spec.bus_edges.retain(|at, feeds| {
        feeds.retain(|feed| !bus_gone(&feed.source));
        !gone.contains(&at.node()) && !feeds.is_empty()
    });
    spec.main_output_sources
        .retain(|feed| !bus_gone(&feed.source));
}

/// Refuses a wire the declared nodes cannot make, with nothing changed.
fn wires<K: AppKey, F: Float>(
    graph: &Graph<F>,
    planned: &Planned<K, F>,
    wires: &BTreeMap<WireId<K>, KeptWire>,
) -> Result<(), PatchError<K>> {
    let pending = Some(&planned.shapes);
    let mut buses = Vec::new();
    let mut bus_ends = Vec::new();
    for id in wires.keys() {
        let resolved = Resolved::of(id, |key| planned.at[key]);
        resolved
            .check(graph, pending)
            .map_err(|cause| PatchError::Wire {
                ends: id.ends.clone(),
                cause,
            })?;
        if let Resolved::Bus(from, to) = resolved {
            buses.push((from, to));
            bus_ends.push(&id.ends);
        }
    }
    if let Some((at, cause)) = graph.first_unlowered(pending, &buses) {
        return Err(PatchError::Wire {
            ends: bus_ends[at].clone(),
            cause: ConnectError::Bus(cause),
        });
    }
    Ok(())
}

/// Refuses a feedback wire the graph would not compile, and a wire that
/// puts a second edge on ports that hold one, with nothing changed.
fn feedback<K: AppKey, F: Float>(
    graph: &Graph<F>,
    planned: &Planned<K, F>,
    wires: &BTreeMap<WireId<K>, KeptWire>,
    kept: &Kept<K, F>,
) -> Result<(), PatchError<K>> {
    let pending = Some(&planned.shapes);
    let max_block = graph.max_block_samples();
    // What the patch takes out leaves its ports free for another kind.
    let mut leaving = BTreeSet::new();
    for id in kept.wires.keys().filter(|id| !wires.contains_key(*id)) {
        let held = [Some(id.source()), id.sink()]
            .into_iter()
            .flatten()
            .all(|key| kept.nodes.contains_key(key));
        if held {
            let resolved = Resolved::of(id, |key| kept.nodes[key].node);
            leaving.extend(
                Ports::of(resolved, graph, None)
                    .into_iter()
                    .map(|ports| (ports, id.delay)),
            );
        }
    }
    let mut declared: BTreeMap<Ports, Option<Samples>> = BTreeMap::new();
    for id in wires.keys() {
        if let Some(delay) = id.delay
            && delay < max_block
        {
            return Err(PatchError::FeedbackTooShort {
                ends: id.ends.clone(),
                delay,
                max_block,
            });
        }
        let resolved = Resolved::of(id, |key| planned.at[key]);
        for ports in Ports::of(resolved, graph, pending) {
            let twice = declared
                .insert(ports, id.delay)
                .is_some_and(|other| other != id.delay);
            let held = ports
                .held_delay(graph)
                .is_some_and(|held| held != id.delay && !leaving.contains(&(ports, held)));
            if twice || held {
                return Err(PatchError::TwoEdges {
                    ends: id.ends.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Refuses a wire that closes a cycle the graph would not compile, and
/// names the edges leaving the graph that must be cut for it not to: the
/// graph's own search ([`GraphSpec::cycle`](tutti_graph::GraphSpec::cycle))
/// over its whole wiring, the edges it only fades out included (the compile
/// counts them), plus the declared wires. A feedback edge is cut by the
/// search. An edge that leaves (dropped by this patch, or still fading)
/// and closes a cycle is cut; a cycle left through live wires is the
/// patch's.
fn acyclic<K: AppKey, F: Float>(
    graph: &Graph<F>,
    planned: &Planned<K, F>,
    wires: &BTreeMap<WireId<K>, KeptWire>,
    kept: &Kept<K, F>,
) -> Result<Cuts, PatchError<K>> {
    let pending = Some(&planned.shapes);
    let mut spec = graph.whole_spec();
    // The nodes this patch takes out are not in the graph it makes, nor
    // are the edges into or out of them, hand-made ones included.
    let gone: BTreeSet<NodeKey> = kept
        .nodes
        .iter()
        .filter(|(key, _)| !planned.at.contains_key(*key))
        .map(|(_, stored)| stored.node)
        .chain(
            planned
                .nodes
                .iter()
                .filter(|p| matches!(p.fate, Fate::Readd(_)))
                .filter_map(|p| kept.nodes.get(&p.key).map(|stored| stored.node)),
        )
        .collect();
    forget_nodes(&mut spec, &gone);
    for (&node, shape) in &planned.shapes {
        let declared = NodeSpec::new("declared", shape.audio_in(), shape.audio_out());
        spec.topology.nodes.insert(node, declared);
    }
    let (mut leaving, mut leaving_buses) = graph.fading_edges();
    for id in kept.wires.keys().filter(|id| !wires.contains_key(*id)) {
        let held = [Some(id.source()), id.sink()]
            .into_iter()
            .flatten()
            .all(|key| kept.nodes.contains_key(key));
        if !held {
            continue;
        }
        let resolved = Resolved::of(id, |key| kept.nodes[key].node);
        match resolved {
            // An event edge is never faded: it goes at once.
            Resolved::Event(..) => {
                resolved.in_spec(&mut spec, graph, None, false);
            }
            Resolved::Bus(from, to) => {
                leaving_buses.insert((from, to));
            }
            Resolved::Channel(..) | Resolved::Main(..) => {
                for ports in Ports::of(resolved, graph, None) {
                    if let Ports::Audio(edge) = ports
                        && let Some(edge) = Graph::<F>::edge_id(edge, id.delay)
                    {
                        leaving.insert(edge);
                    }
                }
            }
        }
    }
    let named: Vec<_> = wires
        .keys()
        .map(|id| {
            let resolved = Resolved::of(id, |key| planned.at[key]);
            // A declared wire is not leaving, whatever the graph fades.
            for ports in Ports::of(resolved, graph, pending) {
                if let Ports::Audio(edge) = ports
                    && let Some(edge) = Graph::<F>::edge_id(edge, id.delay)
                {
                    leaving.remove(&edge);
                }
            }
            if let Resolved::Bus(from, to) = resolved {
                leaving_buses.remove(&(from, to));
            }
            (id, resolved.in_spec(&mut spec, graph, pending, true))
        })
        .collect();
    let mut cuts = Cuts::default();
    let cycle = loop {
        let Some(cycle) = spec.cycle() else {
            break None;
        };
        let audio = cycle.iter().find_map(|edge| match edge {
            CycleEdge::Audio(at) => leaving
                .iter()
                .find(|id| id.sink == PortSink::Port(*at))
                .copied(),
            _ => None,
        });
        if let Some(id) = audio {
            leaving.remove(&id);
            if let PortSink::Port(at) = id.sink {
                spec.disconnect(at, id.from);
            }
            cuts.edges.push(id);
            continue;
        }
        let bus = cycle.iter().find_map(|edge| match edge {
            CycleEdge::Bus { at, from } => leaving_buses
                .iter()
                .find(|(f, t)| f == from && t.0 == BusEnd::Node(*at))
                .copied(),
            _ => None,
        });
        if let Some((from, to)) = bus {
            leaving_buses.remove(&(from, to));
            if let BusEnd::Node(at) = to.0 {
                spec.disconnect_bus(from, at);
            }
            cuts.buses.push((from, to));
            continue;
        }
        break Some(cycle);
    };
    let Some(cycle) = cycle else {
        return Ok(cuts);
    };
    // Only a cycle through a declared wire is the patch's; any other the
    // commit reports.
    match named
        .into_iter()
        .find(|(_, edges)| edges.iter().any(|edge| cycle.contains(edge)))
    {
        Some((id, _)) => Err(PatchError::Cycle {
            ends: id.ends.clone(),
        }),
        None => Ok(cuts),
    }
}
