//! The compiler: `compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)`.
//!
//! Pure: no I/O, no audio, no units — only the value, the shapes
//! the units declared, and the previous plan (for placements, and for the
//! output time an old main output's channels were heard at). The passes, in order:
//!
//! 1. **Check** (`shapes`) every node's shape against its spec — widths, latency and
//!    tail must all agree — and every event edge's ports against the shapes.
//!    The shape is what the unit says about itself and the spec is what the
//!    value says; if they disagree, one of them is stale, and compiling
//!    either would make the latency folds over the value
//!    (`crate::latency_plan`) disagree with the plan. Every event
//!    edge marked with a required resolution is checked against its sink's
//!    declared `Shape::event_resolution`.
//! 2. **SCC** (`schedule`, [`order::scc`]) over direct audio + event dependencies. A cycle
//!    not broken by a feedback edge is [`CompileError::Cycle`], naming the
//!    edges. A feedback edge becomes a read of a slot the executor fills from
//!    the edge's own delay state before the block, and a `Capture` op
//!    that feeds the delay, so the op DAG stays acyclic.
//! 3. **Order** (`schedule`, [`order::kahn`]) — one deterministic topological sort.
//! 4. **Latency solve** (`timing`) — `arrival = max(departures)`, `departure = arrival +
//!    own`: `crate::delays`' solve, over the spec (its
//!    `LatencyGraph` impl lists event and param sources). **Event and param
//!    edges count toward arrival**, and **a `PortSource::MainInput` input is a
//!    merge-point source**, delayed to the node's arrival like any other.
//!
//!    Emits a `Delay` op per mismatched audio port and per mismatched event
//!    *source*, plus per-output alignment rings, with state keyed by
//!    [`DelayKey`](plan::DelayKey). An event fan-in merges its sources by `(offset, source
//!    port)` — ties go to the lower `(NodeKey, port)` source, whatever order
//!    the spec lists them in. One wider than [`MAX_PORTS`] becomes a tree
//!    of merges over contiguous source ranges, which keeps that rule
//!    exactly.
//! 5. **Emit ops** (`emit`) in the serial order into a `Program` that names
//!    values, not slots, recording every value's writer, readers and event
//!    capacity, and each op's dependencies.
//! 6. **Group** (`coarsen`) the ops into tasks, reading no slot: fuse
//!    single-successor / single-predecessor chains, pack sibling tasks too
//!    cheap to dispatch alone (never lengthening the critical path), fuse the
//!    chains that leaves (estimates from `cost.rs`). A block run on the calling
//!    thread alone ignores tasks; one spread over a pool (`par.rs`) dispatches
//!    them.
//! 7. **Colour** (`colour`) — slot sharing that is correct under any parallel
//!    schedule, with in-place aliasing where legal, and the lowering of the
//!    program from values to slots.
//! 8. **Verify** ([`verify`]) in debug builds.
//! 9. **Place** units: `NodeKey` → dense [`UnitIdx`], diffed against `prev`
//!    into a [`Delta`].

mod coarsen;
mod colour;
mod emit;
pub(crate) mod ids;
pub(crate) mod latency;
pub(crate) mod main_output;
pub(crate) mod order;
pub(crate) mod plan;
pub(crate) mod preroll;
pub(crate) mod program;
mod schedule;
mod shapes;
pub(crate) mod tail;
mod timing;
pub(crate) mod verify;

use std::collections::{BTreeMap, BTreeSet};

use crate::NodeKey;
use crate::{Edge, InputPort, OutputPort, PortSink, PortSource};
use tutti_buffers::ChannelLayout;
use tutti_units::{Latency, Samples, Tail};

use crate::compile::ids::{PlanUnitId, Table};
use crate::compile::plan::{Delta, Placement, Plan, PlanUnit, UnitIdx};
use crate::model::spec::{EventInputPort, EventOutputPort, ValidGraph};
use crate::node::param::ParamIn;
use crate::node::shape::Shape;
use crate::node::{MAX_PORTS, Prepare, Resolution, Wired};

pub use verify::VerifyError;

/// The shape each node declared, by key — what `compile` checks the value
/// against and folds latency over.
pub type Shapes = BTreeMap<NodeKey, Shape>;

/// One edge of a cycle, named by its sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CycleEdge {
    /// An audio edge into this port.
    Audio(InputPort),
    /// An event edge from `from` into `at`.
    Event {
        /// The sink port.
        at: EventInputPort,
        /// The source port.
        from: EventOutputPort,
    },
    /// A param edge from `from` into `at`. Param edges are always direct: a
    /// node cannot drive itself, or anything upstream of it.
    Param {
        /// The param port.
        at: ParamIn,
        /// The event output that drives it.
        from: EventOutputPort,
    },
    /// A lane driving the gain of an edge into input port `at`, from `from`:
    /// it runs before the sum at the port, so before its node. Like a param
    /// edge, always direct. (The port and the source, not the whole edge's
    /// name, which would make every `CompileError` larger.)
    Gain {
        /// The port the weighted edge goes into.
        at: InputPort,
        /// The event output that drives the gain.
        from: EventOutputPort,
    },
}

/// Why a [`ValidGraph`] could not be compiled against these shapes.
///
/// This is the graph compiler's error; the modulation compiler's is
/// `tutti_modulation::ProgramCompileError`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CompileError {
    /// A feedback edge's delay is shorter than the `MaxBlock` being compiled
    /// for: a block cannot read samples it has not produced yet. For an
    /// offline render, prepare with a smaller maximum block — never shorten
    /// the loop, or the bounce would not sound like playback.
    #[error(
        "feedback edge {edge:?} delays {delay} frames, less than the maximum block {max_block}"
    )]
    FeedbackTooShort {
        /// The edge, named by its sink.
        edge: CycleEdge,
        /// Its declared delay.
        delay: Samples,
        /// The maximum block being compiled for.
        max_block: Samples,
    },
    /// No shape was supplied for a node.
    #[error("no shape for node {}", .node.0)]
    MissingShape {
        /// The node.
        node: NodeKey,
    },
    /// The node's shape disagrees with the widths its spec declared.
    #[error("node {} declares {}in/{}out but its unit is {}in/{}out", .node.0, .declared.0.count(), .declared.1.count(), .shape.0.count(), .shape.1.count())]
    WidthMismatch {
        /// The node.
        node: NodeKey,
        /// `(inputs, outputs)` as the spec declared them.
        declared: (ChannelLayout, ChannelLayout),
        /// `(inputs, outputs)` as the shape reports them.
        shape: (ChannelLayout, ChannelLayout),
    },
    /// An event edge names a port past a node's declared event ports.
    #[error("{}", event_port_out_of_range(.at, .from))]
    EventPortOutOfRange {
        /// The sink port of the bad edge.
        at: EventInputPort,
        /// The source, when it is the source that is out of range.
        from: Option<EventOutputPort>,
    },
    /// A node has more than [`MAX_PORTS`] channels or event ports on a side.
    #[error("node {} has {count} ports on one side; the limit is {}", .node.0, MAX_PORTS)]
    TooManyPorts {
        /// The node.
        node: NodeKey,
        /// The offending count.
        count: usize,
    },
    /// The shape's latency disagrees with the spec's: one of the two is
    /// stale.
    #[error("node {} declares {declared} frames of latency but its unit reports {shape}", .node.0)]
    LatencyMismatch {
        /// The node.
        node: NodeKey,
        /// What the spec declared.
        declared: Samples,
        /// What the shape reports.
        shape: Latency,
    },
    /// The shape's tail disagrees with the spec's.
    #[error("node {} declares tail {declared:?} but its unit reports {shape:?}", .node.0)]
    TailMismatch {
        /// The node.
        node: NodeKey,
        /// What the spec declared.
        declared: Tail,
        /// What the shape reports.
        shape: Tail,
    },
    /// An event edge marked as requiring a resolution (see
    /// [`GraphSpec::require_resolution`](crate::GraphSpec::require_resolution))
    /// targets a node that declares a coarser one: sample-accurate automation
    /// into a block-rate node, say.
    #[error("event edge from node {} port {} into node {} port {} requires {required:?} \
                 timing, but the sink honours only {sink:?}", .from.node.0, .from.port, .at.node.0, .at.port)]
    ResolutionTooCoarse {
        /// The sink port.
        at: EventInputPort,
        /// The source port.
        from: EventOutputPort,
        /// What the edge requires.
        required: Resolution,
        /// What the sink declares.
        sink: Resolution,
    },
    /// A modulated param its node does not declare
    /// ([`Shape::params`](crate::Shape::params)).
    #[error("node {} does not declare {:?} modulatable", .at.node.0, .at.param)]
    UnknownParam {
        /// The param port.
        at: ParamIn,
    },
    /// A param source names an event output its node does not have.
    #[error("param {:?} of node {}: source {from:?} is not an output of its node", .at.param, .at.node.0)]
    ParamSourceOutOfRange {
        /// The param port.
        at: ParamIn,
        /// The source.
        from: EventOutputPort,
    },
    /// An edge gain's source names an event output its node does not have.
    #[error("gain of an edge into {sink:?}: source {from:?} is not an output of its node")]
    GainSourceOutOfRange {
        /// The sink of the edge whose gain it drives.
        sink: PortSink,
        /// The source.
        from: EventOutputPort,
    },
    /// A cycle that no feedback edge breaks. Every direct edge inside the
    /// cycle's strongly connected component is listed, in key order.
    #[error("unbroken cycle through {} edges", .edges.len())]
    Cycle {
        /// The edges.
        edges: Vec<CycleEdge>,
    },
    /// A bus edge could not be lowered against the shapes: a node's bus is
    /// gone or its topology no longer folds into the other's (a node replaced
    /// by one with another layout), or a lane overlaps an edge. Never a
    /// silent drop.
    #[error("{0}")]
    Bus(crate::BusError),
}

/// The message of a [`CompileError::EventPortOutOfRange`]: the source's port
/// when the edge has a source, else the sink's own.
fn event_port_out_of_range(at: &EventInputPort, from: &Option<EventOutputPort>) -> String {
    match from {
        Some(from) => format!(
            "event edge into node {} port {}: source node {} has no event output {}",
            at.node.0, at.port, from.node.0, from.port
        ),
        None => format!("node {} has no event input {}", at.node.0, at.port),
    }
}

/// Compiles `graph` against the units' `shapes` into an immutable [`Plan`]
/// and the [`Delta`] from `prev`.
///
/// Pure: no I/O and no units, only the value, the shapes the units declared
/// and the `Prepare` they were prepared for. It checks each shape against
/// its node's spec, rejects cycles not broken by a feedback edge, orders the
/// nodes, solves latency compensation (inserting delays on the shorter
/// paths, for audio, events and params alike), assigns buffer slots, and
/// places units in the executor's store. Runs on the control thread and
/// allocates. [`Editor::commit`](crate::Editor::commit) calls it for you; call
/// it directly to inspect a plan or to drive [`Editor::package`](crate::Editor::package).
///
/// `prev` is the plan currently running, if any. It is read for two things:
/// which store index each surviving key already occupies, so that the
/// [`Delta`] moves only what changed, and, when the graph carries an old
/// main output's channels ([`ValidGraph::main_output_bank`]), its total latency, the output
/// time those channels align to. With no `prev`, that time is the new main output's
/// own total. Delay-ring and feedback state carry over by *key* at the
/// runtime and need nothing else from `prev`.
///
/// # Errors
///
/// Returns a [`CompileError`] when a node has no shape, a shape disagrees
/// with its spec, a port is out of range, a cycle has no feedback edge, a
/// feedback delay is shorter than the maximum block, a marked event edge
/// needs a finer resolution than its sink declares, or a limit is exceeded;
/// the variants name each case.
///
/// # Panics
///
/// In debug builds, if the plan it produced fails [`Plan::verify`]
/// — a compiler bug, never a property of the input.
pub fn compile(
    graph: &ValidGraph,
    shapes: &Shapes,
    prepare: &Prepare,
    prev: Option<&Plan>,
) -> Result<(Plan, Delta), CompileError> {
    let topology = graph.topology();
    let checked = shapes::check(graph, shapes, prepare)?;
    let order = schedule::order(&schedule::acyclic(graph, &checked)?, &checked);
    let timing = timing::solve(graph, &checked, prepare, prev);
    let program = emit::emit(graph, &checked, &order, &timing, prepare);
    let tasks = coarsen::group(&program, &checked, prepare.max_block().samples());
    let program = colour::colour(program, &checked);

    // ---- 9. place ---------------------------------------------------------
    let prev_units: BTreeMap<NodeKey, PlanUnit> = prev
        .map(|p| p.units.iter().map(|u| (u.key, *u)).collect())
        .unwrap_or_default();
    let mut delta = Delta::default();
    let mut idx_of: BTreeMap<NodeKey, UnitIdx> = BTreeMap::new();
    let mut taken: BTreeSet<UnitIdx> = BTreeSet::new();
    for &key in &checked.keys {
        let generation = graph.generation(key);
        if let Some(old) = prev_units.get(&key) {
            idx_of.insert(key, old.idx);
            taken.insert(old.idx);
            if old.generation != generation {
                delta.replace.push((
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
    for &key in &checked.keys {
        if idx_of.contains_key(&key) {
            continue;
        }
        while taken.contains(&next_free) {
            next_free = UnitIdx(next_free.0 + 1);
        }
        taken.insert(next_free);
        idx_of.insert(key, next_free);
        delta.insert.push(Placement {
            key,
            generation: graph.generation(key),
            idx: next_free,
        });
    }
    for (key, old) in &prev_units {
        if !topology.nodes.contains_key(key) {
            delta.retire.push(Placement {
                key: *key,
                generation: old.generation,
                idx: old.idx,
            });
        }
    }
    let store_len = taken.last().map_or(0, |m| m.0 + 1);
    delta.store_len = store_len;

    let consumed = consumed_outputs(topology);
    let units: Table<PlanUnitId, PlanUnit> = checked
        .keys
        .iter_enumerated()
        .map(|(n, &key)| PlanUnit {
            key,
            generation: graph.generation(key),
            idx: idx_of[&key],
            shape: checked.shapes[n],
            arrival: timing.arrival[n],
            wired: wired_ports(topology, key, &checked.shapes[n], &consumed),
        })
        .collect();

    let plan = Plan::new(prepare, program, tasks, units, store_len, graph, timing);

    // ---- 8. verify --------------------------------------------------------
    #[cfg(debug_assertions)]
    if let Err(e) = verify::verify(&plan) {
        panic!("tutti-graph produced an unsound plan: {e}");
    }

    Ok((plan, delta))
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
