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
//!    [`DelayKey`]. An event fan-in merges its sources by `(offset, source
//!    port)` — ties go to the lower `(NodeKey, port)` source, whatever order
//!    the spec lists them in. One wider than [`MAX_PORTS`] becomes a tree
//!    of merges over contiguous source ranges, which keeps that rule
//!    exactly.
//! 5. **Emit ops** (`emit`) in the serial order, recording every value's writer and
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
mod emit;
pub(crate) mod ids;
pub(crate) mod latency;
pub(crate) mod main_output;
pub(crate) mod order;
pub(crate) mod plan;
pub(crate) mod preroll;
mod schedule;
mod shapes;
pub(crate) mod tail;
mod timing;
pub(crate) mod verify;

use std::collections::{BTreeMap, BTreeSet};

use crate::NodeKey;
use crate::{Edge, EdgeId, InputPort, OutputPort, PortSink, PortSource};
use tutti_buffers::ChannelLayout;
use tutti_units::{Latency, Samples, Tail};

use crate::compile::emit::{ARef, ERef, Emitted, Pre, Val};
use crate::compile::ids::{
    AudioSlotId, AudioValueId, DelayId, EventSlotId, EventValueId, Id, OpId, PlanUnitId, Table,
    TableSlice,
};
use crate::compile::plan::{
    Csr, Delta, EMPTY_SLOT, EventSlotCapacity, FeedbackKey, FeedbackSpec, GainPortOp, NodeTables,
    Op, ParamPortOp, Placement, Plan, PlanUnit, Span, UnitIdx, Value, ZERO_SLOT,
};
use crate::model::spec::{EventInputPort, EventOutputPort, ValidGraph};
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
    let order = schedule::order(&schedule::acyclic(graph, &checked)?);
    let timing = timing::solve(graph, &checked, prepare, prev);
    let Emitted {
        pre,
        audio,
        event,
        preds,
        delays,
        audio_feedback,
        event_feedback,
        taps,
        event_taps,
        meters,
    } = emit::emit(graph, &checked, &order, &timing);

    // ---- 6. colour --------------------------------------------------------
    let op_count = pre.len();
    let mut succ_rows: Table<OpId, Vec<OpId>> = Table::filled(op_count, Vec::new());
    for (op, ps) in preds.iter_enumerated() {
        for &p in ps {
            debug_assert!(p < op, "ops are emitted in topological order");
            succ_rows[p].push(op);
        }
    }
    let reach = colour::Reach::new(&succ_rows);

    // In-place candidates: (value written, value it may overwrite).
    let mut in_place: BTreeMap<AudioValueId, AudioValueId> = BTreeMap::new();
    for pre in &pre {
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
            } if checked.shapes[*unit].in_place => {
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
    let audio_fixed = 1 + audio_feedback.len();
    let event_fixed = 1 + event_feedback.len();
    let audio_colour = colour::colour(&audio, &reach, &in_place);
    let event_colour = colour::colour(&event, &reach, &BTreeMap::new());
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
        FeedbackKey::Event { from, delay, .. } => EventSlotCapacity::fifo(
            checked.shapes[checked.unit(from.node)].event_capacity,
            delay,
            block,
        ),
        _ => unreachable!("event feedback keys are events"),
    };
    let mut value_cap: Table<EventValueId, EventSlotCapacity> =
        Table::filled(event.len(), EventSlotCapacity::NONE);
    for pre in &pre {
        match pre {
            Pre::Node { unit, eout, .. } => {
                let port = EventSlotCapacity::port(checked.shapes[*unit].event_capacity);
                for &v in eout {
                    value_cap[v] = port;
                }
            }
            Pre::EventDelay { delay, dst, .. } => {
                let d = &delays[*delay];
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
                        .map(|p| checked.shapes[checked.unit(p.node)].event_capacity),
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
                            ERef::Fb(f) => fb_cap(&event_feedback[f]),
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
    for (f, key) in event_feedback.iter_enumerated() {
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
    let ops: Table<OpId, Op> = pre
        .iter()
        .map(|op| match op {
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
    let audio_values = lower_values(&audio, &mut value_readers, |v| aslot(v).get());
    let event_values = lower_values(&event, &mut value_readers, |v| eslot(v).get());

    // ---- 7. coarsen, and price the plan (the cost model) -----------------
    let op_work: Table<OpId, crate::node::cost::Work> = ops
        .iter()
        .map(|op| crate::node::cost::op_work(op, &checked.shapes, &sum_lanes))
        .collect();
    let coarse = coarsen::coarsen(&preds, &succ_rows, &op_work, prepare.max_block().samples());

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
        audio_feedback: audio_feedback
            .iter_enumerated()
            .map(|(f, &key)| FeedbackSpec {
                key,
                slot: f.slot().get(),
            })
            .collect(),
        event_feedback: event_feedback
            .iter_enumerated()
            .map(|(f, &key)| FeedbackSpec {
                key,
                slot: f.slot().get(),
            })
            .collect(),
        delays,
        taps,
        event_taps,
        meters,
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
        order: order.iter().map(|&n| checked.keys[n]).collect(),
        main_inputs: topology.inputs.count(),
        compensation: timing.compensation,
        main_output_id: crate::MainOutputId::GENESIS,
        main_output_bank: graph.main_output_bank(),
        rendered_outputs: timing.rendered_outputs,
        total_latency: timing.total,
        bank_total: timing.bank_total,
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
