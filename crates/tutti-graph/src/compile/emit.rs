//! Compiler pass 5: the ops, in the serial order, before slots exist.
//!
//! Every value is written by one op and read by the ops that name it; each op
//! records its dependencies. The delay rings of latency compensation, the sums
//! of fan-in and the merges of event fan-in are ops of their own. Each event
//! value is priced where its ports are known: what the value can hold is part
//! of the program `colour` lowers.

use std::collections::BTreeSet;

use crate::NodeMix;
use crate::compile::ids::{
    AudioFeedbackId, AudioValueId, EventFeedbackId, EventValueId, Id, OpId, PlanUnitId, Table,
    TableSlice,
};
use crate::compile::plan::{
    Csr, DelayKey, DelaySpec, EventSlotCapacity, FeedbackKey, GainPortOp, InputBound, Op,
    ParamPortOp, Span, Value,
};
use crate::compile::program::{ARef, ByValue, Capacities, ERef, Program};
use crate::compile::shapes::Units;
use crate::compile::timing::Timing;
use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort, ValidGraph};
use crate::node::param::ParamIn;
use crate::node::shape::Shape;
use crate::node::{InPlaceMask, MAX_PORTS, Prepare};
use crate::{Edge, EdgeId, FeedbackFrom, InputPort, NodeKey, OutputPort, PortSink, PortSource};
use tutti_units::{Latency, Samples};

/// The ops, in emission order, with the values they write and read.
type Ops = Op<ARef, ERef>;

/// A value under construction: its writer, once the op is pushed, and the ops
/// that read it.
struct Written {
    writer: Option<OpId>,
    readers: Vec<OpId>,
}

/// The audio feedback key of `f`: the edge, and the generation of its source.
fn audio_feedback_key(graph: &ValidGraph, f: FeedbackFrom) -> FeedbackKey {
    FeedbackKey::Audio {
        from: f.from,
        generation: graph.generation(f.from.node),
        delay: f.delay,
    }
}

/// The event feedback key of the edge from `from` into `at`.
fn event_feedback_key(
    graph: &ValidGraph,
    at: EventInputPort,
    from: EventOutputPort,
    delay: Samples,
) -> FeedbackKey {
    FeedbackKey::Event {
        at,
        from,
        generation: graph.generation(from.node),
        delay,
    }
}

/// The id of the feedback keyed `key` among the sorted feedback `list`.
fn feedback_in<F: Id>(key: FeedbackKey, list: &TableSlice<F, FeedbackKey>) -> F {
    list.search_by_key(&key, |k| *k)
        .expect("feedback keys are collected before they are read")
}

/// Emits the whole graph into a program: the main inputs and every node's ops
/// in `order`, then the main outputs, taps, event taps, meters and feedback
/// captures.
///
/// A source is delayed to its sink's arrival (or to the output time) by a ring
/// of its own when the two differ; a fan-in is summed or merged in source
/// order, so two specs that list the same sources differently emit the same
/// ops. The result's dependencies are sorted rows of earlier ops, and every
/// event value carries the capacity its writer's ports give it.
pub(super) fn emit(
    graph: &ValidGraph,
    units: &Units,
    order: &[PlanUnitId],
    timing: &Timing,
    prepare: &Prepare,
) -> Program<ByValue> {
    let mut emitter = Emitter::new(graph, units, timing, prepare);
    for &n in order {
        emitter.node(n);
    }
    let output_refs = emitter.outputs();
    emitter.taps(&output_refs);
    emitter.event_taps();
    emitter.meters(&output_refs);
    emitter.captures();
    emitter.finish()
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

/// The emission in progress: the read-only graph, the program so far, and
/// what the program holds only once emission ends (each value's writer and
/// readers, each op's dependencies as a set).
struct Emitter<'a> {
    graph: &'a ValidGraph,
    units: &'a Units,
    timing: &'a Timing,
    /// The largest block, which sizes the FIFO of every event delay.
    block: usize,
    program: Program<ByValue>,
    audio: Table<AudioValueId, Written>,
    event: Table<EventValueId, Written>,
    /// The ops each op waits for.
    deps: Table<OpId, BTreeSet<OpId>>,
    /// The main input channels read, ascending, each with the value it is
    /// copied into.
    main_inputs: Vec<(u16, AudioValueId)>,
    /// The node op of each unit already emitted.
    node_ops: Table<PlanUnitId, Option<OpId>>,
}

impl<'a> Emitter<'a> {
    /// An emitter holding the main inputs the graph reads, each copied into a
    /// value once, in channel order.
    fn new(graph: &'a ValidGraph, units: &'a Units, timing: &'a Timing, prepare: &Prepare) -> Self {
        let topology = graph.topology();
        let audio_feedback: Table<AudioFeedbackId, FeedbackKey> = topology
            .edges
            .values()
            .flatten()
            .filter_map(|e| match *e {
                Edge::Feedback(f) => Some(audio_feedback_key(graph, f)),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let event_feedback: Table<EventFeedbackId, FeedbackKey> = graph
            .events()
            .iter()
            .flat_map(|(&at, sources)| {
                sources.iter().filter_map(move |e| match *e {
                    EventEdge::Feedback { from, delay } => Some((at, from, delay)),
                    EventEdge::Direct(_) => None,
                })
            })
            .map(|(at, from, delay)| event_feedback_key(graph, at, from, delay))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let channels: Vec<u16> = topology
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
            .collect::<BTreeSet<u16>>()
            .into_iter()
            .collect();
        let mut emitter = Self {
            graph,
            units,
            timing,
            block: prepare.max_block().get(),
            program: Program {
                ops: Table::default(),
                audio_list: Vec::new(),
                event_list: Vec::new(),
                param_ports: Vec::new(),
                gain_ports: Vec::new(),
                sum_lanes: Vec::new(),
                audio_values: Table::default(),
                event_values: Table::default(),
                value_readers: Vec::new(),
                deps: Csr::default(),
                delays: Table::default(),
                audio_feedback,
                event_feedback,
                taps: Table::default(),
                event_taps: Table::default(),
                meters: Table::default(),
                sizing: Capacities {
                    values: Table::default(),
                    feedback: Table::default(),
                },
            },
            audio: Table::default(),
            event: Table::default(),
            deps: Table::default(),
            main_inputs: Vec::new(),
            node_ops: Table::filled(units.keys.len(), None),
        };
        for channel in channels {
            let dst = emitter.new_audio();
            emitter.push(Op::MainInput {
                channel,
                dst: dst.into(),
            });
            emitter.main_inputs.push((channel, dst));
        }
        emitter
    }

    /// The program: values flattened with their readers, dependencies as
    /// rows, and every event feedback priced.
    fn finish(mut self) -> Program<ByValue> {
        let feedback = self
            .program
            .event_feedback
            .iter()
            .map(|key| self.feedback_capacity(key))
            .collect();
        let mut value_readers: Vec<OpId> = Vec::new();
        let mut values = |written: Vec<Written>| -> Vec<Value<()>> {
            written
                .into_iter()
                .map(|w| Value {
                    slot: (),
                    writer: w.writer.expect("every value is written"),
                    readers: Span::extend(&mut value_readers, w.readers),
                })
                .collect()
        };
        self.program.audio_values = Table::from(values(self.audio.into_iter().collect()));
        self.program.event_values = Table::from(values(self.event.into_iter().collect()));
        self.program.value_readers = value_readers;
        self.program.deps = Csr::from_table(
            &self
                .deps
                .into_iter()
                .map(|row| row.into_iter().collect::<Vec<OpId>>())
                .collect::<Table<OpId, Vec<OpId>>>(),
        );
        self.program.sizing.feedback = feedback;
        self.program
    }

    /// What an event feedback slot holds: all its FIFO can hold at its
    /// source's declared rate.
    fn feedback_capacity(&self, key: &FeedbackKey) -> EventSlotCapacity {
        match *key {
            FeedbackKey::Event { from, delay, .. } => EventSlotCapacity::fifo(
                self.units.shape_of(from.node).event_capacity,
                delay,
                self.block,
            ),
            FeedbackKey::Audio { .. } => unreachable!("event feedback keys are events"),
        }
    }

    // ---- values and ops ------------------------------------------------------

    fn new_audio(&mut self) -> AudioValueId {
        self.audio.push(Written {
            writer: None,
            readers: Vec::new(),
        })
    }

    fn new_event(&mut self, capacity: EventSlotCapacity) -> EventValueId {
        self.program.sizing.values.push(capacity);
        self.event.push(Written {
            writer: None,
            readers: Vec::new(),
        })
    }

    /// Appends `op`, which becomes the writer of the values it writes.
    fn push(&mut self, op: Ops) -> OpId {
        let id = self.program.ops.push(op);
        self.deps.push(BTreeSet::new());
        let (audio, event): (Vec<ARef>, Vec<ERef>) = match op {
            Op::MainInput { dst, .. }
            | Op::Delay { dst, .. }
            | Op::Sum { dst, .. }
            | Op::WeightedSum { dst, .. } => (vec![dst], Vec::new()),
            Op::EventDelay { dst, .. } | Op::EventMerge { dst, .. } => (Vec::new(), vec![dst]),
            Op::Node {
                audio_out,
                event_out,
                ..
            } => (
                self.program.audio_list[audio_out.range()].to_vec(),
                self.program.event_list[event_out.range()].to_vec(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        for v in audio.into_iter().filter_map(ARef::value) {
            self.audio[v].writer = Some(id);
        }
        for v in event.into_iter().filter_map(ERef::value) {
            self.event[v].writer = Some(id);
        }
        id
    }

    fn read_audio(&mut self, v: AudioValueId, op: OpId) {
        let w = self.audio[v]
            .writer
            .expect("a value is written before it is read");
        self.audio[v].readers.push(op);
        self.deps[op].insert(w);
    }

    fn read_event(&mut self, v: EventValueId, op: OpId) {
        let w = self.event[v]
            .writer
            .expect("a value is written before it is read");
        self.event[v].readers.push(op);
        self.deps[op].insert(w);
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

    /// Pushes `op` and records that it reads each of `srcs`.
    fn push_reading(&mut self, op: Ops, srcs: &[ARef]) -> OpId {
        let id = self.push(op);
        for &src in srcs {
            self.read_aref(src, id);
        }
        id
    }

    // ---- what the emitted nodes name ----------------------------------------

    /// The ops of the emitted node `key`: its node op, if it was emitted.
    fn node_op(&self, key: NodeKey) -> Option<Ops> {
        let op = self.node_ops[self.units.get(key)?]?;
        Some(self.program.ops[op])
    }

    /// The value an audio output port writes, if the node and port exist.
    fn audio_out(&self, port: OutputPort) -> Option<AudioValueId> {
        let Op::Node { audio_out, .. } = self.node_op(port.node)? else {
            unreachable!("a unit's op is its node")
        };
        self.program.audio_list[audio_out.range()]
            .get(usize::from(port.port))?
            .value()
    }

    /// The value an event output port writes, if the node and port exist.
    fn event_out(&self, port: EventOutputPort) -> Option<EventValueId> {
        let Op::Node { event_out, .. } = self.node_op(port.node)? else {
            unreachable!("a unit's op is its node")
        };
        self.program.event_list[event_out.range()]
            .get(usize::from(port.port))?
            .value()
    }

    /// What an event input reads, after its sources' delays and merge.
    fn event_in(&self, at: EventInputPort) -> Option<ERef> {
        let Op::Node { event_in, .. } = self.node_op(at.node)? else {
            unreachable!("a unit's op is its node")
        };
        self.program.event_list[event_in.range()]
            .get(usize::from(at.port))
            .copied()
    }

    /// The value a main input channel is copied into, if it is read.
    fn main_input(&self, channel: u16) -> Option<AudioValueId> {
        self.main_inputs
            .binary_search_by_key(&channel, |&(ch, _)| ch)
            .ok()
            .map(|i| self.main_inputs[i].1)
    }

    /// When the node `key` departs.
    fn departure(&self, key: NodeKey) -> Latency {
        self.timing.departure[self.units.unit(key)]
    }

    /// The value `source` reads and when it departs; `None` for silence and
    /// for a port that does not exist.
    fn source(&self, source: PortSource) -> Option<(AudioValueId, Latency)> {
        match source {
            PortSource::Node(p) => Some((self.audio_out(p)?, self.departure(p.node))),
            PortSource::MainInput(ch) => Some((self.main_input(ch)?, Latency::ZERO)),
            PortSource::Zero => None,
        }
    }

    /// The value of an event output, a port a checked graph names.
    fn event_source(&self, from: EventOutputPort) -> EventValueId {
        self.event_out(from)
            .expect("a source is emitted before its readers")
    }

    /// The audio value of `source` through its PDC delay to the time `to`,
    /// keyed `key`; a port a checked graph names.
    fn align_source(&mut self, source: PortSource, to: Latency, key: DelayKey) -> ARef {
        let (v, dep) = self
            .source(source)
            .expect("a validated edge reads a port of its source");
        self.align_audio(v, dep.gap_to(to), key)
    }

    /// The event value of `from` through its PDC delay to the time `to`,
    /// keyed `key`.
    fn align_event_source(
        &mut self,
        from: EventOutputPort,
        to: Latency,
        key: DelayKey,
    ) -> EventValueId {
        let v = self.event_source(from);
        let by = self.departure(from.node).gap_to(to);
        self.align_event(v, by, key)
    }

    /// `v` through its PDC delay of `by` frames, keyed `key`: the value itself
    /// when `by` is zero (no op), else the output of a `Delay` ring.
    fn align_audio(&mut self, v: AudioValueId, by: Samples, key: DelayKey) -> ARef {
        if by.is_zero() {
            return ARef::Val(v);
        }
        let delay = self.program.delays.push(DelaySpec { key, len: by });
        let dst = self.new_audio();
        let op = self.push(Op::Delay {
            delay,
            src: v.into(),
            dst: dst.into(),
        });
        self.read_audio(v, op);
        ARef::Val(dst)
    }

    /// Event value `v`, delayed by `by` through a ring keyed `key` when `by` is
    /// not zero (the value itself otherwise). The delayed value holds all its
    /// FIFO can: what can fall due in one block, from every port that writes
    /// the stream.
    fn align_event(&mut self, v: EventValueId, by: Samples, key: DelayKey) -> EventValueId {
        if by.is_zero() {
            return v;
        }
        let Some(from) = key.event_origin() else {
            unreachable!("an event delay is keyed as events")
        };
        let ports: Vec<EventOutputPort> = match from {
            crate::EventTapPort::Out(port) => vec![port],
            crate::EventTapPort::In(at) => self
                .graph
                .events()
                .get(&at)
                .map(|edges| edges.iter().map(|e| e.from()).collect())
                .unwrap_or_default(),
        };
        let capacity = EventSlotCapacity::fifo_sum(
            ports
                .iter()
                .map(|p| self.units.shape_of(p.node).event_capacity),
            by,
            self.block,
        );
        let delay = self.program.delays.push(DelaySpec { key, len: by });
        let dst = self.new_event(capacity);
        let op = self.push(Op::EventDelay {
            delay,
            src: v.into(),
            dst: dst.into(),
        });
        self.read_event(v, op);
        dst
    }

    // ---- nodes -----------------------------------------------------------------

    /// The node `n`: its inputs (sums, merges, the delays they need), its
    /// driven params, then the op itself.
    fn node(&mut self, n: PlanUnitId) {
        let key = self.units.keys[n];
        let shape = self.units.shapes[n];
        let ain = self.audio_inputs(n, key, &shape);
        let ein = self.event_inputs(n, key, &shape);
        let params = self.params(n, key, &shape);
        let reads: Vec<EventValueId> = params.iter().filter_map(|p| p.slot.value()).collect();

        let aout: Vec<ARef> = (0..shape.audio_out().count())
            .map(|_| self.new_audio().into())
            .collect();
        let port = EventSlotCapacity::port(shape.event_capacity);
        let eout: Vec<ERef> = (0..shape.event_out)
            .map(|_| self.new_event(port).into())
            .collect();
        let node = Op::Node {
            unit: n,
            audio_in: Span::extend(&mut self.program.audio_list, ain.iter().copied()),
            audio_out: Span::extend(&mut self.program.audio_list, aout),
            event_in: Span::extend(&mut self.program.event_list, ein.iter().copied()),
            event_out: Span::extend(&mut self.program.event_list, eout),
            in_place: InPlaceMask::NONE,
            params: Span::extend(&mut self.program.param_ports, params),
        };
        let op = self.push(node);
        self.node_ops[n] = Some(op);
        for r in ain {
            self.read_aref(r, op);
        }
        for r in ein {
            self.read_eref(r, op);
        }
        for v in reads {
            self.read_event(v, op);
        }
    }

    /// Audio inputs: every source of a port through its own PDC delay when it
    /// needs one, then the sum in **source order** (`Edge`'s `Ord`, not the
    /// order the spec listed them in), so two specs that list the same sources
    /// differently render alike. One source is the value itself, none is
    /// silence.
    fn audio_inputs(&mut self, n: PlanUnitId, key: NodeKey, shape: &Shape) -> Vec<ARef> {
        let graph = self.graph;
        let arrival = self.timing.arrival[n];
        (0..shape.audio_in().count())
            .map(|port| {
                let at = InputPort::new(key, port);
                let mut sources: Vec<Edge> =
                    graph.topology().edges.get(&at).cloned().unwrap_or_default();
                sources.sort();
                let mut refs: Vec<Addend> = Vec::with_capacity(sources.len());
                for edge in sources {
                    let r = match edge {
                        // Silence adds nothing, bit for bit (the sum starts at
                        // `+0.0`); `validate` refuses it in a set, so this is a
                        // raw write's.
                        Edge::Direct(PortSource::Zero) => continue,
                        // A main input arrives at zero: it is a merge-point
                        // source like any other (see the module docs of
                        // `compile`, step 4).
                        Edge::Direct(from) => {
                            self.align_source(from, arrival, DelayKey::Audio { at, from })
                        }
                        Edge::Feedback(f) => ARef::Fb(feedback_in(
                            audio_feedback_key(graph, f),
                            &self.program.audio_feedback,
                        )),
                    };
                    // An edge with a gain entry is a weighted lane.
                    let id = EdgeId {
                        sink: PortSink::Port(at),
                        from: edge,
                    };
                    let gain = graph.gains().get(&id);
                    // Its driving lane, aligned to this node's arrival like a
                    // param's source (a silent addend has none: nothing is
                    // added).
                    let drive =
                        gain.and_then(|g| g.source)
                            .filter(|_| r != ARef::Zero)
                            .map(|from| GainDrive {
                                from,
                                value: self.align_event_source(
                                    from,
                                    arrival,
                                    DelayKey::EdgeGain { id, from },
                                ),
                            });
                    refs.push(Addend {
                        r,
                        lane: gain.map(|_| id),
                        drive,
                    });
                }
                self.sum_chain(refs)
            })
            .collect()
    }

    /// Event inputs: delay each source that needs it, then merge fan-in in
    /// **source order**, `(source NodeKey, source port)` — not the order the
    /// edges were listed in. The tie-break at equal offsets is then a property
    /// of the wiring alone: two specs that list the same sources differently
    /// merge alike, so a host that rebuilds its spec from an unordered store
    /// (the ECS) cannot reorder a chord by accident. Sources are distinct
    /// (`GraphSpec::validate`), so the order is total.
    fn event_inputs(&mut self, n: PlanUnitId, key: NodeKey, shape: &Shape) -> Vec<ERef> {
        let (graph, units) = (self.graph, self.units);
        let arrival = self.timing.arrival[n];
        (0..shape.event_in)
            .map(|port| {
                let at = EventInputPort::new(key, port);
                let mut sources: Vec<EventEdge> =
                    graph.events().get(&at).cloned().unwrap_or_default();
                sources.sort_by_key(|e| e.from());
                let refs: Vec<ERef> = sources
                    .iter()
                    .map(|e| match *e {
                        EventEdge::Feedback { from, delay } => ERef::Fb(feedback_in(
                            event_feedback_key(graph, at, from, delay),
                            &self.program.event_feedback,
                        )),
                        EventEdge::Direct(from) => ERef::Val(self.align_event_source(
                            from,
                            arrival,
                            DelayKey::Event { at, from },
                        )),
                    })
                    .collect();
                // A lone undelayed writer whose port cannot hold more than the
                // bound needs no merge to keep it.
                let lone_within_bound = match sources[..] {
                    [EventEdge::Direct(from)] => {
                        refs[..] == [ERef::Val(self.event_source(from))]
                            && units.shape_of(from.node).event_capacity.is_some_and(|c| {
                                shape.event_input_capacity.is_some_and(|b| {
                                    crate::event::writer::note_off_room(c.get() as usize)
                                        <= b.get() as usize
                                })
                            })
                    }
                    _ => false,
                };
                let bound = shape
                    .event_input_capacity
                    .filter(|_| !lone_within_bound)
                    .map(|capacity| InputBound { capacity, unit: n });
                self.merge_tree(refs, bound)
            })
            .collect()
    }

    /// Driven params, in port order, then the node's mix (its port is past
    /// every declared index): each source through its PDC delay when it needs
    /// one, exactly as an input is (see the `param` module docs).
    fn params(&mut self, n: PlanUnitId, key: NodeKey, shape: &Shape) -> Vec<ParamPortOp<ERef>> {
        let graph = self.graph;
        let arrival = self.timing.arrival[n];
        let declared = shape
            .params
            .as_slice()
            .iter()
            .enumerate()
            .map(|(k, &param)| (k as u16, param));
        let mix = std::iter::once((ParamPortOp::<ERef>::MIX_PORT, NodeMix::ADDR));
        declared
            .chain(mix)
            .filter_map(|(port, param)| {
                let at = ParamIn { node: key, param };
                let from = graph.params().get(&at).copied()?;
                let value =
                    self.align_event_source(from, arrival, DelayKey::ParamEvent { at, from });
                Some(ParamPortOp {
                    port,
                    param,
                    from,
                    slot: value.into(),
                })
            })
            .collect()
    }

    /// The lanes of a tap or meter: one per source, silence for a port that
    /// does not exist (a node replaced by a narrower one, a removed node, a
    /// main input past the declared width). An aligned lane is delayed to the
    /// graph's output time by a ring keyed `key(lane, source)`; a raw one
    /// reads the source as it leaves its node.
    fn lanes(
        &mut self,
        sources: &[PortSource],
        align: crate::TapAlign,
        key: impl Fn(usize, PortSource) -> DelayKey,
    ) -> Vec<ARef> {
        let total = self.timing.total;
        sources
            .iter()
            .enumerate()
            .map(|(lane, &source)| match self.source(source) {
                Some((v, dep)) if align == crate::TapAlign::ToOutput => {
                    self.align_audio(v, dep.gap_to(total), key(lane, source))
                }
                Some((v, _)) => ARef::Val(v),
                None => ARef::Zero,
            })
            .collect()
    }

    // ---- the graph's edges to the outside -------------------------------------

    /// The main output channels: each channel's sources through the delays
    /// that align them, summed when there are several. A main output meter or
    /// an outputs tap reads each channel as it leaves the alignment delay, so
    /// with one declared the delay is its own op writing a slot that the
    /// `Output` op and the reader both read (one ring, the same key).
    ///
    /// Returns what each rendered channel carries.
    fn outputs(&mut self) -> Vec<ARef> {
        let (graph, timing) = (self.graph, self.timing);
        let topology = graph.topology();
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
                // A summed channel (or one whose source is weighted): each
                // source is delayed to the output time on its own, as an op of
                // its own, then summed; the `Output` op carries no ring.
                let output_time = timing.output_time(channel);
                let refs: Vec<Addend> = sources
                    .iter()
                    .map(|&source| {
                        let r = self.align_source(
                            source,
                            output_time,
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
                                value: self.align_event_source(
                                    from,
                                    output_time,
                                    DelayKey::EdgeGain { id, from },
                                ),
                            });
                        Addend { r, lane, drive }
                    })
                    .collect();
                let src = self.sum_chain(refs);
                output_refs.push(src);
                self.push_reading(
                    Op::Output {
                        channel,
                        src,
                        delay: None,
                    },
                    &[src],
                );
                continue;
            }
            let source = sources.first().copied().unwrap_or(PortSource::Zero);
            let mut src = match source {
                PortSource::Zero => ARef::Zero,
                from => ARef::Val(
                    self.source(from)
                        .expect("a validated output reads a port of its source")
                        .0,
                ),
            };
            let d = if timing.in_bank(channel) {
                let dep = self.source(source).map_or(Latency::ZERO, |(_, dep)| dep);
                match source {
                    PortSource::Zero => Samples::ZERO,
                    _ => dep.gap_to(timing.bank_total),
                }
            } else {
                timing.compensation[usize::from(channel)]
            };
            // Delaying silence is silence: a zero channel reports its
            // compensation (as `latency::plan` does) but gets no ring.
            let key = DelayKey::Output {
                channel,
                from: source,
            };
            let mut delay = None;
            if !d.is_zero() {
                match src {
                    ARef::Zero => {}
                    ARef::Val(v) if main_output_meter => {
                        src = self.align_audio(v, d, key);
                    }
                    _ => {
                        delay = Some(self.program.delays.push(DelaySpec { key, len: d }));
                    }
                }
            }
            output_refs.push(src);
            self.push_reading(
                Op::Output {
                    channel,
                    src,
                    delay,
                },
                &[src],
            );
        }
        output_refs
    }

    /// Taps: one lane per source, any ports. A lane whose port does not exist
    /// reads silence rather than failing the commit, as a meter's does; it
    /// gets no delay ring, and the other lanes' rings are keyed by their own
    /// lane and source. A closed tap is not in the topology, so nothing is
    /// emitted for it. One op a tap, after every lane's delay: it writes the
    /// lanes together.
    fn taps(&mut self, output_refs: &[ARef]) {
        let graph = self.graph;
        for (&tap, spec) in &graph.topology().taps {
            let srcs: Vec<ARef> = match spec {
                // What each output channel carries, post-sum and aligned; a
                // lane past the output count is silence.
                crate::TapSpec::Outputs { width } => (0..*width)
                    .map(|c| {
                        output_refs
                            .get(c)
                            .filter(|_| c < self.timing.main_output_width)
                            .copied()
                            .unwrap_or(ARef::Zero)
                    })
                    .collect(),
                crate::TapSpec::Ports { sources, align } => {
                    self.lanes(sources, *align, |lane, from| DelayKey::Tap {
                        tap,
                        lane,
                        from,
                    })
                }
            };
            let tap = self.program.taps.push(tap);
            let srcs_span = Span::extend(&mut self.program.audio_list, srcs.iter().copied());
            self.push_reading(
                Op::Tap {
                    tap,
                    srcs: srcs_span,
                },
                &srcs,
            );
        }
    }

    /// Event taps: one event output each. An aligned tap is delayed to the
    /// graph's output time (the compensation an event edge into an output-time
    /// sink would get); a raw one reads the port as it leaves its node. A
    /// closed tap is not in the topology, so nothing is emitted for it. A tap
    /// whose port does not exist (a node replaced by one with fewer event
    /// outputs, a removed node) keeps its place in the table, so its writer is
    /// carried and resumes if the port returns, but it gets no op and no delay
    /// ring: it reads nothing and the commit succeeds. One op a tap, after its
    /// delay.
    fn event_taps(&mut self) {
        let (graph, timing) = (self.graph, self.timing);
        for (&tap, spec) in &graph.topology().event_taps {
            let index = self.program.event_taps.push(tap);
            let Some(from) = spec.source else {
                continue;
            };
            // What the port carries, and when it departs (an output) or
            // arrives (an input, which an aligned tap delays to the output
            // time too).
            let (carried, at_time) = match from {
                crate::EventTapPort::Out(port) => {
                    let (Some(v), Some(n)) = (self.event_out(port), self.units.get(port.node))
                    else {
                        continue;
                    };
                    (ERef::Val(v), timing.departure[n])
                }
                crate::EventTapPort::In(at) => {
                    let (Some(r), Some(n)) = (self.event_in(at), self.units.get(at.node)) else {
                        continue;
                    };
                    (r, timing.arrival[n])
                }
            };
            if carried == ERef::Empty {
                continue;
            }
            // A feedback edge's slot is read as the node reads it: it has no
            // value to delay.
            let src = match (spec.align, carried) {
                (crate::TapAlign::ToOutput, ERef::Val(v)) => ERef::Val(self.align_event(
                    v,
                    at_time.gap_to(timing.total),
                    DelayKey::EventTap { tap, from },
                )),
                _ => carried,
            };
            let op = self.push(Op::EventTap { tap: index, src });
            self.read_eref(src, op);
        }
    }

    /// Meters: one lane per source. `Outputs` reads what each output channel
    /// carries (after its alignment delay); `Ports` reads each source as it
    /// leaves its node, or, aligned, delayed to the output time by a ring of
    /// its own like a tap's. A lane whose port does not exist reads silence
    /// rather than failing the commit. One op a meter, so its lanes are folded
    /// together. A closed meter is not in the topology.
    fn meters(&mut self, output_refs: &[ARef]) {
        let (graph, timing) = (self.graph, self.timing);
        for (&meter, spec) in &graph.topology().meters {
            let srcs: Vec<ARef> = match spec {
                crate::MeterSpec::Outputs => output_refs[..timing.main_output_width].to_vec(),
                crate::MeterSpec::Ports { sources, align } => {
                    self.lanes(sources, *align, |lane, from| DelayKey::Meter {
                        meter,
                        lane,
                        from,
                    })
                }
            };
            let meter = self.program.meters.push((meter, srcs.len()));
            let srcs_span = Span::extend(&mut self.program.audio_list, srcs.iter().copied());
            self.push_reading(
                Op::Meter {
                    meter,
                    srcs: srcs_span,
                },
                &srcs,
            );
        }
    }

    /// Captures run last, one per feedback delay. They feed the feedback
    /// delays, which the executor reads from *before* the block (into the
    /// feedback slots), so a capture needs no ordering against the feedback's
    /// readers: with a delay of a whole `MaxBlock`, nothing it queues is due
    /// this block.
    fn captures(&mut self) {
        let audio: Vec<(AudioFeedbackId, OutputPort)> = self
            .program
            .audio_feedback
            .iter_enumerated()
            .map(|(feedback, key)| match *key {
                FeedbackKey::Audio { from, .. } => (feedback, from),
                FeedbackKey::Event { .. } => unreachable!("audio feedback keys are audio"),
            })
            .collect();
        for (feedback, from) in audio {
            let src = self
                .audio_out(from)
                .expect("a feedback source is emitted before its capture");
            let op = self.push(Op::Capture {
                feedback,
                src: src.into(),
            });
            self.read_audio(src, op);
        }
        let events: Vec<(EventFeedbackId, EventOutputPort)> = self
            .program
            .event_feedback
            .iter_enumerated()
            .map(|(feedback, key)| match *key {
                FeedbackKey::Event { from, .. } => (feedback, from),
                FeedbackKey::Audio { .. } => unreachable!("event feedback keys are events"),
            })
            .collect();
        for (feedback, from) in events {
            let src = self.event_source(from);
            let op = self.push(Op::EventCapture {
                feedback,
                src: src.into(),
            });
            self.read_event(src, op);
        }
    }

    // ---- fan-in ----------------------------------------------------------------

    /// Sum `refs` (already in source order) into one audio value: the left fold
    /// `((+0.0 + a_0) + a_1) + ..`, each weighted addend `acc + w * a`.
    ///
    /// Silence (`ARef::Zero`) adds nothing and is dropped; none left is
    /// silence, one plain one is itself (an alias, no op). A weighted one is an
    /// op even alone (`+0.0 + w * a`: a plain alias keeps a `-0.0` and a weight
    /// does not). Up to [`MAX_PORTS`] addends are one op. A wider fan-in is a
    /// **chain, not a tree**: a float sum is not associative, so the first op
    /// folds the first `MAX_PORTS` and each next one reads the last result as
    /// its first addend (plain: its weight is `1.0`) and adds the following
    /// `MAX_PORTS - 1`. The accumulator is never `-0.0`, so adding it to `+0.0`
    /// again changes no bit: the chain is one flat fold, wherever it is cut. An
    /// op with no weighted lane is a plain `Op::Sum`.
    fn sum_chain(&mut self, refs: Vec<Addend>) -> ARef {
        let refs: Vec<Addend> = refs.into_iter().filter(|a| a.r != ARef::Zero).collect();
        match refs.len() {
            0 => ARef::Zero,
            1 if refs[0].lane.is_none() => refs[0].r,
            len => {
                let first = len.min(MAX_PORTS);
                let mut acc = self.sum(&refs[..first]);
                for run in refs[first..].chunks(MAX_PORTS - 1) {
                    let mut addends = Vec::with_capacity(run.len() + 1);
                    addends.push(Addend {
                        r: acc,
                        lane: None,
                        drive: None,
                    });
                    addends.extend_from_slice(run);
                    acc = self.sum(&addends);
                }
                acc
            }
        }
    }

    /// One sum op over `addends`.
    fn sum(&mut self, addends: &[Addend]) -> ARef {
        let srcs: Vec<ARef> = addends.iter().map(|a| a.r).collect();
        let lanes: Vec<Option<EdgeId>> = addends.iter().map(|a| a.lane).collect();
        let gains: Vec<GainPortOp<ERef>> = addends
            .iter()
            .enumerate()
            .filter_map(|(lane, a)| {
                a.drive.map(|d| GainPortOp {
                    lane: lane as u16,
                    from: d.from,
                    slot: d.value.into(),
                })
            })
            .collect();
        let dst = self.new_audio();
        let op = if lanes.iter().any(Option::is_some) {
            Op::WeightedSum {
                srcs: Span::extend(&mut self.program.audio_list, srcs.iter().copied()),
                dst: dst.into(),
                lanes: Span::extend(&mut self.program.sum_lanes, lanes),
                gains: Span::extend(&mut self.program.gain_ports, gains.iter().copied()),
            }
        } else {
            Op::Sum {
                srcs: Span::extend(&mut self.program.audio_list, srcs.iter().copied()),
                dst: dst.into(),
            }
        };
        let op = self.push(op);
        for &r in &srcs {
            self.read_aref(r, op);
        }
        // The sum reads each driving lane's events: a read of the op, so the
        // colouring and the verifier cover it.
        for g in &gains {
            if let Some(v) = g.slot.value() {
                self.read_event(v, op);
            }
        }
        ARef::Val(dst)
    }

    /// Merge `refs` (already in source order) into one event stream, in
    /// `(offset, source order)`.
    ///
    /// Up to [`MAX_PORTS`] sources is one `EventMerge` op. Wider fan-in becomes
    /// a tree: contiguous runs of at most `MAX_PORTS` sources merge first, and
    /// the run results merge in run order. Because each merge breaks ties
    /// toward the lower input and the runs are contiguous and in order, a tie
    /// between sources in different runs still goes to the earlier source — the
    /// tree delivers exactly what one flat merge would.
    ///
    /// With a `bound` (the node's `Shape::event_input_capacity`) the last merge
    /// keeps at most that many events, and a single source goes through it too:
    /// the bound holds whatever the edges are. The runs below it hold all their
    /// sources. A merge holds all its inputs together, so it can never drop an
    /// event (a note-off least of all) short of its bound.
    fn merge_tree(&mut self, refs: Vec<ERef>, bound: Option<InputBound>) -> ERef {
        match refs.len() {
            0 => ERef::Empty,
            1 if bound.is_none() || refs[0] == ERef::Empty => refs[0],
            n if n <= MAX_PORTS => {
                let capacity = refs
                    .iter()
                    .fold(EventSlotCapacity::NONE, |acc, r| {
                        acc.plus(match *r {
                            ERef::Val(v) => self.program.sizing.values[v],
                            ERef::Fb(f) => self.feedback_capacity(&self.program.event_feedback[f]),
                            ERef::Empty => EventSlotCapacity::NONE,
                        })
                    })
                    .bounded(bound.map(|b| b.capacity));
                let dst = self.new_event(capacity);
                let merge = Op::EventMerge {
                    srcs: Span::extend(&mut self.program.event_list, refs.iter().copied()),
                    dst: dst.into(),
                    bound,
                };
                let op = self.push(merge);
                for &r in &refs {
                    self.read_eref(r, op);
                }
                ERef::Val(dst)
            }
            _ => {
                let runs: Vec<ERef> = refs
                    .chunks(MAX_PORTS)
                    .map(|run| self.merge_tree(run.to_vec(), None))
                    .collect();
                self.merge_tree(runs, bound)
            }
        }
    }
}
