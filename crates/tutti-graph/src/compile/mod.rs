//! The compiler: `compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)`.
//!
//! Pure: no I/O, no audio, no units — only the value, the shapes
//! the units declared, and the previous plan (for placements, and for the
//! output time an old main output's channels were heard at). The passes, in order:
//!
//! 1. **Check** every node's shape against its spec — widths, latency and
//!    tail must all agree — and every event edge's ports against the shapes.
//!    The shape is what the unit says about itself and the spec is what the
//!    value says; if they disagree, one of them is stale, and compiling
//!    either would make the latency folds over the value
//!    (`crate::latency_plan`) disagree with the plan. Every event
//!    edge marked with a required resolution is checked against its sink's
//!    declared `Shape::event_resolution`.
//! 2. **SCC** ([`order::scc`]) over direct audio + event dependencies. A cycle
//!    not broken by a feedback edge is [`CompileError::Cycle`], naming the
//!    edges. A feedback edge becomes a read of a slot the executor fills from
//!    the edge's own delay state before the block, and a `Capture` op
//!    that feeds the delay, so the op DAG stays acyclic.
//! 3. **Order** ([`order::kahn`]) — one deterministic topological sort.
//! 4. **Latency solve** — `arrival = max(departures)`, `departure = arrival +
//!    own`: `crate::delays`' solve, over the spec (its
//!    `LatencyGraph` impl lists event and param sources). **Event and param
//!    edges count toward arrival**, and **a `PortSource::MainInput` input is a
//!    merge-point source**, delayed to the node's arrival like any other.
//!
//!    Emits a `Delay` op per mismatched audio port and per mismatched event
//!    *source*, plus per-output alignment rings, with state keyed by
//!    [`DelayKey`]. An event fan-in merges its sources by `(offset, source
//!    port)` — ties go to the lower `(NodeKey, port)` source, whatever order
//!    the spec lists them in. One wider than [`MAX_PORTS`] becomes a tree
//!    of merges over contiguous source ranges, which keeps that rule
//!    exactly.
//! 5. **Emit ops** in the serial order, recording every value's writer and
//!    readers, and the op DAG.
//! 6. **Colour** ([`colour`]) — slot sharing that is correct under any
//!    parallel schedule, with in-place aliasing where legal.
//! 7. **Coarsen and price** ([`coarsen`]) — fuse single-successor /
//!    single-predecessor chains into tasks, pack sibling tasks too cheap to
//!    dispatch alone (never lengthening the critical path), fuse the chains
//!    that leaves (estimates from `cost.rs`). A block run on
//!    the calling thread alone ignores tasks; one spread over a pool
//!    (`par.rs`) dispatches them.
//! 8. **Verify** ([`verify`]) in debug builds.
//! 9. **Place** units: `NodeKey` → dense [`UnitIdx`], diffed against `prev`
//!    into a [`Delta`].

mod coarsen;
mod colour;
pub(crate) mod ids;
pub(crate) mod latency;
pub(crate) mod main_output;
pub(crate) mod order;
pub(crate) mod plan;
pub(crate) mod preroll;
pub(crate) mod tail;
pub(crate) mod verify;

use std::collections::{BTreeMap, BTreeSet};

use crate::NodeKey;
use crate::NodeMix;
use crate::{Edge, EdgeId, FeedbackFrom, InputPort, OutputPort, PortSink, PortSource};
use tutti_buffers::ChannelLayout;
use tutti_units::{Latency, Samples, Tail};

use crate::compile::ids::{
    AudioFeedbackId, AudioSlotId, AudioValueId, DelayId, EventFeedbackId, EventSlotId,
    EventValueId, Id, OpId, PlanEventTapId, PlanMeterId, PlanTapId, PlanUnitId, Table, TableSlice,
};
use crate::compile::plan::{
    Csr, DelayKey, DelaySpec, Delta, EMPTY_SLOT, EventSlotCapacity, FeedbackKey, FeedbackSpec,
    GainPortOp, InputBound, NodeTables, Op, ParamPortOp, Placement, Plan, PlanUnit, Span, UnitIdx,
    Value, ZERO_SLOT,
};
use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort, ValidGraph};
use crate::node::param::ParamIn;
use crate::node::shape::Shape;
use crate::node::{InPlaceMask, MAX_PORTS, Prepare, Resolution, Wired};

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

/// Where an audio input reads from, before colouring.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ARef {
    Val(AudioValueId),
    Zero,
    Fb(AudioFeedbackId),
}

/// Where an event input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ERef {
    Val(EventValueId),
    Empty,
    Fb(EventFeedbackId),
}

/// An op before colouring: the same shape as [`Op`] with value ids where
/// slots will go.
enum Pre {
    MainInput {
        channel: u16,
        dst: AudioValueId,
    },
    Delay {
        delay: DelayId,
        src: AudioValueId,
        dst: AudioValueId,
    },
    Sum {
        srcs: Vec<ARef>,
        dst: AudioValueId,
    },
    /// A `Sum` with a weighted lane: `lanes` is one entry per addend.
    WeightedSum {
        srcs: Vec<ARef>,
        lanes: Vec<Option<EdgeId>>,
        /// The driven lanes among them.
        gains: Vec<GPort>,
        dst: AudioValueId,
    },
    EventDelay {
        delay: DelayId,
        src: EventValueId,
        dst: EventValueId,
    },
    EventMerge {
        srcs: Vec<ERef>,
        dst: EventValueId,
        bound: Option<InputBound>,
    },
    Node {
        unit: PlanUnitId,
        ain: Vec<ARef>,
        aout: Vec<AudioValueId>,
        ein: Vec<ERef>,
        eout: Vec<EventValueId>,
        params: Vec<PPort>,
    },
    Output {
        channel: u16,
        src: ARef,
        delay: Option<DelayId>,
    },
    Tap {
        tap: PlanTapId,
        srcs: Vec<ARef>,
    },
    EventTap {
        tap: PlanEventTapId,
        src: ERef,
    },
    Meter {
        meter: PlanMeterId,
        srcs: Vec<ARef>,
    },
    Capture {
        feedback: AudioFeedbackId,
        src: AudioValueId,
    },
    EventCapture {
        feedback: EventFeedbackId,
        src: EventValueId,
    },
}

/// A driven param port before colouring: its event source, and the event
/// value it reads.
struct PPort {
    port: u16,
    param: tutti_units::UnitAddr,
    from: EventOutputPort,
    value: EventValueId,
}

/// A driven lane of a weighted sum before colouring: which addend it is, its
/// event source, and the event value it reads (the sibling of [`PPort`]).
#[derive(Clone, Copy)]
struct GPort {
    lane: u16,
    from: EventOutputPort,
    value: EventValueId,
}

/// A value under construction.
pub(crate) struct Val {
    pub(crate) writer: OpId,
    pub(crate) readers: Vec<OpId>,
}

/// What the op-emission pass produces for the colouring pass.
struct Emitted {
    pre: Table<OpId, Pre>,
    audio: Table<AudioValueId, Val>,
    event: Table<EventValueId, Val>,
    preds: Table<OpId, BTreeSet<OpId>>,
    delays: Table<DelayId, DelaySpec>,
}

impl Emitted {
    /// The id the next op pushed gets.
    fn next_op(&self) -> OpId {
        OpId::from_index(self.pre.len())
    }

    fn push(&mut self, op: Pre) -> OpId {
        self.preds.push(BTreeSet::new());
        self.pre.push(op)
    }

    fn audio_value(&mut self, writer: OpId) -> AudioValueId {
        self.audio.push(Val {
            writer,
            readers: Vec::new(),
        })
    }

    fn event_value(&mut self, writer: OpId) -> EventValueId {
        self.event.push(Val {
            writer,
            readers: Vec::new(),
        })
    }

    /// Pushes the op that writes a new audio value, built from that value.
    fn push_audio(&mut self, op: impl FnOnce(AudioValueId) -> Pre) -> (OpId, AudioValueId) {
        let value = self.audio_value(self.next_op());
        (self.push(op(value)), value)
    }

    /// Pushes the op that writes a new event value, built from that value.
    fn push_event(&mut self, op: impl FnOnce(EventValueId) -> Pre) -> (OpId, EventValueId) {
        let value = self.event_value(self.next_op());
        (self.push(op(value)), value)
    }

    fn read_audio(&mut self, v: AudioValueId, op: OpId) {
        let w = self.audio[v].writer;
        self.audio[v].readers.push(op);
        self.preds[op].insert(w);
    }

    fn read_event(&mut self, v: EventValueId, op: OpId) {
        let w = self.event[v].writer;
        self.event[v].readers.push(op);
        self.preds[op].insert(w);
    }

    fn read_aref(&mut self, r: ARef, op: OpId) {
        if let ARef::Val(v) = r {
            self.read_audio(v, op);
        }
    }

    fn read_eref(&mut self, r: ERef, op: OpId) {
        if let ERef::Val(v) = r {
            self.read_event(v, op);
        }
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
    let keys: Table<PlanUnitId, NodeKey> = topology.nodes.keys().copied().collect();
    let dense: BTreeMap<NodeKey, PlanUnitId> =
        keys.iter_enumerated().map(|(n, &k)| (k, n)).collect();

    // ---- 1. shapes against the value -------------------------------------
    let mut node_shapes: Table<PlanUnitId, Shape> = Table::default();
    for (&key, spec) in &topology.nodes {
        let shape = *shapes
            .get(&key)
            .ok_or(CompileError::MissingShape { node: key })?;
        if (spec.inputs, spec.outputs) != (shape.audio_in(), shape.audio_out()) {
            return Err(CompileError::WidthMismatch {
                node: key,
                declared: (spec.inputs, spec.outputs),
                shape: (shape.audio_in(), shape.audio_out()),
            });
        }
        if spec.latency != shape.latency.samples() {
            return Err(CompileError::LatencyMismatch {
                node: key,
                declared: spec.latency,
                shape: shape.latency,
            });
        }
        if spec.tail != shape.tail {
            return Err(CompileError::TailMismatch {
                node: key,
                declared: spec.tail,
                shape: shape.tail,
            });
        }
        for count in [
            shape.audio_in().count() as usize,
            shape.audio_out().count() as usize,
            shape.event_in as usize,
            shape.event_out as usize,
        ] {
            if count > MAX_PORTS {
                return Err(CompileError::TooManyPorts { node: key, count });
            }
        }
        node_shapes.push(shape);
    }
    for (&at, sources) in graph.events() {
        if at.port >= node_shapes[dense[&at.node]].event_in {
            return Err(CompileError::EventPortOutOfRange { at, from: None });
        }
        for e in sources {
            let from = e.from();
            if from.port >= node_shapes[dense[&from.node]].event_out {
                return Err(CompileError::EventPortOutOfRange {
                    at,
                    from: Some(from),
                });
            }
        }
    }

    // Params: each is declared by its node, and each source is an output of
    // its own.
    for (&at, &from) in graph.params() {
        if at.param != NodeMix::ADDR
            && node_shapes[dense[&at.node]]
                .params
                .index_of(at.param)
                .is_none()
        {
            return Err(CompileError::UnknownParam { at });
        }
        if from.port >= node_shapes[dense[&from.node]].event_out {
            return Err(CompileError::ParamSourceOutOfRange { at, from });
        }
    }
    // Edge gains: each driving source is an output of its node.
    for (&id, gain) in graph.gains() {
        if let Some(from) = gain.source
            && from.port >= node_shapes[dense[&from.node]].event_out
        {
            return Err(CompileError::GainSourceOutOfRange {
                sink: id.sink,
                from,
            });
        }
    }

    // Resolution: a marked edge's sink must honour what it requires (see
    // `GraphSpec::require_resolution` for the rule, and why only marked edges).
    for (&(at, from), &required) in graph.required_resolution() {
        let sink = node_shapes[dense[&at.node]].event_resolution;
        if !sink.satisfies(required) {
            return Err(CompileError::ResolutionTooCoarse {
                at,
                from,
                required,
                sink,
            });
        }
    }

    // Feedback delays: at least one block, or a read would need the future.
    let max_block = prepare.max_block().samples();
    for (&at, e) in topology
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
        if let Edge::Feedback(f) = *e
            && f.delay < max_block
        {
            return Err(CompileError::FeedbackTooShort {
                edge: CycleEdge::Audio(at),
                delay: f.delay,
                max_block,
            });
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Feedback { from, delay } = *e
                && delay < max_block
            {
                return Err(CompileError::FeedbackTooShort {
                    edge: CycleEdge::Event { at, from },
                    delay,
                    max_block,
                });
            }
        }
    }

    // ---- 2. cycles --------------------------------------------------------
    // Direct dependencies, with multiplicity (for Kahn) and labelled (for the
    // error).
    // The cycle search runs over plain positions: `order`'s graphs are
    // generic over `usize`.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    let mut labelled: Vec<(usize, usize, CycleEdge)> = Vec::new();
    for (&at, edge) in topology
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
        if let Edge::Direct(PortSource::Node(p)) = *edge {
            let (s, d) = (dense[&p.node].index(), dense[&at.node].index());
            preds[d].push(s);
            labelled.push((s, d, CycleEdge::Audio(at)));
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Direct(from) = *e {
                let (s, d) = (dense[&from.node].index(), dense[&at.node].index());
                preds[d].push(s);
                labelled.push((s, d, CycleEdge::Event { at, from }));
            }
        }
    }
    for (&at, &from) in graph.params() {
        let (src, d) = (dense[&from.node].index(), dense[&at.node].index());
        preds[d].push(src);
        labelled.push((src, d, CycleEdge::Param { at, from }));
    }
    // A lane that drives the gain of an edge into a node's port runs before
    // the sum at that port, so before the node. (One into an output channel
    // feeds nothing a node reads.)
    for (&id, gain) in graph.gains() {
        if let (PortSink::Port(at), Some(from)) = (id.sink, gain.source) {
            let (src, d) = (dense[&from.node].index(), dense[&at.node].index());
            preds[d].push(src);
            labelled.push((src, d, CycleEdge::Gain { at, from }));
        }
    }
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    for (d, ps) in preds.iter().enumerate() {
        for &s in ps {
            succ[s].push(d);
        }
    }
    let comp = order::scc(&succ);
    let mut comp_size = vec![0usize; keys.len()];
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

    // ---- 3. order ---------------------------------------------------------
    let order = order::kahn(&preds);
    debug_assert!(order.cyclic.is_empty(), "the SCC pass left a cycle");
    let order: Vec<PlanUnitId> = order
        .order
        .into_iter()
        .map(PlanUnitId::from_index)
        .collect();

    // ---- 4. latency -------------------------------------------------------
    // One solve: `crate::delays` over the checked graph. The
    // compiler places its delays itself (event and param edges included) from
    // the solve's arrivals.
    let solved = crate::delays(graph, prepare.sample_rate());
    let arrival: Table<PlanUnitId, Latency> = keys
        .iter()
        .map(|&k| Latency::new(solved.arrival(k)))
        .collect();
    let departures: Table<PlanUnitId, Latency> = keys
        .iter()
        .map(|&k| Latency::new(solved.departure(k)))
        .collect();
    let departure = |n: PlanUnitId| departures[n];
    let total = Latency::new(solved.compensation().total());
    // The main output's channels: a main output change's old channels, after them, are
    // aligned like any output and report nothing outward.
    let rendered_outputs = topology.outputs.len();
    // An old main output's channels are aligned to the output time they were heard
    // at: the previous plan's total, or, for a plan that continues the
    // previous plan's crossfade, the alignment that plan kept. The new main output's
    // alignment does not move for the length of the fade and back after it.
    let bank_total = match (graph.main_output_bank(), prev) {
        (Some(bank), Some(p)) if p.main_output_bank.is_some_and(|b| b.old == bank.old) => {
            p.bank_total
        }
        (_, Some(p)) => p.total_latency,
        (_, None) => total,
    };
    let output_time = |channel: u16| match graph.main_output_bank() {
        Some(bank) if usize::from(channel) >= bank.base => bank_total,
        _ => total,
    };
    let main_output_width = graph
        .main_output_bank()
        .map_or(rendered_outputs, |b| b.base);
    let compensation: Vec<Samples> = (0..main_output_width)
        .map(|ch| solved.compensation().for_channel(ch))
        .collect();

    // ---- 5. emit ----------------------------------------------------------
    let audio_fb_key = |f: FeedbackFrom| FeedbackKey::Audio {
        from: f.from,
        generation: graph.generation(f.from.node),
        delay: f.delay,
    };
    let event_fb_key =
        |at: EventInputPort, from: EventOutputPort, delay: Samples| FeedbackKey::Event {
            at,
            from,
            generation: graph.generation(from.node),
            delay,
        };
    let audio_fb: Table<AudioFeedbackId, FeedbackKey> = topology
        .edges
        .values()
        .flatten()
        .filter_map(|e| match *e {
            Edge::Feedback(f) => Some(audio_fb_key(f)),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let event_fb: Table<EventFeedbackId, FeedbackKey> = graph
        .events()
        .iter()
        .flat_map(|(&at, sources)| {
            sources.iter().filter_map(move |e| match *e {
                EventEdge::Feedback { from, delay } => Some((at, from, delay)),
                EventEdge::Direct(_) => None,
            })
        })
        .map(|(at, from, delay)| event_fb_key(at, from, delay))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut em = Emitted {
        pre: Table::default(),
        audio: Table::default(),
        event: Table::default(),
        preds: Table::default(),
        delays: Table::default(),
    };

    // Main inputs actually read, each copied into a slot once.
    let mut main_input_val: BTreeMap<u16, AudioValueId> = BTreeMap::new();
    let used_main_inputs: BTreeSet<u16> = topology
        .edges
        .values()
        .flatten()
        .filter_map(|e| match *e {
            Edge::Direct(PortSource::MainInput(ch)) => Some(ch),
            _ => None,
        })
        .chain(topology.outputs.iter().flatten().filter_map(|s| match *s {
            PortSource::MainInput(ch) => Some(ch),
            _ => None,
        }))
        .collect();
    for ch in used_main_inputs {
        let (_, v) = em.push_audio(|dst| Pre::MainInput { channel: ch, dst });
        main_input_val.insert(ch, v);
    }

    let mut audio_out_val: BTreeMap<OutputPort, AudioValueId> = BTreeMap::new();
    let mut event_out_val: BTreeMap<EventOutputPort, EventValueId> = BTreeMap::new();
    // What each event input reads, after its sources' delays and merge: the
    // stream an event tap of an input copies.
    let mut event_in_val: BTreeMap<EventInputPort, ERef> = BTreeMap::new();

    for &n in &order {
        let key = keys[n];
        let shape = node_shapes[n];

        // Audio inputs: every source of a port through its own PDC delay
        // when it needs one, then the sum in **source order** (`Edge`'s `Ord`,
        // not the order the spec listed them in), so two specs that list the
        // same sources differently render alike. One source is the value itself, none is silence.
        let mut ain = Vec::with_capacity(shape.audio_in().count() as usize);
        for port in 0..shape.audio_in().count() {
            let at = InputPort::new(key, port);
            let mut sources: Vec<Edge> = topology.edges.get(&at).cloned().unwrap_or_default();
            sources.sort();
            let mut refs: Vec<Addend> = Vec::with_capacity(sources.len());
            for edge in sources {
                let r = match edge {
                    // Silence adds nothing, bit for bit (the sum starts at
                    // `+0.0`); `validate` refuses it in a set, so this is a
                    // raw write's.
                    Edge::Direct(PortSource::Zero) => continue,
                    Edge::Direct(from @ (PortSource::MainInput(_) | PortSource::Node(_))) => {
                        // A main input arrives at zero: it is a merge-point
                        // source like any other (see the module docs, step 4).
                        let (v, dep) = match from {
                            PortSource::MainInput(ch) => (main_input_val[&ch], Latency::ZERO),
                            PortSource::Node(p) => (audio_out_val[&p], departure(dense[&p.node])),
                            PortSource::Zero => unreachable!("matched above"),
                        };
                        align_audio(
                            &mut em,
                            v,
                            dep.gap_to(arrival[n]),
                            DelayKey::Audio { at, from },
                        )
                    }
                    Edge::Feedback(f) => ARef::Fb(fb_index(audio_fb_key(f), &audio_fb)),
                };
                // An edge with a gain entry is a weighted lane.
                let id = EdgeId {
                    sink: PortSink::Port(at),
                    from: edge,
                };
                let gain = graph.gains().get(&id);
                // Its driving lane, aligned to this node's arrival like a
                // param's source (a silent addend has none: nothing is added).
                let drive = gain
                    .and_then(|g| g.source)
                    .filter(|_| r != ARef::Zero)
                    .map(|from| {
                        let by = departure(dense[&from.node]).gap_to(arrival[n]);
                        GainDrive {
                            from,
                            value: align_event(
                                &mut em,
                                event_out_val[&from],
                                by,
                                DelayKey::EdgeGain { id, from },
                            ),
                        }
                    });
                refs.push(Addend {
                    r,
                    lane: gain.map(|_| id),
                    drive,
                });
            }
            ain.push(sum_chain(&mut em, refs));
        }

        // Event inputs: delay each source that needs it, then merge fan-in
        // in **source order**, `(source NodeKey, source port)` — not the
        // order the edges were listed in. The
        // tie-break at equal offsets is then a property of the wiring alone:
        // two specs that list the same sources differently merge alike, so
        // a host that rebuilds its spec from an unordered store (the ECS)
        // cannot reorder a chord by accident. Sources are distinct
        // (`GraphSpec::validate`), so the order is total.
        let mut ein = Vec::with_capacity(shape.event_in as usize);
        for port in 0..shape.event_in {
            let at = EventInputPort::new(key, port);
            let mut sources: Vec<EventEdge> = graph.events().get(&at).cloned().unwrap_or_default();
            sources.sort_by_key(|e| e.from());
            let mut refs: Vec<ERef> = Vec::with_capacity(sources.len());
            for e in &sources {
                refs.push(match *e {
                    EventEdge::Feedback { from, delay } => {
                        ERef::Fb(fb_index(event_fb_key(at, from, delay), &event_fb))
                    }
                    EventEdge::Direct(from) => ERef::Val(align_event(
                        &mut em,
                        event_out_val[&from],
                        departure(dense[&from.node]).gap_to(arrival[n]),
                        DelayKey::Event { at, from },
                    )),
                });
            }
            // A lone undelayed writer whose port cannot hold more than the
            // bound needs no merge to keep it.
            let within = |e: &EventEdge| match *e {
                EventEdge::Direct(from) => {
                    refs[..] == [ERef::Val(event_out_val[&from])]
                        && node_shapes[dense[&from.node]]
                            .event_capacity
                            .is_some_and(|c| {
                                shape.event_input_capacity.is_some_and(|b| {
                                    crate::event::writer::note_off_room(c.get() as usize)
                                        <= b.get() as usize
                                })
                            })
                }
                EventEdge::Feedback { .. } => false,
            };
            let bound = shape
                .event_input_capacity
                .filter(|_| !(sources.len() == 1 && within(&sources[0])))
                // The public bound names its unit by raw index until the plan
                // is narrowed.
                .map(|capacity| InputBound {
                    capacity,
                    unit: n.get(),
                });
            let merged = merge_tree(&mut em, refs, bound);
            event_in_val.insert(at, merged);
            ein.push(merged);
        }

        // Driven params, in port order: each source through its PDC delay
        // when it needs one, exactly as an input is (see the `param` module
        // docs).
        let mut params: Vec<PPort> = Vec::new();
        for (k, &param) in shape.params.as_slice().iter().enumerate() {
            let at = ParamIn { node: key, param };
            let Some(e) = graph.params().get(&at).copied() else {
                continue;
            };
            let value = align_event(
                &mut em,
                event_out_val[&e],
                departure(dense[&e.node]).gap_to(arrival[n]),
                DelayKey::ParamEvent { at, from: e },
            );
            params.push(PPort {
                port: k as u16,
                param,
                from: e,
                value,
            });
        }
        // The node's mix, last: its port is past every declared index. Its
        // source is aligned like any param's (the same key kind: the mix is a
        // `ParamIn` of the node).
        let at = ParamIn {
            node: key,
            param: NodeMix::ADDR,
        };
        if let Some(e) = graph.params().get(&at).copied() {
            let value = align_event(
                &mut em,
                event_out_val[&e],
                departure(dense[&e.node]).gap_to(arrival[n]),
                DelayKey::ParamEvent { at, from: e },
            );
            params.push(PPort {
                port: ParamPortOp::MIX_PORT,
                param: NodeMix::ADDR,
                from: e,
                value,
            });
        }
        let reads: Vec<EventValueId> = params.iter().map(|p| p.value).collect();

        let at = em.next_op();
        let aout: Vec<AudioValueId> = (0..shape.audio_out().count())
            .map(|port| {
                let v = em.audio_value(at);
                audio_out_val.insert(OutputPort::new(key, port), v);
                v
            })
            .collect();
        let eout: Vec<EventValueId> = (0..shape.event_out)
            .map(|port| {
                let v = em.event_value(at);
                event_out_val.insert(EventOutputPort::new(key, port), v);
                v
            })
            .collect();
        let op = em.push(Pre::Node {
            unit: n,
            ain: ain.clone(),
            aout,
            ein: ein.clone(),
            eout,
            params,
        });
        debug_assert_eq!(op, at);
        for &r in &ain {
            em.read_aref(r, op);
        }
        for &r in &ein {
            em.read_eref(r, op);
        }
        for v in reads {
            em.read_event(v, op);
        }
    }

    // A main output meter or an outputs tap reads each output channel as it leaves
    // the alignment delay, so with one declared the delay is its own op
    // writing a slot that the `Output` op and the reader both read (one ring,
    // the same key).
    let main_output_meter = topology
        .meters
        .values()
        .any(|spec| matches!(spec, crate::MeterSpec::Outputs))
        || topology
            .taps
            .values()
            .any(|spec| matches!(spec, crate::TapSpec::Outputs { .. }));
    let mut output_refs: Vec<ARef> = Vec::with_capacity(topology.outputs.len());
    for (channel, sources) in topology.outputs.iter().enumerate() {
        let channel = channel as u16;
        let mut sources: Vec<PortSource> = sources
            .iter()
            .copied()
            .filter(|s| *s != PortSource::Zero)
            .collect();
        sources.sort();
        let lane_of = |source: PortSource| {
            let id = EdgeId {
                sink: PortSink::Output(channel),
                from: Edge::Direct(source),
            };
            graph.gains().contains_key(&id).then_some(id)
        };
        if sources.len() > 1 || sources.iter().any(|&s| lane_of(s).is_some()) {
            // A summed channel (or one whose source is weighted): each source
            // is delayed to the output time on its own, as an op of its own,
            // then summed; the `Output` op carries no ring.
            let refs: Vec<Addend> = sources
                .iter()
                .map(|&source| {
                    let (v, dep) = match source {
                        PortSource::Node(p) => (audio_out_val[&p], departure(dense[&p.node])),
                        PortSource::MainInput(ch) => (main_input_val[&ch], Latency::ZERO),
                        PortSource::Zero => unreachable!("filtered above"),
                    };
                    let r = align_audio(
                        &mut em,
                        v,
                        dep.gap_to(output_time(channel)),
                        DelayKey::Output {
                            channel,
                            from: source,
                        },
                    );
                    let lane = lane_of(source);
                    // Its driving lane, aligned to the output time.
                    let drive = lane
                        .and_then(|id| Some((id, graph.gains().get(&id)?.source?)))
                        .map(|(id, from)| GainDrive {
                            from,
                            value: align_event(
                                &mut em,
                                event_out_val[&from],
                                departure(dense[&from.node]).gap_to(output_time(channel)),
                                DelayKey::EdgeGain { id, from },
                            ),
                        });
                    Addend { r, lane, drive }
                })
                .collect();
            let src = sum_chain(&mut em, refs);
            output_refs.push(src);
            let op = em.push(Pre::Output {
                channel,
                src,
                delay: None,
            });
            em.read_aref(src, op);
            continue;
        }
        let source = &sources.first().copied().unwrap_or(PortSource::Zero);
        let mut src = match *source {
            PortSource::Node(p) => ARef::Val(audio_out_val[&p]),
            PortSource::MainInput(ch) => ARef::Val(main_input_val[&ch]),
            PortSource::Zero => ARef::Zero,
        };
        let d = match (graph.main_output_bank(), source) {
            (Some(bank), _) if usize::from(channel) >= bank.base => match source {
                PortSource::Node(p) => departure(dense[&p.node]).gap_to(bank_total),
                PortSource::MainInput(_) => Latency::ZERO.gap_to(bank_total),
                PortSource::Zero => Samples::ZERO,
            },
            _ => solved.compensation().for_channel(usize::from(channel)),
        };
        // Delaying silence is silence: a zero channel reports its
        // compensation (as `latency::plan` does) but gets no ring.
        let key = DelayKey::Output {
            channel,
            from: *source,
        };
        let mut delay = None;
        if !d.is_zero() {
            match src {
                ARef::Zero => {}
                ARef::Val(v) if main_output_meter => src = align_audio(&mut em, v, d, key),
                _ => {
                    delay = Some(em.delays.push(DelaySpec { key, len: d }));
                }
            }
        }
        output_refs.push(src);
        let op = em.push(Pre::Output {
            channel,
            src,
            delay,
        });
        em.read_aref(src, op);
    }

    // Taps: one lane per source, any ports. An aligned lane is delayed to the
    // graph's output time (the compensation an output of the same source
    // gets); a raw one reads the source as it leaves its node. A closed tap
    // is not in the topology, so nothing here is emitted for it. A lane whose
    // port does not exist (a node replaced by a narrower one, a removed node,
    // a main input past the declared width) reads silence rather than
    // failing the commit, as a meter's does; it gets no delay ring, and the
    // other lanes' rings are keyed by their own lane and source.
    let mut tap_ids: Table<PlanTapId, crate::TapId> = Table::default();
    for (&tap, spec) in &topology.taps {
        let (sources, align) = match spec {
            crate::TapSpec::Outputs { width } => {
                // What each output channel carries, post-sum and aligned
                // (above); a lane past the output count is silence.
                let srcs: Vec<ARef> = (0..*width)
                    .map(|c| {
                        output_refs
                            .get(c)
                            .filter(|_| c < main_output_width)
                            .copied()
                            .unwrap_or(ARef::Zero)
                    })
                    .collect();
                let op = em.push(Pre::Tap {
                    tap: tap_ids.push(tap),
                    srcs: srcs.clone(),
                });
                for src in srcs {
                    em.read_aref(src, op);
                }
                continue;
            }
            crate::TapSpec::Ports { sources, align } => (sources, *align),
        };
        let mut srcs = Vec::with_capacity(sources.len());
        for (lane, source) in sources.iter().enumerate() {
            let (mut src, dep) = match *source {
                PortSource::Node(p) => match (audio_out_val.get(&p), dense.get(&p.node)) {
                    (Some(&v), Some(&n)) => (ARef::Val(v), departure(n)),
                    _ => (ARef::Zero, Latency::ZERO),
                },
                PortSource::MainInput(ch) => match main_input_val.get(&ch) {
                    Some(&v) => (ARef::Val(v), Latency::ZERO),
                    None => (ARef::Zero, Latency::ZERO),
                },
                PortSource::Zero => (ARef::Zero, Latency::ZERO),
            };
            if align == crate::TapAlign::ToOutput && src != ARef::Zero {
                let ARef::Val(v) = src else {
                    unreachable!("not zero")
                };
                let key = DelayKey::Tap {
                    tap,
                    lane,
                    from: *source,
                };
                src = align_audio(&mut em, v, dep.gap_to(total), key);
            }
            srcs.push(src);
        }
        // One op a tap, after every lane's delay: it writes the lanes together.
        let op = em.push(Pre::Tap {
            tap: tap_ids.push(tap),
            srcs: srcs.clone(),
        });
        for src in srcs {
            em.read_aref(src, op);
        }
    }

    // Event taps: one event output each. An aligned tap is delayed to the
    // graph's output time (the compensation an event edge into an output-time
    // sink would get); a raw one reads the port as it leaves its node. A closed
    // tap is not in the topology, so nothing is emitted for it. A tap whose
    // port does not exist (a node replaced by one with fewer event outputs, a
    // removed node) keeps its place in the table, so its writer is carried and
    // resumes if the port returns, but it gets no op and no delay ring: it
    // reads nothing and the commit succeeds. One op a tap, after its delay.
    let mut event_tap_ids: Table<PlanEventTapId, crate::EventTapId> = Table::default();
    for (&tap, spec) in &topology.event_taps {
        let index = event_tap_ids.push(tap);
        let Some(from) = spec.source else {
            continue;
        };
        // What the port carries, and when it departs (an output) or arrives
        // (an input, which an aligned tap delays to the output time too).
        let (carried, at_time) = match from {
            crate::EventTapPort::Out(port) => {
                let (Some(&v), Some(&n)) = (event_out_val.get(&port), dense.get(&port.node)) else {
                    continue;
                };
                (ERef::Val(v), departure(n))
            }
            crate::EventTapPort::In(at) => {
                let (Some(&r), Some(&n)) = (event_in_val.get(&at), dense.get(&at.node)) else {
                    continue;
                };
                (r, arrival[n])
            }
        };
        if carried == ERef::Empty {
            continue;
        }
        let mut src = carried;
        // A feedback edge's slot is read as the node reads it: it has no
        // value to delay.
        if let (crate::TapAlign::ToOutput, ERef::Val(v)) = (spec.align, carried) {
            src = ERef::Val(align_event(
                &mut em,
                v,
                at_time.gap_to(total),
                DelayKey::EventTap { tap, from },
            ));
        }
        let op = em.push(Pre::EventTap { tap: index, src });
        em.read_eref(src, op);
    }

    // Meters: one lane per source. `Outputs` reads what each output channel
    // carries (after its alignment delay, above); `Ports` reads each source as
    // it leaves its node, or, aligned, delayed to the output time by a ring of
    // its own like a tap's. A lane whose port does not exist (a node replaced
    // by a narrower one, a removed node) reads silence rather than failing the
    // commit. One op a meter, so its lanes are folded together. A closed meter
    // is not in the topology.
    let mut meter_ids: Table<PlanMeterId, (crate::MeterId, usize)> = Table::default();
    for (&meter, spec) in &topology.meters {
        let srcs: Vec<ARef> = match spec {
            crate::MeterSpec::Outputs => output_refs[..main_output_width].to_vec(),
            crate::MeterSpec::Ports { sources, align } => {
                let mut srcs = Vec::with_capacity(sources.len());
                for (lane, source) in sources.iter().enumerate() {
                    let (mut src, dep) = match *source {
                        PortSource::Node(p) => match (audio_out_val.get(&p), dense.get(&p.node)) {
                            (Some(&v), Some(&n)) => (ARef::Val(v), departure(n)),
                            _ => (ARef::Zero, Latency::ZERO),
                        },
                        PortSource::MainInput(ch) => match main_input_val.get(&ch) {
                            Some(&v) => (ARef::Val(v), Latency::ZERO),
                            None => (ARef::Zero, Latency::ZERO),
                        },
                        PortSource::Zero => (ARef::Zero, Latency::ZERO),
                    };
                    if *align == crate::TapAlign::ToOutput
                        && let ARef::Val(v) = src
                    {
                        let key = DelayKey::Meter {
                            meter,
                            lane,
                            from: *source,
                        };
                        src = align_audio(&mut em, v, dep.gap_to(total), key);
                    }
                    srcs.push(src);
                }
                srcs
            }
        };
        let op = em.push(Pre::Meter {
            meter: meter_ids.push((meter, srcs.len())),
            srcs: srcs.clone(),
        });
        for src in srcs {
            em.read_aref(src, op);
        }
    }

    // Captures run last. They feed the feedback delays, which the executor
    // reads from *before* the block (into the feedback slots), so a capture
    // needs no ordering against the feedback's readers: with a delay of a
    // whole `MaxBlock`, nothing it queues is due this block.
    for (feedback, key) in audio_fb.iter_enumerated() {
        let FeedbackKey::Audio { from, .. } = *key else {
            unreachable!("audio feedback keys are audio")
        };
        let src = audio_out_val[&from];
        let op = em.push(Pre::Capture { feedback, src });
        em.read_audio(src, op);
    }
    for (feedback, key) in event_fb.iter_enumerated() {
        let FeedbackKey::Event { from, .. } = *key else {
            unreachable!("event feedback keys are events")
        };
        let src = event_out_val[&from];
        let op = em.push(Pre::EventCapture { feedback, src });
        em.read_event(src, op);
    }

    // ---- 6. colour --------------------------------------------------------
    let op_count = em.pre.len();
    let mut succ_rows: Table<OpId, Vec<OpId>> = Table::filled(op_count, Vec::new());
    for (op, ps) in em.preds.iter_enumerated() {
        for &p in ps {
            debug_assert!(p < op, "ops are emitted in topological order");
            succ_rows[p].push(op);
        }
    }
    let reach = colour::Reach::new(&succ_rows);

    // In-place candidates: (value written, value it may overwrite).
    let mut in_place: BTreeMap<AudioValueId, AudioValueId> = BTreeMap::new();
    for pre in &em.pre {
        match pre {
            Pre::Delay { src, dst, .. } => {
                in_place.insert(*dst, *src);
            }
            // The sum accumulates into its first addend's slot when that
            // addend is read nowhere else later (the colouring's own test).
            Pre::Sum { srcs, dst } | Pre::WeightedSum { srcs, dst, .. } => {
                if let Some(&ARef::Val(u)) = srcs.first() {
                    in_place.insert(*dst, u);
                }
            }
            Pre::Node {
                unit, ain, aout, ..
            } if node_shapes[*unit].in_place => {
                for (&r, &out) in ain.iter().zip(aout) {
                    let ARef::Val(u) = r else { continue };
                    // Read by this node on exactly one port, or the other
                    // port would see the output instead of the input.
                    if ain.iter().filter(|&&x| x == r).count() != 1 {
                        continue;
                    }
                    in_place.insert(out, u);
                }
            }
            _ => {}
        }
    }

    // Slot 0 of each arena is the null slot, then one slot per feedback.
    let audio_fixed = 1 + audio_fb.len();
    let event_fixed = 1 + event_fb.len();
    let audio_colour = colour::colour(&em.audio, &reach, &in_place);
    let event_colour = colour::colour(&em.event, &reach, &BTreeMap::new());
    let aslot = |v: AudioValueId| AudioSlotId::from_index(audio_fixed + audio_colour.slot[v]);
    let eslot = |v: EventValueId| EventSlotId::from_index(event_fixed + event_colour.slot[v]);

    // Event slot capacities (`EventSlotCapacity`): a node output holds what
    // its shape declares, a delay output (and a feedback slot) all its FIFO
    // can hold at its source's declared rate — what can fall due in one
    // block — and a merge all its inputs together, so a merge can never drop
    // an event (a note-off least of all).
    // Emitted in topological order, so a merge's inputs are priced first.
    let block = prepare.max_block().get();
    let fb_cap = |key: &FeedbackKey| match *key {
        FeedbackKey::Event { from, delay, .. } => {
            EventSlotCapacity::fifo(node_shapes[dense[&from.node]].event_capacity, delay, block)
        }
        _ => unreachable!("event feedback keys are events"),
    };
    let mut value_cap: Table<EventValueId, EventSlotCapacity> =
        Table::filled(em.event.len(), EventSlotCapacity::NONE);
    for pre in &em.pre {
        match pre {
            Pre::Node { unit, eout, .. } => {
                let port = EventSlotCapacity::port(node_shapes[*unit].event_capacity);
                for &v in eout {
                    value_cap[v] = port;
                }
            }
            Pre::EventDelay { delay, dst, .. } => {
                let d = &em.delays[*delay];
                let Some(from) = d.key.event_origin() else {
                    unreachable!("an event delay is keyed as events")
                };
                // An output's own capacity, or, for an input, every port with an
                // edge into it.
                let ports: Vec<EventOutputPort> = match from {
                    crate::EventTapPort::Out(port) => vec![port],
                    crate::EventTapPort::In(at) => graph
                        .events()
                        .get(&at)
                        .map(|edges| edges.iter().map(|e| e.from()).collect())
                        .unwrap_or_default(),
                };
                value_cap[*dst] = EventSlotCapacity::fifo_sum(
                    ports
                        .iter()
                        .map(|p| node_shapes[dense[&p.node]].event_capacity),
                    d.len,
                    block,
                );
            }
            Pre::EventMerge { srcs, dst, bound } => {
                value_cap[*dst] = srcs
                    .iter()
                    .fold(EventSlotCapacity::NONE, |acc, r| {
                        acc.plus(match *r {
                            ERef::Val(v) => value_cap[v],
                            ERef::Fb(f) => fb_cap(&event_fb[f]),
                            ERef::Empty => EventSlotCapacity::NONE,
                        })
                    })
                    .bounded(bound.map(|b| b.capacity));
            }
            _ => {}
        }
    }
    let mut event_slot_capacity: Table<EventSlotId, EventSlotCapacity> =
        Table::filled(event_fixed + event_colour.count, EventSlotCapacity::NONE);
    for (f, key) in event_fb.iter_enumerated() {
        event_slot_capacity[f.slot()] = fb_cap(key);
    }
    for (v, &c) in value_cap.iter_enumerated() {
        let s = eslot(v);
        event_slot_capacity[s] = event_slot_capacity[s].covering(c);
    }
    let aref_slot = |r: ARef| match r {
        ARef::Val(v) => aslot(v),
        ARef::Zero => ZERO_SLOT,
        ARef::Fb(f) => f.slot(),
    };
    let eref_slot = |r: ERef| match r {
        ERef::Val(v) => eslot(v),
        ERef::Empty => EMPTY_SLOT,
        ERef::Fb(f) => f.slot(),
    };

    // ---- lower ------------------------------------------------------------
    // The public `Op`, `Value` and the port structs name slots, units, delays
    // and ops by raw `u32` until the plan is narrowed; each is `Id::get` here.
    let mut audio_list: Vec<AudioSlotId> = Vec::new();
    let mut event_list: Vec<EventSlotId> = Vec::new();
    let mut param_ports: Vec<ParamPortOp> = Vec::new();
    let mut sum_lanes: Vec<Option<EdgeId>> = Vec::new();
    let mut gain_ports: Vec<GainPortOp> = Vec::new();
    let ops: Table<OpId, Op> = em
        .pre
        .iter()
        .map(|pre| match pre {
            Pre::MainInput { channel, dst } => Op::MainInput {
                channel: *channel,
                dst: aslot(*dst).get(),
            },
            Pre::Delay { delay, src, dst } => Op::Delay {
                delay: delay.get(),
                src: aslot(*src).get(),
                dst: aslot(*dst).get(),
            },
            Pre::Sum { srcs, dst } => Op::Sum {
                srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
                dst: aslot(*dst).get(),
            },
            Pre::WeightedSum {
                srcs,
                lanes,
                gains,
                dst,
            } => {
                let lanes = Span::extend(&mut sum_lanes, lanes.iter().copied());
                let gains = Span::extend(
                    &mut gain_ports,
                    gains.iter().map(|g| GainPortOp {
                        lane: g.lane,
                        from: g.from,
                        slot: eslot(g.value).get(),
                    }),
                );
                Op::WeightedSum {
                    srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
                    dst: aslot(*dst).get(),
                    lanes,
                    gains,
                }
            }
            Pre::EventDelay { delay, src, dst } => Op::EventDelay {
                delay: delay.get(),
                src: eslot(*src).get(),
                dst: eslot(*dst).get(),
            },
            Pre::EventMerge { srcs, dst, bound } => Op::EventMerge {
                srcs: Span::extend(&mut event_list, srcs.iter().map(|&r| eref_slot(r))),
                dst: eslot(*dst).get(),
                bound: *bound,
            },
            Pre::Node {
                unit,
                ain,
                aout,
                ein,
                eout,
                params,
            } => {
                let params = Span::extend(
                    &mut param_ports,
                    params.iter().map(|p| ParamPortOp {
                        port: p.port,
                        param: p.param,
                        from: p.from,
                        slot: eslot(p.value).get(),
                    }),
                );
                let mask = ain
                    .iter()
                    .zip(aout)
                    .enumerate()
                    .filter(|&(_, (&r, &o))| matches!(r, ARef::Val(u) if aslot(u) == aslot(o)))
                    .fold(InPlaceMask::NONE, |mask, (c, _)| mask.with(c));
                Op::Node {
                    unit: unit.get(),
                    audio_in: Span::extend(&mut audio_list, ain.iter().map(|&r| aref_slot(r))),
                    audio_out: Span::extend(&mut audio_list, aout.iter().map(|&v| aslot(v))),
                    event_in: Span::extend(&mut event_list, ein.iter().map(|&r| eref_slot(r))),
                    event_out: Span::extend(&mut event_list, eout.iter().map(|&v| eslot(v))),
                    in_place: mask,
                    params,
                }
            }
            Pre::Output {
                channel,
                src,
                delay,
            } => Op::Output {
                channel: *channel,
                src: aref_slot(*src).get(),
                delay: delay.map(DelayId::get),
            },
            Pre::Tap { tap, srcs } => Op::Tap {
                tap: tap.get(),
                srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
            },
            Pre::EventTap { tap, src } => Op::EventTap {
                tap: tap.get(),
                src: eref_slot(*src).get(),
            },
            Pre::Meter { meter, srcs } => Op::Meter {
                meter: meter.get(),
                srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
            },
            Pre::Capture { feedback, src } => Op::Capture {
                feedback: feedback.get(),
                src: aslot(*src).get(),
            },
            Pre::EventCapture { feedback, src } => Op::EventCapture {
                feedback: feedback.get(),
                src: eslot(*src).get(),
            },
        })
        .collect();

    let mut value_readers: Vec<OpId> = Vec::new();
    let audio_values = lower_values(&em.audio, &mut value_readers, |v| aslot(v).get());
    let event_values = lower_values(&em.event, &mut value_readers, |v| eslot(v).get());

    // ---- 7. coarsen, and price the plan (the cost model) -----------------
    let op_work: Table<OpId, crate::node::cost::Work> = ops
        .iter()
        .map(|op| crate::node::cost::op_work(op, &node_shapes, &sum_lanes))
        .collect();
    let coarse = coarsen::coarsen(
        &em.preds,
        &succ_rows,
        &op_work,
        prepare.max_block().samples(),
    );

    // ---- 9. place ---------------------------------------------------------
    let prev_units: BTreeMap<NodeKey, PlanUnit> = prev
        .map(|p| p.units.iter().map(|u| (u.key, *u)).collect())
        .unwrap_or_default();
    let mut delta = Delta::default();
    let mut idx_of: BTreeMap<NodeKey, UnitIdx> = BTreeMap::new();
    let mut taken: BTreeSet<UnitIdx> = BTreeSet::new();
    for &key in &keys {
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
    for &key in &keys {
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
    let units: Table<PlanUnitId, PlanUnit> = keys
        .iter_enumerated()
        .map(|(n, &key)| PlanUnit {
            key,
            generation: graph.generation(key),
            idx: idx_of[&key],
            shape: node_shapes[n],
            arrival: arrival[n],
            wired: wired_ports(topology, key, &node_shapes[n], &consumed),
        })
        .collect();

    let nodes = NodeTables::lower(ops.as_slice(), &audio_list, &event_list, &units);
    let plan = Plan {
        prepare: *prepare,
        ops,
        audio_list,
        event_list,
        op_succ: Csr::from_ids(&succ_rows),
        tasks: coarse.tasks,
        task_ops: coarse.task_ops,
        task_succ: coarse.succ,
        task_activation: coarse.activation,
        audio_slots: AudioSlotId::from_index(audio_fixed + audio_colour.count).get(),
        event_slots: EventSlotId::from_index(event_fixed + event_colour.count).get(),
        event_slot_capacity,
        audio_feedback: audio_fb
            .iter_enumerated()
            .map(|(f, &key)| FeedbackSpec {
                key,
                slot: f.slot().get(),
            })
            .collect(),
        event_feedback: event_fb
            .iter_enumerated()
            .map(|(f, &key)| FeedbackSpec {
                key,
                slot: f.slot().get(),
            })
            .collect(),
        delays: em.delays,
        taps: tap_ids,
        event_taps: event_tap_ids,
        meters: meter_ids,
        event_edges: {
            let mut v: Vec<(EventOutputPort, EventInputPort)> = graph
                .events()
                .iter()
                .flat_map(|(&at, edges)| edges.iter().map(move |e| (e.from(), at)))
                .collect();
            v.sort();
            v
        },
        units,
        store_len,
        order: order.iter().map(|&n| keys[n]).collect(),
        main_inputs: topology.inputs.count(),
        compensation,
        main_output_id: crate::MainOutputId::GENESIS,
        main_output_bank: graph.main_output_bank(),
        rendered_outputs,
        total_latency: total,
        bank_total,
        audio_values,
        event_values,
        value_readers,
        param_ports,
        gain_ports,
        sum_lanes,
        nodes,
    };

    // ---- 8. verify --------------------------------------------------------
    #[cfg(debug_assertions)]
    if let Err(e) = verify::verify(&plan) {
        panic!("tutti-graph produced an unsound plan: {e}");
    }

    Ok((plan, delta))
}

/// The position of `key` in the sorted feedback `list`.
fn fb_index<F: Id>(key: FeedbackKey, list: &TableSlice<F, FeedbackKey>) -> F {
    F::from_index(
        list.as_slice()
            .binary_search(&key)
            .expect("feedback keys are collected before they are read"),
    )
}

/// The public [`Value`] table for `vals`, each reader list appended to
/// `readers`.
fn lower_values<V: Id>(
    vals: &TableSlice<V, Val>,
    readers: &mut Vec<OpId>,
    slot: impl Fn(V) -> u32,
) -> Table<V, Value> {
    vals.iter_enumerated()
        .map(|(i, v)| Value {
            slot: slot(i),
            writer: v.writer.get(),
            readers: Span::extend(readers, v.readers.iter().copied()),
        })
        .collect()
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

/// `v` through its PDC delay of `by` frames, keyed `key`: the value itself
/// when `by` is zero (no op), else the output of a `Pre::Delay` ring.
fn align_audio(em: &mut Emitted, v: AudioValueId, by: Samples, key: DelayKey) -> ARef {
    if by.is_zero() {
        return ARef::Val(v);
    }
    let delay = em.delays.push(DelaySpec { key, len: by });
    let (op, dv) = em.push_audio(|dst| Pre::Delay { delay, src: v, dst });
    em.read_audio(v, op);
    ARef::Val(dv)
}

/// Event value `v`, delayed by `by` through a ring keyed `key` when `by` is
/// not zero (the value itself otherwise).
fn align_event(em: &mut Emitted, v: EventValueId, by: Samples, key: DelayKey) -> EventValueId {
    if by.is_zero() {
        return v;
    }
    let delay = em.delays.push(DelaySpec { key, len: by });
    let (op, dv) = em.push_event(|dst| Pre::EventDelay { delay, src: v, dst });
    em.read_event(v, op);
    dv
}

/// One addend of a sum: the value, and the edge whose gain weights it when
/// the edge has an entry.
#[derive(Clone, Copy)]
struct Addend {
    r: ARef,
    lane: Option<EdgeId>,
    /// The event source that drives the lane's gain, and the event value
    /// that carries it to the sum (through its PDC delay when it needs one).
    drive: Option<GainDrive>,
}

/// The event source that drives an addend's lane, and the event value that
/// carries it to the sum.
#[derive(Clone, Copy)]
struct GainDrive {
    from: EventOutputPort,
    value: EventValueId,
}

/// Sum `refs` (already in source order) into one audio value: the left fold
/// `((+0.0 + a_0) + a_1) + ..`, each
/// weighted addend `acc + w * a`.
///
/// Silence (`ARef::Zero`) adds nothing and is dropped; none left is silence,
/// one plain one is itself (an alias, no op). A weighted one is an op even
/// alone (`+0.0 + w * a`: a plain alias keeps a `-0.0` and a weight does not).
/// Up to [`MAX_PORTS`] addends are one op. A wider fan-in is a **chain, not a
/// tree**: a float sum is not associative, so the first op folds the first
/// `MAX_PORTS` and each next one reads the last result as its first addend
/// (plain: its weight is `1.0`) and adds the following `MAX_PORTS - 1`. The
/// accumulator is never `-0.0`, so adding it to `+0.0` again changes no bit:
/// the chain is one flat fold, wherever it is cut. An op with no weighted
/// lane is a plain `Op::Sum`.
fn sum_chain(em: &mut Emitted, refs: Vec<Addend>) -> ARef {
    let refs: Vec<Addend> = refs.into_iter().filter(|a| a.r != ARef::Zero).collect();
    let emit = |em: &mut Emitted, addends: Vec<Addend>| -> ARef {
        let srcs: Vec<ARef> = addends.iter().map(|a| a.r).collect();
        let lanes: Vec<Option<EdgeId>> = addends.iter().map(|a| a.lane).collect();
        let gains: Vec<GPort> = addends
            .iter()
            .enumerate()
            .filter_map(|(lane, a)| {
                a.drive.map(|d| GPort {
                    lane: lane as u16,
                    from: d.from,
                    value: d.value,
                })
            })
            .collect();
        let weighted = lanes.iter().any(Option::is_some);
        let (op, v) = em.push_audio(|dst| {
            if weighted {
                Pre::WeightedSum {
                    srcs: srcs.clone(),
                    lanes,
                    gains: gains.clone(),
                    dst,
                }
            } else {
                Pre::Sum {
                    srcs: srcs.clone(),
                    dst,
                }
            }
        });
        for &r in &srcs {
            em.read_aref(r, op);
        }
        // The sum reads each driving lane's events: a read of the op, so the
        // colouring and the verifier cover it.
        for g in &gains {
            em.read_event(g.value, op);
        }
        ARef::Val(v)
    };
    match refs.len() {
        0 => ARef::Zero,
        1 if refs[0].lane.is_none() => refs[0].r,
        len => {
            let first = len.min(MAX_PORTS);
            let mut acc = emit(em, refs[..first].to_vec());
            for run in refs[first..].chunks(MAX_PORTS - 1) {
                let mut addends = Vec::with_capacity(run.len() + 1);
                addends.push(Addend {
                    r: acc,
                    lane: None,
                    drive: None,
                });
                addends.extend_from_slice(run);
                acc = emit(em, addends);
            }
            acc
        }
    }
}

/// Merge `refs` (already in source order) into one event stream, in
/// `(offset, source order)`.
///
/// Up to [`MAX_PORTS`] sources is one `EventMerge` op. Wider fan-in becomes a
/// tree: contiguous runs of at most `MAX_PORTS` sources merge first, and the
/// run results merge in run order. Because each merge breaks ties toward the
/// lower input and the runs are contiguous and in order, a tie between
/// sources in different runs still goes to the earlier source — the tree
/// delivers exactly what one flat merge would.
///
/// With a `bound` (the node's `Shape::event_input_capacity`) the last merge
/// keeps at most that many events, and a single source goes through it too:
/// the bound holds whatever the edges are. The runs below it hold all their
/// sources.
fn merge_tree(em: &mut Emitted, refs: Vec<ERef>, bound: Option<InputBound>) -> ERef {
    match refs.len() {
        0 => ERef::Empty,
        1 if bound.is_none() || refs[0] == ERef::Empty => refs[0],
        n if n <= MAX_PORTS => {
            let (op, mv) = em.push_event(|dst| Pre::EventMerge {
                srcs: refs.clone(),
                dst,
                bound,
            });
            for &r in &refs {
                em.read_eref(r, op);
            }
            ERef::Val(mv)
        }
        _ => {
            let runs: Vec<ERef> = refs
                .chunks(MAX_PORTS)
                .map(|run| merge_tree(em, run.to_vec(), None))
                .collect();
            merge_tree(em, runs, bound)
        }
    }
}
