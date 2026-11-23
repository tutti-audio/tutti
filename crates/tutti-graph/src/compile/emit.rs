//! Compiler pass 5: the ops, in the serial order, before slots exist.
//!
//! Every value is written by one op and read by the ops that name it; each op
//! records its predecessors. The delay rings of latency compensation, the sums
//! of fan-in and the merges of event fan-in are ops of their own.

use std::collections::BTreeSet;

use crate::NodeMix;
use crate::compile::ids::{
    AudioFeedbackId, AudioValueId, DelayId, EventFeedbackId, EventValueId, Id, OpId,
    PlanEventTapId, PlanMeterId, PlanTapId, PlanUnitId, Table, TableSlice,
};
use crate::compile::plan::{DelayKey, DelaySpec, FeedbackKey, InputBound, ParamPortOp};
use crate::compile::shapes::Units;
use crate::compile::timing::Timing;
use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort, ValidGraph};
use crate::node::MAX_PORTS;
use crate::node::param::ParamIn;
use crate::node::shape::Shape;
use crate::{Edge, EdgeId, FeedbackFrom, InputPort, NodeKey, OutputPort, PortSink, PortSource};
use tutti_units::{Latency, Samples};

/// Where an audio input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ARef {
    Val(AudioValueId),
    Zero,
    Fb(AudioFeedbackId),
}

/// Where an event input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ERef {
    Val(EventValueId),
    Empty,
    Fb(EventFeedbackId),
}

/// An op before colouring: the same shape as [`Op`](crate::Op) with value ids
/// where slots will go.
pub(super) enum Pre {
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
pub(super) struct PPort {
    pub(super) port: u16,
    pub(super) param: tutti_units::UnitAddr,
    pub(super) from: EventOutputPort,
    pub(super) value: EventValueId,
}

/// A driven lane of a weighted sum before colouring: which addend it is, its
/// event source, and the event value it reads (the sibling of [`PPort`]).
#[derive(Clone, Copy)]
pub(super) struct GPort {
    pub(super) lane: u16,
    pub(super) from: EventOutputPort,
    pub(super) value: EventValueId,
}

/// A value under construction.
pub(crate) struct Val {
    pub(crate) writer: OpId,
    pub(crate) readers: Vec<OpId>,
}

/// What the op-emission pass hands on: the ops in emission order, the values
/// they write and read, and the tables ops name by id.
///
/// Every op's predecessors precede it (`preds[op]` holds smaller ids only),
/// every value's writer precedes its readers, and each feedback and tap table
/// is the one its ops index.
pub(super) struct Emitted {
    pub(super) pre: Table<OpId, Pre>,
    pub(super) audio: Table<AudioValueId, Val>,
    pub(super) event: Table<EventValueId, Val>,
    pub(super) preds: Table<OpId, BTreeSet<OpId>>,
    pub(super) delays: Table<DelayId, DelaySpec>,
    /// The audio feedback edges, ascending: the slots after the null slot.
    pub(super) audio_feedback: Table<AudioFeedbackId, FeedbackKey>,
    /// The event feedback edges, ascending.
    pub(super) event_feedback: Table<EventFeedbackId, FeedbackKey>,
    pub(super) taps: Table<PlanTapId, crate::TapId>,
    pub(super) event_taps: Table<PlanEventTapId, crate::EventTapId>,
    pub(super) meters: Table<PlanMeterId, (crate::MeterId, usize)>,
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

    /// Pushes `op` and records that it reads each of `srcs`.
    fn push_reading(&mut self, op: Pre, srcs: &[ARef]) -> OpId {
        let id = self.push(op);
        for &src in srcs {
            self.read_aref(src, id);
        }
        id
    }
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

/// The position of `key` in the sorted feedback `list`.
fn fb_index<F: Id>(key: FeedbackKey, list: &TableSlice<F, FeedbackKey>) -> F {
    F::from_index(
        list.as_slice()
            .binary_search(&key)
            .expect("feedback keys are collected before they are read"),
    )
}

/// Emits every op of `graph`: the nodes in `order`, then the main outputs,
/// taps, event taps and meters, then the feedback captures.
///
/// Takes the checked `units`, the serial `order` and the `timing`; returns the
/// [`Emitted`] ops. A source is delayed to its sink's arrival (or to the output
/// time) by a ring of its own when the two differ; a fan-in is summed or merged
/// in source order, so two specs that list the same sources differently emit
/// the same ops.
pub(super) fn emit(
    graph: &ValidGraph,
    units: &Units,
    order: &[PlanUnitId],
    timing: &Timing,
) -> Emitted {
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

    // Main inputs actually read, each copied into a slot once: the first audio
    // values, in channel order.
    let main_inputs: Vec<u16> = topology
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
    let mut emitter = Emitter {
        graph,
        units,
        timing,
        em: Emitted {
            pre: Table::default(),
            audio: Table::default(),
            event: Table::default(),
            preds: Table::default(),
            delays: Table::default(),
            audio_feedback,
            event_feedback,
            taps: Table::default(),
            event_taps: Table::default(),
            meters: Table::default(),
        },
        main_inputs,
        node_ops: Table::filled(units.keys.len(), None),
    };
    for channel in emitter.main_inputs.clone() {
        emitter.em.push_audio(|dst| Pre::MainInput { channel, dst });
    }
    for &n in order {
        emitter.node(n);
    }
    let output_refs = emitter.outputs();
    emitter.taps(&output_refs);
    emitter.event_taps();
    emitter.meters(&output_refs);
    emitter.captures();
    emitter.em
}

/// The emission in progress: the read-only graph, and the ops so far.
struct Emitter<'a> {
    graph: &'a ValidGraph,
    units: &'a Units,
    timing: &'a Timing,
    em: Emitted,
    /// The main input channels read, ascending: channel `main_inputs[i]` is
    /// the audio value `i`.
    main_inputs: Vec<u16>,
    /// The `Pre::Node` op of each unit already emitted.
    node_ops: Table<PlanUnitId, Option<OpId>>,
}

impl Emitter<'_> {
    /// The ports of the emitted node `key`: its audio outputs, event outputs
    /// and event inputs; `None` for a node not emitted.
    fn node_ports(&self, key: NodeKey) -> Option<(&[AudioValueId], &[EventValueId], &[ERef])> {
        let op = self.node_ops[self.units.get(key)?]?;
        match &self.em.pre[op] {
            Pre::Node {
                aout, eout, ein, ..
            } => Some((aout, eout, ein)),
            _ => unreachable!("a unit's op is its node"),
        }
    }

    /// The value an audio output port writes, if the node and port exist.
    fn audio_out(&self, port: OutputPort) -> Option<AudioValueId> {
        let (aout, _, _) = self.node_ports(port.node)?;
        aout.get(usize::from(port.port)).copied()
    }

    /// The value an event output port writes, if the node and port exist.
    fn event_out(&self, port: EventOutputPort) -> Option<EventValueId> {
        let (_, eout, _) = self.node_ports(port.node)?;
        eout.get(usize::from(port.port)).copied()
    }

    /// What an event input reads, after its sources' delays and merge.
    fn event_in(&self, at: EventInputPort) -> Option<ERef> {
        let (_, _, ein) = self.node_ports(at.node)?;
        ein.get(usize::from(at.port)).copied()
    }

    /// The value a main input channel is copied into, if it is read.
    fn main_input(&self, channel: u16) -> Option<AudioValueId> {
        self.main_inputs
            .binary_search(&channel)
            .ok()
            .map(AudioValueId::from_index)
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
        align_audio(&mut self.em, v, dep.gap_to(to), key)
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
        align_event(&mut self.em, v, by, key)
    }

    /// The node `n`: its inputs (sums, merges, the delays they need), its
    /// driven params, then the op itself.
    fn node(&mut self, n: PlanUnitId) {
        let key = self.units.keys[n];
        let shape = self.units.shapes[n];
        let ain = self.audio_inputs(n, key, &shape);
        let ein = self.event_inputs(n, key, &shape);
        let params = self.params(n, key, &shape);
        let reads: Vec<EventValueId> = params.iter().map(|p| p.value).collect();

        let at = self.em.next_op();
        let aout: Vec<AudioValueId> = (0..shape.audio_out().count())
            .map(|_| self.em.audio_value(at))
            .collect();
        let eout: Vec<EventValueId> = (0..shape.event_out)
            .map(|_| self.em.event_value(at))
            .collect();
        let op = self.em.push(Pre::Node {
            unit: n,
            ain: ain.clone(),
            aout,
            ein: ein.clone(),
            eout,
            params,
        });
        debug_assert_eq!(op, at);
        self.node_ops[n] = Some(op);
        for r in ain {
            self.em.read_aref(r, op);
        }
        for r in ein {
            self.em.read_eref(r, op);
        }
        for v in reads {
            self.em.read_event(v, op);
        }
    }

    /// Audio inputs: every source of a port through its own PDC delay when it
    /// needs one, then the sum in **source order** (`Edge`'s `Ord`, not the
    /// order the spec listed them in), so two specs that list the same sources
    /// differently render alike. One source is the value itself, none is
    /// silence.
    fn audio_inputs(&mut self, n: PlanUnitId, key: NodeKey, shape: &Shape) -> Vec<ARef> {
        let (graph, timing) = (self.graph, self.timing);
        let arrival = timing.arrival[n];
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
                        Edge::Feedback(f) => ARef::Fb(fb_index(
                            audio_feedback_key(graph, f),
                            &self.em.audio_feedback,
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
                sum_chain(&mut self.em, refs)
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
        let (graph, units, timing) = (self.graph, self.units, self.timing);
        let arrival = timing.arrival[n];
        (0..shape.event_in)
            .map(|port| {
                let at = EventInputPort::new(key, port);
                let mut sources: Vec<EventEdge> =
                    graph.events().get(&at).cloned().unwrap_or_default();
                sources.sort_by_key(|e| e.from());
                let refs: Vec<ERef> = sources
                    .iter()
                    .map(|e| match *e {
                        EventEdge::Feedback { from, delay } => ERef::Fb(fb_index(
                            event_feedback_key(graph, at, from, delay),
                            &self.em.event_feedback,
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
                    // The public bound names its unit by raw index until the
                    // plan is narrowed.
                    .map(|capacity| InputBound {
                        capacity,
                        unit: n.get(),
                    });
                merge_tree(&mut self.em, refs, bound)
            })
            .collect()
    }

    /// Driven params, in port order, then the node's mix (its port is past
    /// every declared index): each source through its PDC delay when it needs
    /// one, exactly as an input is (see the `param` module docs).
    fn params(&mut self, n: PlanUnitId, key: NodeKey, shape: &Shape) -> Vec<PPort> {
        let graph = self.graph;
        let arrival = self.timing.arrival[n];
        let declared = shape
            .params
            .as_slice()
            .iter()
            .enumerate()
            .map(|(k, &param)| (k as u16, param));
        let mix = std::iter::once((ParamPortOp::MIX_PORT, NodeMix::ADDR));
        declared
            .chain(mix)
            .filter_map(|(port, param)| {
                let at = ParamIn { node: key, param };
                let from = graph.params().get(&at).copied()?;
                let value =
                    self.align_event_source(from, arrival, DelayKey::ParamEvent { at, from });
                Some(PPort {
                    port,
                    param,
                    from,
                    value,
                })
            })
            .collect()
    }

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
                let src = sum_chain(&mut self.em, refs);
                output_refs.push(src);
                self.em.push_reading(
                    Pre::Output {
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
                        src = align_audio(&mut self.em, v, d, key);
                    }
                    _ => {
                        delay = Some(self.em.delays.push(DelaySpec { key, len: d }));
                    }
                }
            }
            output_refs.push(src);
            self.em.push_reading(
                Pre::Output {
                    channel,
                    src,
                    delay,
                },
                &[src],
            );
        }
        output_refs
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
                    align_audio(&mut self.em, v, dep.gap_to(total), key(lane, source))
                }
                Some((v, _)) => ARef::Val(v),
                None => ARef::Zero,
            })
            .collect()
    }

    /// Taps: one lane per source, any ports. A lane whose port does not exist
    /// reads silence rather than failing the commit, as a meter's does; it
    /// gets no delay ring, and the other lanes' rings are keyed by their own
    /// lane and source. A closed tap is not in the topology, so nothing is
    /// emitted for it. One op a tap, after every lane's delay: it writes the
    /// lanes together.
    fn taps(&mut self, output_refs: &[ARef]) {
        let (graph, timing) = (self.graph, self.timing);
        for (&tap, spec) in &graph.topology().taps {
            let srcs: Vec<ARef> = match spec {
                // What each output channel carries, post-sum and aligned; a
                // lane past the output count is silence.
                crate::TapSpec::Outputs { width } => (0..*width)
                    .map(|c| {
                        output_refs
                            .get(c)
                            .filter(|_| c < timing.main_output_width)
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
            let tap = self.em.taps.push(tap);
            self.em.push_reading(
                Pre::Tap {
                    tap,
                    srcs: srcs.clone(),
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
        let (graph, units, timing) = (self.graph, self.units, self.timing);
        for (&tap, spec) in &graph.topology().event_taps {
            let index = self.em.event_taps.push(tap);
            let Some(from) = spec.source else {
                continue;
            };
            // What the port carries, and when it departs (an output) or
            // arrives (an input, which an aligned tap delays to the output
            // time too).
            let (carried, at_time) = match from {
                crate::EventTapPort::Out(port) => {
                    let (Some(v), Some(n)) = (self.event_out(port), units.get(port.node)) else {
                        continue;
                    };
                    (ERef::Val(v), timing.departure[n])
                }
                crate::EventTapPort::In(at) => {
                    let (Some(r), Some(n)) = (self.event_in(at), units.get(at.node)) else {
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
                (crate::TapAlign::ToOutput, ERef::Val(v)) => ERef::Val(align_event(
                    &mut self.em,
                    v,
                    at_time.gap_to(timing.total),
                    DelayKey::EventTap { tap, from },
                )),
                _ => carried,
            };
            let op = self.em.push(Pre::EventTap { tap: index, src });
            self.em.read_eref(src, op);
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
            let meter = self.em.meters.push((meter, srcs.len()));
            self.em.push_reading(
                Pre::Meter {
                    meter,
                    srcs: srcs.clone(),
                },
                &srcs,
            );
        }
    }

    /// Captures run last. They feed the feedback delays, which the executor
    /// reads from *before* the block (into the feedback slots), so a capture
    /// needs no ordering against the feedback's readers: with a delay of a
    /// whole `MaxBlock`, nothing it queues is due this block.
    fn captures(&mut self) {
        let audio: Vec<(AudioFeedbackId, OutputPort)> = self
            .em
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
            let op = self.em.push(Pre::Capture { feedback, src });
            self.em.read_audio(src, op);
        }
        let events: Vec<(EventFeedbackId, EventOutputPort)> = self
            .em
            .event_feedback
            .iter_enumerated()
            .map(|(feedback, key)| match *key {
                FeedbackKey::Event { from, .. } => (feedback, from),
                FeedbackKey::Audio { .. } => unreachable!("event feedback keys are events"),
            })
            .collect();
        for (feedback, from) in events {
            let src = self.event_source(from);
            let op = self.em.push(Pre::EventCapture { feedback, src });
            self.em.read_event(src, op);
        }
    }
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
