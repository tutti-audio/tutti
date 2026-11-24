//! Small test nodes and a harness that drives the executor.
//!
//! Every node here is deterministic, declares an **honest** tail (the
//! executor's silence skip trusts it), and never turns silence into `-0.0`
//! (all gains are positive): a skipped node writes `+0.0`, so a node that
//! produced `-0.0` from silence would differ in the sign bit only, which the
//! bit-exact comparisons of the tests would report as a failure of the skip rather
//! than of the node.

// Test support, not API: its items are named by the tests that use them.
#![allow(
    dead_code,
    missing_docs,
    missing_debug_implementations,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_buffers::Float;
use tutti_buffers::MaxBlock;

use crate::NodeKey;
use crate::{
    Cx, Editor, Event, EventKind, Executor, Fade, Io, Node, Plan, Prepare, Shape, Shapes, Status,
    TransportChanges, TransportPosition, ValidGraph, compile,
};
use tutti_buffers::ChannelLayout;
use tutti_ump::Ump;
use tutti_units::{Frame, Latency, SampleRate, Samples, Tail};

pub const RATE: SampleRate = SampleRate::SR_48K;

/// [`DECLICK`](crate::DECLICK) in frames at [`RATE`].
pub fn declick() -> usize {
    crate::declick_frames(RATE).get()
}

/// `(out, in)` at frame `j` of an `n`-frame equal-amplitude fade, from the
/// smootherstep `x³(6x² − 15x + 10)` at `x = (j + 1) / (n + 1)` written out in
/// `f64`: the figure the fade tests hold the engine to.
pub fn smooth_gains(j: usize, n: usize) -> (f64, f64) {
    let x = (j + 1) as f64 / (n + 1) as f64;
    let s = x * x * x * (6.0 * x * x - 15.0 * x + 10.0);
    (1.0 - s, s)
}

/// How many times an `EventLag` forwards one event.
pub const MAX_HOPS: u32 = 2;

/// Events per slot in the harness: far above what a generated graph produces,
/// so a refused event is a declared capacity's (`Kind::Burst`) and nothing
/// else's.
pub const EVENT_CAPACITY: usize = 16_384;

/// What a [`Kind::GainDriver`] steps its gain to at frame `f`: a figure
/// that changes at every step and is exact in `f32`.
pub fn driver_value(f: u64, period: u64) -> f64 {
    ((f / period) % 7) as f64 * 0.25 - 0.5
}

pub fn prepare(max_block: usize) -> Prepare {
    Prepare::new(RATE, MaxBlock::of(Samples::new(max_block)))
}

fn lat(frames: usize) -> Latency {
    Latency::new(Samples::new(frames))
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::try_from(n).expect("a test width is small")
}

/// What a test node does. `Clone` so the harness builds a fresh unit of a kind
/// each time it places a key.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Constant `value` on `width` outputs; writes sample 0 and returns
    /// `Status::Constant`.
    Const { value: f32, width: usize },
    /// `width` channels scaled by `gain`; accepts in-place channels.
    Gain { gain: f32, width: usize },
    /// Sum of `inputs` channels onto one output.
    Sum { inputs: usize },
    /// `y[n] = 0.5 (x[n] + x[n-1])`: one sample of memory, in place.
    Smooth,
    /// A pure delay of `latency` frames, declared as processing latency.
    Lag { latency: usize },
    /// Emits a MIDI event every `period` frames of absolute time, offset by
    /// `phase`; also outputs how many it has emitted, as audio.
    Emitter { period: u64, phase: u64 },
    /// Emits `burst` MIDI events on every frame where `(frame + phase) %
    /// period == 0`, on a port declaring `cap` events per block
    /// (`Shape::event_capacity`); also outputs how many its writer
    /// accepted, as audio. With `cap < burst` the writer refuses the newest
    /// of each burst — the same ones on every participant count. `period` is at
    /// least `MAX_BLOCK` in the generator, so the port keeps its declared
    /// *rate* (at most `cap` per `MaxBlock` frames) and nothing downstream
    /// may drop.
    Burst {
        period: u64,
        phase: u64,
        burst: u32,
        cap: u32,
    },
    /// A lane for an edge gain: on every frame that is
    /// a multiple of `period` it steps the gain it drives to
    /// [`driver_value`] (a zero-length ramp addressed to `EdgeGain::ADDR`).
    /// It declares `latency` frames of processing latency it does not
    /// realise, so its events leave late for the PDC tests.
    GainDriver { period: u64, latency: usize },
    /// [`Kind::GainDriver`]'s twin for a node's mix: the same steps doubled
    /// (`-1.0` to `2.0`, off `0..=1` on both sides, for the clamp), as a `Mix`
    /// addressed to `NodeMix::ADDR`.
    MixDriver { period: u64, latency: usize },
    /// Folds incoming events into a running value: `s = 0.5 s + (w0 % 97)`.
    /// Order-sensitive, so a merge that reorders ties is caught.
    Consumer { inputs: u16 },
    /// [`Kind::Consumer`] declaring that its event inputs together accept at
    /// most `cap` events per block (`Shape::event_input_capacity`).
    BoundedConsumer { inputs: u16, cap: u32 },
    /// Re-emits its input events `latency` frames later.
    EventLag { latency: usize },
    /// Reads the environment: frame, arrival latency, tempo.
    EnvProbe,
    /// `Status::Bypass` over `width` channels.
    Thru { width: usize },
    /// `Status::Bypass` over `width` channels, accepting in-place channels —
    /// the executor must then leave the aliased output as it is.
    ThruInPlace { width: usize },
    /// `width` in-place audio channels scaled by a gain its event inputs
    /// steer, and every output event port forwarding one input port. Audio
    /// and events in one node, at any width: it reaches the executor's
    /// general borrow path in every bucket, with or without in-place
    /// channels.
    Mixed {
        width: usize,
        events_in: u16,
        events_out: u16,
    },
    /// A spec-driven node for the fixed compile shapes: `dc`/`gain`/`sum`/`fan`
    /// behaviour with a *declared* latency and tail it does not realise.
    Spec {
        behaviour: SpecBehaviour,
        ins: usize,
        outs: usize,
        latency: usize,
        tail: Tail,
    },
    /// `inner`, declaring these buses on its shape: the node behaves as `inner` and
    /// only its `Shape::buses_in` / `buses_out` differ.
    Bused {
        inner: Box<Kind>,
        buses_in: Box<crate::Buses>,
        buses_out: Box<crate::Buses>,
    },
    /// `inner`, declaring `Shape::own_bypass`: the node behaves as
    /// `inner` and the executor never blends or parks it in `Bypassed`.
    OwnBypass { inner: Box<Kind> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpecBehaviour {
    Dc(f32),
    Gain(f32),
    Sum,
    Fan,
}

pub struct TestNode<F: Float = f32> {
    kind: Kind,
    /// The handles its kind's shape issued, in port order.
    event_inputs: Vec<crate::InEvents>,
    event_outputs: Vec<crate::OutEvents>,
    ring: VecDeque<F>,
    prev: F,
    count: f32,
    state: f32,
    pending: VecDeque<(Frame, Event)>,
    /// The rate last prepared for.
    rate: Option<f64>,
    calls: Arc<AtomicUsize>,
    /// What its shape declares to the cost model (`Cost::Heavy`, the
    /// default, unless set with `cost`).
    cost: crate::Cost,
    /// The buses a `Kind::Bused` declares.
    buses: Option<(crate::Buses, crate::Buses)>,
    /// Whether a `Kind::OwnBypass` wrapped the kind.
    own_bypass: bool,
}

impl TestNode {
    /// An `f32` node. [`in_format`](TestNode::in_format) builds one for
    /// another sample format.
    pub fn new(kind: Kind) -> Self {
        Self::in_format(kind)
    }

    /// An `f32` node that counts its `process` calls into `calls`.
    pub fn counted(kind: Kind, calls: Arc<AtomicUsize>) -> Self {
        Self::counted_in(kind, calls)
    }
}

impl<F: Float> TestNode<F> {
    /// A node of `kind` in sample format `F`.
    pub fn in_format(kind: Kind) -> Self {
        Self::counted_in(kind, Arc::new(AtomicUsize::new(0)))
    }

    /// A node that counts its `process` calls into `calls`.
    pub fn counted_in(kind: Kind, calls: Arc<AtomicUsize>) -> Self {
        let (kind, own_bypass) = match kind {
            Kind::OwnBypass { inner } => (*inner, true),
            kind => (kind, false),
        };
        let (kind, buses) = match kind {
            Kind::Bused {
                inner,
                buses_in,
                buses_out,
            } => (*inner, Some((*buses_in, *buses_out))),
            kind => (kind, None),
        };
        let ring = match kind {
            Kind::Lag { latency } => VecDeque::from(vec![F::ZERO; latency]),
            _ => VecDeque::new(),
        };
        let mut node = Self {
            kind,
            event_inputs: Vec::new(),
            event_outputs: Vec::new(),
            ring,
            prev: F::ZERO,
            count: 0.0,
            state: 0.0,
            pending: VecDeque::with_capacity(1024),
            rate: None,
            calls,
            cost: crate::Cost::default(),
            buses,
            own_bypass,
        };
        let shape = node.kind_shape();
        node.event_inputs = shape.event_inputs().collect();
        node.event_outputs = shape.event_outputs().collect();
        node
    }

    /// This node, declaring `cost` (`Shape::cost`).
    pub fn cost(mut self, cost: crate::Cost) -> Self {
        self.cost = cost;
        self
    }
}

impl<F: Float> TestNode<F> {
    fn kind_shape(&self) -> Shape {
        match self.kind {
            Kind::Bused { .. } | Kind::OwnBypass { .. } => {
                unreachable!("`counted` unwraps the buses and the own bypass")
            }
            Kind::Const { width, .. } => Shape::source_unassigned(ch(width)),
            Kind::Gain { width, .. } => Shape::through_unassigned(ch(width)).in_place(),
            // Any number of inputs, which are not a bus: ports.
            Kind::Sum { inputs } => Shape::EMPTY.ports(ch(inputs), ch(1)),
            Kind::Smooth => Shape::through_unassigned(ch(1))
                .tail(Tail::Finite(Samples::new(1)))
                .in_place(),
            // A pure delay declared as latency: its tail, which excludes
            // latency, is none.
            Kind::Lag { latency } => Shape::through_unassigned(ch(1))
                .latency(lat(latency))
                .tail(Tail::None),
            Kind::Emitter { .. } => Shape::source_unassigned(ch(1)).events(0, 1),
            Kind::GainDriver { latency, .. } | Kind::MixDriver { latency, .. } => Shape::EMPTY
                .events(0, 1)
                .param_events()
                .latency(lat(latency))
                .tail(Tail::None),
            Kind::Burst { cap, .. } => Shape::source_unassigned(ch(1))
                .events(0, 1)
                .event_capacity(cap_of(cap)),
            Kind::Consumer { inputs } => Shape::source_unassigned(ch(1))
                .events(inputs, 0)
                .param_events()
                .tail(Tail::Unbounded),
            Kind::BoundedConsumer { inputs, cap } => Shape::source_unassigned(ch(1))
                .events(inputs, 0)
                .tail(Tail::Unbounded)
                .event_input_capacity(cap_of(cap)),
            Kind::EventLag { latency } => Shape::EMPTY
                .events(1, 1)
                .latency(lat(latency))
                .tail(Tail::None),
            Kind::EnvProbe => Shape::source_unassigned(ch(1)),
            Kind::Thru { width } => Shape::through_unassigned(ch(width)),
            Kind::ThruInPlace { width } => Shape::through_unassigned(ch(width)).in_place(),
            Kind::Mixed {
                width,
                events_in,
                events_out,
            } => Shape::through_unassigned(ch(width))
                .events(events_in, events_out)
                .tail(Tail::Unbounded)
                .in_place(),
            Kind::Spec {
                ins,
                outs,
                latency,
                tail,
                ..
            } => Shape::buses(
                crate::Buses::unassigned(ch(ins)),
                crate::Buses::unassigned(ch(outs)),
            )
            .latency(lat(latency))
            .tail(tail),
        }
    }
}

/// An event capacity of `n` (non-zero; a literal zero fails at compile time in
/// a `const`, and at the call otherwise).
#[allow(dead_code)]
pub const fn cap_of(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(n) => n,
        None => panic!("a test event capacity of zero"),
    }
}

/// `shape`'s figures over these buses, its ports kept.
#[allow(dead_code)]
pub fn with_buses(shape: Shape, buses_in: crate::Buses, buses_out: crate::Buses) -> Shape {
    let mut bused = Shape::buses(buses_in, buses_out).ports(shape.audio_in(), shape.audio_out());
    bused.event_in = shape.event_in;
    bused.event_out = shape.event_out;
    bused.event_capacity = shape.event_capacity;
    bused.event_input_capacity = shape.event_input_capacity;
    bused.latency = shape.latency;
    bused.tail = shape.tail;
    bused.in_place = shape.in_place;
    bused.event_resolution = shape.event_resolution;
    bused.params = shape.params;
    bused.cost = shape.cost;
    bused.note_events = shape.note_events;
    bused.own_bypass = shape.own_bypass;
    bused
}

impl<F: Float> Node<F> for TestNode<F> {
    fn shape(&self) -> Shape {
        let mut shape = self.kind_shape().cost(self.cost);
        if self.own_bypass {
            shape = shape.own_bypass();
        }
        match self.buses {
            // The kind's figures over the buses the test declared.
            Some((buses_in, buses_out)) => with_buses(shape, buses_in, buses_out),
            None => shape,
        }
    }

    fn prepare(&mut self, p: &Prepare) {
        // `Frame` means samples at the current rate: an `EventLag` holding
        // absolute due frames moves them to the same wall-clock time when the
        // rate changes, as the executor moves its own clock.
        let rate = p.sample_rate().get();
        if let Some(old) = self.rate.filter(|&old| old != rate) {
            for (due, _) in &mut self.pending {
                *due = due.scaled(rate / old);
            }
        }
        self.rate = Some(rate);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let n = io.frames();
        match self.kind {
            Kind::Bused { .. } | Kind::OwnBypass { .. } => {
                unreachable!("`counted` unwraps the buses and the own bypass")
            }
            Kind::Const { value, .. } => {
                for c in 0..io.output_count() {
                    io.output(c)[0] = F::from_f32(value);
                }
                Status::Constant
            }
            Kind::Gain { gain, width } => {
                for c in 0..width {
                    let gain = F::from_f32(gain);
                    io.channel(c).map(|x| x * gain);
                }
                Status::Modified
            }
            Kind::Sum { inputs } => {
                let (ins, mut outs) = io.split();
                let out = outs.get(0);
                out.fill(F::ZERO);
                for c in 0..inputs {
                    for (o, &i) in out.iter_mut().zip(ins.get(c)) {
                        *o += i;
                    }
                }
                Status::Modified
            }
            Kind::Smooth => {
                let prev = &mut self.prev;
                io.channel(0).map(|now| {
                    let y = F::lit(0.5) * (now + *prev);
                    *prev = now;
                    y
                });
                Status::Modified
            }
            Kind::Lag { latency } => {
                let (ins, mut outs) = io.split();
                for (o, &i) in outs.get(0).iter_mut().zip(ins.get(0)) {
                    if latency == 0 {
                        *o = i;
                    } else {
                        // Pop before push, so the ring never grows past the
                        // capacity it was built with (the no-alloc test runs
                        // this node).
                        *o = self.ring.pop_front().expect("ring");
                        self.ring.push_back(i);
                    }
                }
                Status::Modified
            }
            Kind::Emitter { period, phase } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if (f + phase).is_multiple_of(period) {
                        let _ = io
                            .event_out(self.event_outputs[0])
                            .push(Event::midi(at, Ump([f as u32, 0, 0, 0])));
                        self.count += 1.0;
                    }
                }
                io.output(0).fill(F::from_f32(self.count));
                Status::Modified
            }
            Kind::GainDriver { period, .. } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if f.is_multiple_of(period) {
                        let ramp = crate::ParamRamp::new(
                            tutti_units::ParamKey::<tutti_units::Amplitude>::VOLUME,
                            tutti_units::Amplitude::new(driver_value(f, period)),
                            Samples::new(0),
                        );
                        let _ = io
                            .event_out(self.event_outputs[0])
                            .push_param(crate::ParamEvent::ramp(at, ramp));
                    }
                }
                Status::Modified
            }
            Kind::MixDriver { period, .. } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if f.is_multiple_of(period) {
                        let ramp = crate::ParamRamp::new(
                            tutti_units::ParamKey::<tutti_units::Mix>::WET.at(u8::MAX),
                            tutti_units::Mix::new(driver_value(f, period) * 2.0),
                            Samples::new(0),
                        );
                        let _ = io
                            .event_out(self.event_outputs[0])
                            .push_param(crate::ParamEvent::ramp(at, ramp));
                    }
                }
                Status::Modified
            }
            Kind::Burst {
                period,
                phase,
                burst,
                ..
            } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if (f + phase).is_multiple_of(period) {
                        for i in 0..burst {
                            let e = Event::midi(at, Ump([f as u32 + i, 0, 0, 0]));
                            if io.event_out(self.event_outputs[0]).push(e).is_ok() {
                                self.count += 1.0;
                            }
                        }
                    }
                }
                io.output(0).fill(F::from_f32(self.count));
                Status::Modified
            }
            Kind::Consumer { inputs } | Kind::BoundedConsumer { inputs, .. } => {
                // A merge of this node's own ports, in port order, so several
                // event inputs are consumed deterministically too.
                let mut heads = [0usize; 8];
                for i in 0..n {
                    for (p, h) in (0..inputs).zip(heads.iter_mut()) {
                        let evs = io.events(self.event_inputs[usize::from(p)]);
                        while *h < evs.len() && evs[*h].offset.index() == i {
                            if let EventKind::Midi(Ump(w)) = evs[*h].kind {
                                self.state = 0.5 * self.state + (w[0] % 97) as f32 + f32::from(p);
                            }
                            *h += 1;
                        }
                    }
                    io.output(0)[i] = F::from_f32(self.state);
                }
                Status::Modified
            }
            Kind::EventLag { latency } => {
                for e in io.events(self.event_inputs[0]) {
                    // Word 1 counts hops. Forwarding at most `MAX_HOPS`
                    // times bounds how far an event feedback loop can
                    // multiply events -- without it a random graph can
                    // amplify exponentially until the executor's
                    // preallocated slots overflow.
                    let EventKind::Midi(Ump(mut w)) = e.kind else {
                        continue;
                    };
                    if w[1] >= MAX_HOPS {
                        continue;
                    }
                    w[1] += 1;
                    let fwd = Event::midi(e.offset, Ump(w));
                    // Kept sorted by due frame (stably): after a rate change
                    // rescaled the older entries, new ones can fall due
                    // before them. `insert` stays within the reserved
                    // capacity, so it does not allocate.
                    let due = cx.env.frame_at(e.offset) + Samples::new(latency);
                    let at = self.pending.partition_point(|&(d, _)| d <= due);
                    self.pending.insert(at, (due, fwd));
                }
                while let Some(&(due, e)) = self.pending.front() {
                    // Queued in due order. One behind the block — the clock
                    // jumped: a re-prepare's suspension, or a rate change
                    // rescaling it — goes out at once rather than never.
                    let offset = match cx.env.offset_of(due) {
                        Some(o) => o,
                        None if due < cx.env.frame => crate::Offset::ZERO,
                        None => break,
                    };
                    self.pending.pop_front();
                    let _ = io
                        .event_out(self.event_outputs[0])
                        .push(Event { offset, ..e });
                }
                Status::Modified
            }
            Kind::EnvProbe => {
                let base =
                    cx.arrival.samples().get() as f32 + cx.env.transport.tempo.get() as f32 * 1e-3;
                // The transport per frame, changes included, so every
                // participant count must hand every node the same `Env`.
                let env = *cx.env;
                for (k, o) in env.offsets().zip(io.output(0).iter_mut()) {
                    let t = env.transport_at(k);
                    let moving = if t.playing { 0.5 } else { 0.0 };
                    *o = F::from_f32(
                        ((env.frame.get() + k.get() as u64) % 1000) as f32 * 1e-3
                            + base
                            + moving
                            + t.beat().get().fract() as f32,
                    );
                }
                Status::Modified
            }
            Kind::Thru { .. } | Kind::ThruInPlace { .. } => Status::Bypass,
            Kind::Mixed {
                width,
                events_in,
                events_out,
            } => {
                // The gain follows every event on every port, in port order
                // (order-sensitive, like `Consumer`), and stays positive so
                // silence stays `+0.0`.
                for p in 0..events_in {
                    for e in io.events(self.event_inputs[usize::from(p)]) {
                        if let EventKind::Midi(Ump(w)) = e.kind {
                            self.state =
                                0.5 * self.state + ((w[0] % 7) as f32 + f32::from(p)) * 0.125;
                        }
                    }
                }
                let gain = 1.0 + self.state;
                for c in 0..width {
                    let g = F::from_f32(gain * (c + 1) as f32);
                    io.channel(c).map(|x| x * g);
                }
                if events_in > 0 {
                    for q in 0..events_out {
                        let from = io.events(self.event_inputs[usize::from(q % events_in)]);
                        for &e in from {
                            let _ = io.event_out(self.event_outputs[usize::from(q)]).push(e);
                        }
                    }
                }
                Status::Modified
            }
            Kind::Spec {
                behaviour,
                outs,
                ins: width,
                ..
            } => {
                let (ins, mut o) = io.split();
                for c in 0..outs {
                    for i in 0..n {
                        let v = match behaviour {
                            SpecBehaviour::Dc(v) => F::from_f32(v),
                            SpecBehaviour::Gain(g) => {
                                if c < width {
                                    ins.get(c)[i] * F::from_f32(g)
                                } else {
                                    F::ZERO
                                }
                            }
                            SpecBehaviour::Sum => (0..width).map(|k| ins.get(k)[i]).sum(),
                            SpecBehaviour::Fan => {
                                if width == 0 {
                                    F::ZERO
                                } else {
                                    ins.get(c % width)[i]
                                }
                            }
                        };
                        o.get(c)[i] = v;
                    }
                }
                Status::Modified
            }
        }
    }

    fn reset(&mut self) {
        // In place: the ring keeps its length and `pending` its room, so a reset
        // allocates nothing.
        self.ring.iter_mut().for_each(|v| *v = F::ZERO);
        self.prev = F::ZERO;
        self.count = 0.0;
        self.state = 0.0;
        self.pending.clear();
        self.rate = None;
    }
}

/// Shapes for every node in `kinds`.
/// The topology entry of a node of `kind`: its audio widths, latency and
/// tail, as the node's own shape declares them.
pub fn spec_of(kind: &Kind) -> crate::NodeSpec {
    let s = TestNode::new(kind.clone()).shape();
    crate::NodeSpec::new("test", s.audio_in(), s.audio_out())
        .latency(s.latency.samples())
        .tail(s.tail)
}

pub fn shapes_of(kinds: &BTreeMap<NodeKey, Kind>) -> Shapes {
    shapes_costed(kinds, &BTreeMap::new())
}

/// Shapes for every node in `kinds`, those in `costs` declaring that cost.
pub fn shapes_costed(
    kinds: &BTreeMap<NodeKey, Kind>,
    costs: &BTreeMap<NodeKey, crate::Cost>,
) -> Shapes {
    kinds
        .iter()
        .map(|(&k, kind)| (k, costed::<f32>(k, kind, costs).shape()))
        .collect()
}

fn costed<F: Float>(
    k: NodeKey,
    kind: &Kind,
    costs: &BTreeMap<NodeKey, crate::Cost>,
) -> TestNode<F> {
    let node = TestNode::<F>::in_format(kind.clone());
    match costs.get(&k) {
        Some(&c) => node.cost(c),
        None => node,
    }
}

/// Fresh units for `keys`, those in `costs` declaring that cost.
pub fn units_costed<F: Float>(
    kinds: &BTreeMap<NodeKey, Kind>,
    costs: &BTreeMap<NodeKey, crate::Cost>,
    keys: impl IntoIterator<Item = NodeKey>,
) -> BTreeMap<NodeKey, Box<dyn Node<F>>> {
    keys.into_iter()
        .map(|k| {
            (
                k,
                Box::new(costed::<F>(k, &kinds[&k], costs)) as Box<dyn Node<F>>,
            )
        })
        .collect()
}

/// The executor, driven block by block over a graph the harness compiles
/// itself and ships through [`Editor::package`].
pub struct Harness<F: Float = f32> {
    pub editor: Editor<F>,
    pub exec: Executor<F>,
    pub plan: Option<Plan>,
    pub outputs: usize,
    pub inputs: usize,
    pub max_block: usize,
    /// Declared costs (`Shape::cost`) by key; every other node declares
    /// none (`Cost::Heavy`).
    pub costs: BTreeMap<NodeKey, crate::Cost>,
    /// The graph last switched to, for the editor's spec (`set_mode`).
    graph: Option<ValidGraph>,
}

impl Harness {
    /// An `f32` harness. [`with_format`](Harness::with_format) builds one for
    /// another sample format.
    pub fn new(max_block: usize) -> Self {
        Self::with_format(max_block)
    }

    /// As [`new`](Self::new), with the executor running blocks on a shared
    /// [`TestPool`] of `workers` participants (`1`: no pool).
    pub fn with_workers(max_block: usize, workers: usize) -> Self {
        let mut harness = Self::new(max_block);
        pooled(&mut harness.exec, workers);
        harness
    }
}

impl<F: Float> Harness<F> {
    /// A harness in sample format `F`.
    pub fn with_format(max_block: usize) -> Self {
        let (editor, exec) = Editor::with_event_capacity(prepare(max_block), EVENT_CAPACITY);
        Self {
            editor,
            exec,
            plan: None,
            outputs: 0,
            inputs: 0,
            max_block,
            costs: BTreeMap::new(),
            graph: None,
        }
    }

    /// Switch to `graph`, building fresh units for whatever is new or
    /// regenerated.
    pub fn switch(&mut self, graph: &ValidGraph, kinds: &BTreeMap<NodeKey, Kind>) {
        self.switch_with_fades(graph, kinds, &BTreeMap::new());
    }

    /// `switch`, with the regenerated keys in `fades` crossfading
    /// (`Editor::replace`'s contract). The executor gets the fades for the
    /// keys its delta replaces, as `Editor::commit` attaches them.
    ///
    /// A node's **declared latency is the spec's**, as after
    /// `Editor::set_latency`: the shapes compiled against take it from
    /// `graph`, not from the kind. A kept unit whose declared latency
    /// changed has its crossfade cut (`Delta::cuts`, as `Editor::commit`
    /// attaches it, derived here from the two plans).
    pub fn switch_with_fades(
        &mut self,
        graph: &ValidGraph,
        kinds: &BTreeMap<NodeKey, Kind>,
        fades: &BTreeMap<NodeKey, Fade>,
    ) {
        let mut shapes = shapes_costed(kinds, &self.costs);
        for (k, s) in shapes.iter_mut() {
            if let Some(n) = graph.topology().nodes.get(k) {
                s.latency = Latency::new(n.latency);
            }
        }
        let (plan, mut delta) =
            compile(graph, &shapes, &prepare(self.max_block), self.plan.as_ref())
                .expect("compiles");
        delta.cuts = plan
            .units()
            .iter()
            .filter(|now| {
                self.plan
                    .as_ref()
                    .and_then(|p| p.unit(now.key))
                    .is_some_and(|was| {
                        was.generation == now.generation
                            && was.idx == now.idx
                            && was.shape.latency != now.shape.latency
                    })
            })
            .map(|u| crate::Placement {
                key: u.key,
                generation: u.generation,
                idx: u.idx,
            })
            .collect();
        plan.verify().expect("verifies");
        delta.fades = delta
            .replace
            .iter()
            .filter_map(|&(_, new)| fades.get(&new.key).map(|&f| (new.key, f)))
            .collect();
        let placed: Vec<NodeKey> = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, n)| n.key))
            .collect();
        self.outputs = graph.topology().outputs.len();
        self.inputs = graph.topology().inputs.count() as usize;
        self.graph = Some(graph.clone());
        self.plan = Some(plan.clone());
        // The editor sends each weighted lane's cell from its own spec, and
        // `set_edge_gain` checks the edge against it: with edge gains in
        // play, the editor's spec is the graph's (a harness that compiles
        // itself has otherwise left it empty).
        if !graph.gains().is_empty() || !self.editor.spec().gains.is_empty() {
            self.editor.spec_mut().topology = graph.topology().clone();
            self.editor.enter_gains(graph.gains().clone());
        }
        if !graph.params().is_empty() || !self.editor.spec_mut().params.is_empty() {
            self.editor.spec_mut().params = graph.params().clone();
        }
        // A lane the graph no longer has takes its base and range with it.
        self.editor
            .values_mut()
            .retain_edges(|id| graph.gains().contains_key(&id));
        // The node values name nodes of the graph switched to: a node that is
        // gone takes its values with it (`Editor::remove`'s rule), and
        // `set_mode` checks the node against the editor's spec.
        let live_nodes = graph
            .topology()
            .nodes
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        self.editor
            .values_mut()
            .retain(|key| live_nodes.contains(&key));
        if !self.editor.values().is_empty() {
            self.editor.spec_mut().topology = graph.topology().clone();
        }
        // Back on the control side, where the box and what it retired are
        // freed.
        self.editor
            .package(plan, delta, units_costed(kinds, &self.costs, placed))
            .expect("the harness collects every box");
        self.exec.apply_pending();
        self.editor.collect();
    }

    /// Move edge `id`'s weight to `base`, as a host moving a fader: the
    /// editor's `set_edge_gain` (a store, no commit).
    pub fn set_edge_gain(&mut self, id: crate::EdgeId, base: tutti_units::Gain) {
        self.editor
            .set_edge_gain(id, base)
            .expect("the edge has a gain entry");
    }

    /// Sets the mode a unit placed at `key` by the next switch starts
    /// steady in: before the first block, no fade. After a switch it is
    /// [`set_mode`](Self::set_mode) that moves a running unit.
    pub fn start_mode(&mut self, key: NodeKey, mode: crate::NodeMode) {
        self.editor.values_mut().set_mode(key, mode);
    }

    /// Sets the base a lane declared by the next switch starts at.
    pub fn start_edge_gain(&mut self, id: crate::EdgeId, base: tutti_units::Gain) {
        self.editor.values_mut().set_base(id, base);
    }

    /// Sets the range a lane declared by the next switch starts with.
    pub fn start_edge_gain_range(&mut self, id: crate::EdgeId, range: crate::ParamRange) {
        self.editor.values_mut().set_gain_range(id, range);
    }

    /// Sets the range a port of a node placed by the next switch starts with.
    pub fn start_param_range(&mut self, at: crate::ParamIn, range: crate::ParamRange) {
        self.editor.values_mut().set_range(at, range);
    }

    /// As [`start_mode`](Self::start_mode), for the mix.
    pub fn start_mix(&mut self, key: NodeKey, mix: tutti_units::Mix) {
        self.editor.values_mut().set_mix(key, mix);
    }

    /// Gives the editor the graph's topology when its spec lacks `key`
    /// (`set_mode` and `set_mix` check the node against it, and a harness
    /// that compiles itself has left it empty).
    fn know(&mut self, key: NodeKey) {
        if !self.editor.spec().topology.nodes.contains_key(&key) {
            let graph = self.graph.as_ref().expect("a graph was switched to");
            self.editor.spec_mut().topology = graph.topology().clone();
        }
    }

    /// Move `key`'s mode, as a host clicking bypass: the editor's `set_mode`
    /// (a store, no commit).
    pub fn set_mode(&mut self, key: NodeKey, mode: crate::NodeMode) {
        self.know(key);
        self.editor
            .set_mode(key, mode)
            .expect("the harness's editor holds the graph's nodes");
    }

    /// Move `key`'s mix, as a host moving a send knob: the editor's
    /// `set_mix` (a store, no commit).
    pub fn set_mix(&mut self, key: NodeKey, mix: tutti_units::Mix) {
        self.know(key);
        self.editor
            .set_mix(key, mix)
            .expect("the harness's editor holds the graph's nodes");
    }

    /// Move `key`'s mix range, as a host narrowing a send's knob: the
    /// editor's `set_param_range` at the mix's address (a store, no commit).
    pub fn set_mix_range(&mut self, key: NodeKey, range: crate::ParamRange) {
        self.know(key);
        let at = crate::ParamIn {
            node: key,
            param: crate::NodeMix::ADDR,
        };
        self.editor
            .set_param_range(at, range)
            .expect("the harness's editor holds the graph's nodes");
    }

    /// Move edge `id`'s range: the editor's `set_edge_gain_range` (a store,
    /// no commit).
    pub fn set_edge_gain_range(&mut self, id: crate::EdgeId, range: crate::ParamRange) {
        self.editor
            .set_edge_gain_range(id, range)
            .expect("the edge has a gain entry");
    }

    /// Channel `c` of the graph input is the signal scaled by 2^-c: exact,
    /// and distinct per channel.
    pub fn channels(&self, frames: usize, input: &[F]) -> Vec<Vec<F>> {
        (0..self.inputs.max(1))
            .map(|c| {
                input[..frames]
                    .iter()
                    .map(|&x| x * F::from_f32(0.5f32.powi(c as i32)))
                    .collect()
            })
            .collect()
    }

    /// Render one block; return the output channels.
    pub fn block(&mut self, frames: usize, input: &[F]) -> Vec<Vec<F>> {
        let transport = TransportPosition::new(
            true,
            tutti_units::Bpm::of(120.0),
            tutti_units::Beat::new(0.0),
            None,
        );
        self.block_at(frames, input, &transport)
    }

    /// `block`, under a transport the caller moves.
    pub fn block_at(
        &mut self,
        frames: usize,
        input: &[F],
        transport: &TransportPosition,
    ) -> Vec<Vec<F>> {
        self.block_with_changes(frames, input, transport, &TransportChanges::NONE)
    }

    /// `block_at`, with the transport changing inside the block.
    pub fn block_with_changes(
        &mut self,
        frames: usize,
        input: &[F],
        transport: &TransportPosition,
        changes: &TransportChanges,
    ) -> Vec<Vec<F>> {
        let chans = self.channels(frames, input);
        let ins: Vec<&[F]> = chans.iter().map(Vec::as_slice).collect();
        let mut out = vec![vec![F::ZERO; frames]; self.outputs];
        let mut outs: Vec<&mut [F]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        self.exec
            .process_with_changes(frames, transport, changes, &ins, &mut outs);
        out
    }

    /// Render `blocks` (each a block length) from frame `from` on the
    /// harness's own input, [`input_signal`], and return every output channel
    /// concatenated over the blocks. Each block is [`block`](Self::block).
    pub fn run(&mut self, blocks: &[usize], from: u64) -> Vec<Vec<F>> {
        self.run_with(blocks, from, input_signal)
    }

    /// [`run`](Self::run) over the input `signal(start, frames)` gives for
    /// each block, where `start` is the frame the block begins at (`from`
    /// plus the lengths of the blocks before it).
    pub fn run_with(
        &mut self,
        blocks: &[usize],
        from: u64,
        mut signal: impl FnMut(u64, usize) -> Vec<F>,
    ) -> Vec<Vec<F>> {
        let mut all: Vec<Vec<F>> = vec![Vec::new(); self.outputs];
        let mut frame = from;
        for &n in blocks {
            let input = signal(frame, n);
            for (all, out) in all.iter_mut().zip(self.block(n, &input)) {
                all.extend(out);
            }
            frame += n as u64;
        }
        all
    }
}

/// Run `exec`'s blocks on the shared [`test_pool`] of `workers`
/// participants; `1` leaves it serial (no pool).
pub fn pooled<F: Float>(exec: &mut Executor<F>, workers: usize) {
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
}

/// As [`pooled`], on a fresh [`TestPool`] that no other executor of the
/// process shares.
pub fn pooled_fresh<F: Float>(exec: &mut Executor<F>, workers: usize) {
    if workers > 1 {
        exec.set_pool(Some(TestPool::new(workers)));
    }
}

/// Call `render(1)`, the serial render, then `render(w)` for each `w` of
/// `workers`, and assert every value equals the serial one (the panic names
/// the worker count). Returns the serial value, for the caller's own
/// assertions. A float comparison that must be exact returns [`bits`] from
/// `render`.
pub fn agree_across_workers<T: PartialEq + std::fmt::Debug>(
    workers: impl IntoIterator<Item = usize>,
    mut render: impl FnMut(usize) -> T,
) -> T {
    let serial = render(1);
    for w in workers {
        assert_eq!(
            render(w),
            serial,
            "{w} workers differ from the serial render"
        );
    }
    serial
}

/// A deterministic main input signal.
pub fn input_signal<F: Float>(start: u64, frames: usize) -> Vec<F> {
    (0..frames)
        .map(|i| {
            let f = start + i as u64;
            // Silent stretches, so the skip path is exercised.
            if (f / 50).is_multiple_of(3) {
                F::ZERO
            } else {
                F::from_f32(((f % 17) as f32 - 8.0) * 0.0625)
            }
        })
        .collect()
}

/// Bits, so a comparison is exact (and NaN-safe).
pub fn bits<F: Word>(v: &[Vec<F>]) -> Vec<Vec<F::Bits>> {
    v.iter()
        .map(|c| c.iter().map(|&x| x.to_word()).collect())
        .collect()
}

/// [`bits`] of one channel.
pub fn bits_of<F: Word>(v: &[F]) -> Vec<F::Bits> {
    v.iter().map(|&x| x.to_word()).collect()
}

/// A float and the integer holding its bits.
pub trait Word: Float {
    type Bits: Copy + Eq + std::fmt::Debug;
    fn to_word(self) -> Self::Bits;
}

impl Word for f32 {
    type Bits = u32;
    fn to_word(self) -> u32 {
        self.to_bits()
    }
}

impl Word for f64 {
    type Bits = u64;
    fn to_word(self) -> u64 {
        self.to_bits()
    }
}

/// A pool of `participants - 1` helper threads over `tutti_rt::JobGate`,
/// for tests: helpers spin (yielding) for work and never sleep, so every
/// job gets its helpers promptly and the parallel paths are exercised hard.
/// `WorkerPool` is the production pool; this one never parks.
pub struct TestPool {
    gate: Arc<tutti_rt::JobGate>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    helpers: Vec<std::thread::JoinHandle<()>>,
}

impl TestPool {
    pub fn new(participants: usize) -> Arc<Self> {
        Self::build_with(participants, crate::BlockWaits::DEFAULT)
    }

    /// A pool whose gate holds `waits`, which it reports to the executor.
    pub fn with_waits(participants: usize, waits: crate::BlockWaits) -> Arc<Self> {
        Self::build_with(participants, waits)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test support: the job gate's helpers race the test's own thread"
    )]
    fn build_with(participants: usize, waits: crate::BlockWaits) -> Arc<Self> {
        let gate = Arc::new(tutti_rt::JobGate::with_waits(participants, waits));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let helpers = (1..participants)
            .map(|_| {
                let (gate, stop) = (Arc::clone(&gate), Arc::clone(&stop));
                std::thread::spawn(move || {
                    let mut served = 0;
                    while !stop.load(Ordering::Relaxed) {
                        if !gate.help(&mut served) {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        Arc::new(Self {
            gate,
            stop,
            helpers,
        })
    }
}

impl crate::Pool for TestPool {
    fn participants(&self) -> usize {
        self.gate.participants()
    }

    fn run(&self, _wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.gate.run(job, || {});
    }

    fn waits(&self) -> crate::BlockWaits {
        self.gate.waits()
    }
}

impl Drop for TestPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.helpers.drain(..) {
            let _ = h.join();
        }
    }
}

/// A shared [`TestPool`] per participant count, for a whole test binary.
pub fn test_pool(participants: usize) -> Arc<dyn crate::Pool> {
    use std::sync::OnceLock;
    use tutti_rt::Mutex;
    static POOLS: OnceLock<Mutex<BTreeMap<usize, Arc<TestPool>>>> = OnceLock::new();
    let pools = POOLS.get_or_init(|| Mutex::new(BTreeMap::new()));

    (Arc::clone(
        pools
            .lock()
            .entry(participants)
            .or_insert_with(|| TestPool::new(participants)),
    )) as _
}

/// The worker counts the parallel suites cover: two, three (odd, so a
/// block's tasks rarely split evenly), four and eight (more participants
/// than a small graph has tasks).
pub const WORKERS: [usize; 4] = [2, 3, 4, 8];

/// A source that is `1.0` for its first `len` frames, then silent for good
/// (and says so, `Status::Silent`).
pub struct Burst {
    pub len: usize,
    pub at: usize,
}

impl Burst {
    pub fn new(len: usize) -> Self {
        Self { len, at: 0 }
    }
}

impl Node for Burst {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.output(0);
        let mut loud = false;
        for o in out.iter_mut() {
            *o = if self.at < self.len { 1.0 } else { 0.0 };
            loud |= *o != 0.0;
            self.at += 1;
        }
        if loud {
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

/// What a [`RingingNode`] reports each block.
pub const NO_REPORT: u64 = u64::MAX;
/// A [`RingingNode`] report of `Tail::Unbounded`.
pub const REPORT_UNBOUNDED: u64 = u64::MAX - 1;
/// A [`RingingNode`] report of `Tail::Unknown`.
pub const REPORT_UNKNOWN: u64 = u64::MAX - 2;

/// Passes its input and adds `0.25` for `ring - 1` frames after the last
/// non-silent input frame (that frame itself is the first of the `ring`), so
/// its last non-zero output frame is `ring - 1` past the input's last. It
/// rings `ring` frames, which is the honest report. Declares `ceiling` and
/// reports what `report` holds: a frame count, or one of the `REPORT_*`
/// words.
pub struct RingingNode {
    pub ceiling: Tail,
    pub ring: usize,
    pub report: Arc<std::sync::atomic::AtomicU64>,
    pub calls: Arc<AtomicUsize>,
    pub latency: Samples,
    /// Audio channels in and out; every channel rings together.
    pub width: usize,
    /// Declares an (unused) event output port: the general form.
    pub event_out: bool,
    left: usize,
}

impl RingingNode {
    pub fn new(ceiling: Tail, ring: usize, report: u64) -> Self {
        Self {
            ceiling,
            ring,
            report: Arc::new(std::sync::atomic::AtomicU64::new(report)),
            calls: Arc::new(AtomicUsize::new(0)),
            latency: Samples::ZERO,
            width: 1,
            event_out: false,
            left: 0,
        }
    }

    /// This node, declaring `latency`.
    pub fn latency(mut self, latency: Samples) -> Self {
        self.latency = latency;
        self
    }

    /// This node, `width` channels wide (2: the `Direct` form).
    pub fn wide(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// This node, with an event output port (the general form).
    pub fn with_event_out(mut self) -> Self {
        self.event_out = true;
        self
    }
}

impl Node for RingingNode {
    fn shape(&self) -> Shape {
        let ch = ChannelLayout::try_from(self.width).expect("a width");
        let shape = Shape::through_unassigned(ch)
            .latency(Latency::new(self.latency))
            .tail(self.ceiling);
        if self.event_out {
            shape.events(0, 1)
        } else {
            shape
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut loud = false;
        {
            let (ins, mut outs) = io.split();
            for i in 0..ins.get(0).len() {
                if (0..self.width).any(|c| ins.get(c)[i] != 0.0) {
                    self.left = self.ring;
                }
                let ringing = if self.left > 0 {
                    self.left -= 1;
                    0.25
                } else {
                    0.0
                };
                for c in 0..self.width {
                    let o = ins.get(c)[i] + ringing;
                    outs.get(c)[i] = o;
                    loud |= o != 0.0;
                }
            }
        }
        match self.report.load(Ordering::Relaxed) {
            NO_REPORT => {}
            REPORT_UNBOUNDED => io.report_tail(Tail::Unbounded),
            REPORT_UNKNOWN => io.report_tail(Tail::Unknown),
            0 => io.report_tail(Tail::None),
            n => io.report_tail(Tail::Finite(Samples::new(n as usize))),
        }
        if loud {
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {
        self.left = 0;
    }
}
