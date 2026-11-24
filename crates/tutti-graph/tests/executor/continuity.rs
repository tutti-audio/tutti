//! What carries across a recompile, and what a rate change resets, checked
//! against figures written out in the test and not against the executor's own
//! keeping.
//!
//! - **The per-key model.** A random script of edits over event delays, audio
//!   delays and event and audio feedback (add, remove, resize) is committed
//!   mid-run on a graph whose units were created in the reverse of their key
//!   order. One ideal ring or FIFO per key (`Key`, never an index) says what
//!   every event and every sample must be: a delay of `n` frames moves what
//!   it carries `n` frames, a retune keeps the most recent `min(old, new)`
//!   inputs of an audio ring and every queued event of a FIFO (an event past
//!   due waits for the next block), and a key that vanishes flushes its
//!   queue to the sink at its next block, spaced as it was, earliest first.
//!   Events sent equal events delivered, flushed and counted dropped, with
//!   what is still in flight.
//! - **Flush order.** Two FIFOs vanishing in one commit into one sink reach
//!   it in key order at offset 0, whatever order they were made in.
//! - **A main output change with delay compensation** continues the old
//!   output's history in the bank channels.
//! - **A rate change** resets the feedback ring and flushes the queued
//!   events, and the edge weight, tap stream and meter run on. (That a PDC
//!   ring restarts silent is `topology::reprepare`'s.)
//!
//! Each runs on one participant and on a pool.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_graph::testing::{self as common, Harness, Kind, TestNode, input_signal, prepare};
use tutti_graph::{
    Cx, Edge, EdgeGain, EdgeId, Editor, EventEdge, EventInputPort, EventKind, EventOutputPort,
    Executor, Fade, FadeLaw, FeedbackFrom, InEvents, InputPort, Io, MeterId, MeterSpec, Node,
    NodeKey, OutputPort, PortSink, PortSource, Prepare, Shape, Status, TapAlign, TapId, TapOptions,
    TransportPosition, Unforkable,
};
use tutti_ump::Ump;
use tutti_units::{Frame, Gain, Latency, SampleRate, Samples, Tail};

use crate::generator::{MAX_BLOCK, Rng, schedule};
use crate::invariants::workers;

/// `(word 0, word 1, delivery frame)` of every event the recorder received.
type Inbox = tutti_fixtures::Record<(u32, u32, Frame)>;

/// Logs every event it receives with the frame it is delivered on.
struct Recorder(Inbox);

impl Recorder {
    const SHAPE: Shape = Shape::EMPTY.events(1, 0).tail(Tail::Unknown);
    const IN: InEvents = Self::SHAPE.event_input(0).expect("declared");
}

impl Node for Recorder {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        for e in io.events(Self::IN) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((w[0], w[1], cx.env.frame_at(e.offset)));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn node(key: NodeKey, kind: Kind) -> (NodeKey, Unforkable<TestNode>) {
    (key, Unforkable(TestNode::new(kind)))
}

/// An emitter that fires on the frames `f` with `f % 16 == source`, its event
/// carrying `f` in word 0: the frame names the event, and `f % 16` its source.
fn emitter(source: u64) -> Kind {
    Kind::Emitter {
        period: 16,
        phase: (16 - source) % 16,
    }
}

fn input_f64(frames: usize) -> Vec<f64> {
    input_signal::<f64>(0, frames)
}

fn src(node: NodeKey) -> EventOutputPort {
    EventOutputPort::new(node, 0)
}

fn audio(node: NodeKey) -> PortSource {
    PortSource::Node(OutputPort::new(node, 0))
}

/// Commits, lets the executor take it and the editor collect what comes back.
fn land(harness: &mut Harness) {
    harness.editor.commit().expect("commits");
    harness.exec.apply_pending();
    harness.editor.collect();
}

// ---------------------------------------------------------------------------
// The per-key model
// ---------------------------------------------------------------------------

const EA: NodeKey = NodeKey(10);
const EB: NodeKey = NodeKey(11);
const EC: NodeKey = NodeKey(12);
const EL: NodeKey = NodeKey(13);
const REC: NodeKey = NodeKey(14);
const LAG: NodeKey = NodeKey(15);
const GAIN: NodeKey = NodeKey(16);
const SUM: NodeKey = NodeKey(17);

/// The frames an `EventLag` and a `Lag` really delay by, whatever their
/// declared latency becomes.
const REAL_EVENT_LAG: u64 = 20;
const REAL_LAG: u64 = 24;
/// The gain on the audio feedback's source.
const FEEDBACK_GAIN: f64 = 0.5;
/// The frames a case renders.
const FRAMES: usize = 900;

/// The wiring a script edits: what exists, and each delay's declared length.
///
/// ```text
/// EA ────────────── (direct, a FIFO of `event_latency`) ─┐
/// EB ── (feedback, a FIFO of its delay) ─────────────────┼─ REC
/// EC ── EL (declares `event_latency`, really delays 20) ─┘
///
/// input ─ LAG (declares `audio_latency`, really delays 24) ─ SUM.0 ─ out
/// input ─ (a ring of `audio_latency`) ──────────────────── SUM.1
/// input ─ GAIN 0.5 ─ (feedback, a ring of its delay) ───── SUM.2
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
struct Config {
    emitter: bool,
    direct: bool,
    event_latency: Samples,
    event_feedback: Option<Samples>,
    ring: bool,
    audio_latency: Samples,
    audio_feedback: Option<Samples>,
}

impl Config {
    fn start() -> Self {
        Self {
            emitter: true,
            direct: true,
            event_latency: Samples::new(REAL_EVENT_LAG as usize),
            event_feedback: Some(Samples::new(MAX_BLOCK)),
            ring: true,
            audio_latency: Samples::new(REAL_LAG as usize),
            audio_feedback: Some(Samples::new(MAX_BLOCK)),
        }
    }
}

/// A delay's state is keyed by what it joins, never by where the plan puts
/// it: a feedback delay is part of its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    /// The event delay from `EA` into `REC`.
    Direct,
    EventFeedback(Samples),
    /// The audio delay from the input into `SUM.1`.
    Ring,
    AudioFeedback(Samples),
}

impl Config {
    /// The delays this wiring has, each with its length.
    fn keys(&self) -> BTreeMap<Key, Samples> {
        let mut keys = BTreeMap::new();
        if self.emitter && self.direct {
            keys.insert(Key::Direct, self.event_latency);
        }
        if let Some(d) = self.event_feedback {
            keys.insert(Key::EventFeedback(d), d);
        }
        if self.ring {
            keys.insert(Key::Ring, self.audio_latency);
        }
        if let Some(d) = self.audio_feedback {
            keys.insert(Key::AudioFeedback(d), d);
        }
        keys
    }
}

/// One edit of a script.
#[derive(Clone, Copy, Debug)]
enum Op {
    EventLatency(Samples),
    AudioLatency(Samples),
    Direct(bool),
    Emitter(bool),
    Ring(bool),
    EventFeedback(Option<Samples>),
    AudioFeedback(Option<Samples>),
}

impl Config {
    fn after(mut self, op: Op) -> Self {
        match op {
            Op::EventLatency(l) => self.event_latency = l,
            Op::AudioLatency(l) => self.audio_latency = l,
            Op::Direct(on) => self.direct = on,
            // Removing the node takes its edge with it.
            Op::Emitter(on) => {
                self.emitter = on;
                self.direct = false;
            }
            Op::Ring(on) => self.ring = on,
            Op::EventFeedback(d) => self.event_feedback = d,
            Op::AudioFeedback(d) => self.audio_feedback = d,
        }
        self
    }
}

const DELAYS: [Option<usize>; 4] = [
    None,
    Some(MAX_BLOCK),
    Some(MAX_BLOCK + 17),
    Some(MAX_BLOCK + 34),
];

fn random_op(rng: &mut Rng, cfg: &Config) -> Op {
    let latency = |rng: &mut Rng| Samples::new(1 + rng.below(60) as usize);
    let delay = |rng: &mut Rng| DELAYS[rng.below(4) as usize].map(Samples::new);
    match rng.below(7) {
        0 => Op::EventLatency(latency(rng)),
        1 => Op::AudioLatency(latency(rng)),
        2 if cfg.emitter => Op::Direct(!cfg.direct),
        3 => Op::Emitter(!cfg.emitter),
        4 => Op::Ring(!cfg.ring),
        5 => Op::EventFeedback(delay(rng)),
        6 => Op::AudioFeedback(delay(rng)),
        _ => Op::Ring(!cfg.ring),
    }
}

/// The ideal event delay of one key: the frames its events were emitted at,
/// oldest first. An event is due `len` frames after it was emitted.
struct Fifo {
    len: Samples,
    queued: Vec<u64>,
}

/// The ideal audio delay of one key: it holds the inputs from `kept_from` on.
struct Ring {
    len: Samples,
    kept_from: u64,
}

/// What every delay of the graph must hold and every event must be.
#[derive(Default)]
struct Model {
    fifos: BTreeMap<Key, Fifo>,
    rings: BTreeMap<Key, Ring>,
    /// Every event the sink must receive: `(word 0, word 1, frame)`.
    want: Vec<(u32, u32, Frame)>,
    /// Events handed to a delay.
    sent: usize,
    /// How often a script made each case the model covers.
    seen: Seen,
}

/// What a script exercised: events flushed by a vanished FIFO, FIFOs retuned
/// while holding events, audio rings retuned or recreated.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    flushed: usize,
    retuned_holding: usize,
    ring_retunes: usize,
    ring_vanished: usize,
}

impl Model {
    /// The delays of `cfg` as a render that starts there has them.
    fn start(cfg: &Config) -> Self {
        let mut model = Self::default();
        model.reconcile(&Config::start().none(), cfg, 0, 1);
        model
    }

    /// What `old` becomes at frame `at`, in a commit whose first block is `n`
    /// frames: a key that is gone flushes (events) or is forgotten (audio), a
    /// key that is new is empty, a key that stays takes its new length.
    fn reconcile(&mut self, old: &Config, new: &Config, at: u64, n: u64) {
        let now = new.keys();
        for key in old.keys().keys().filter(|k| !now.contains_key(k)) {
            self.seen.ring_vanished += usize::from(self.rings.remove(key).is_some());
            if let Some(fifo) = self.fifos.remove(key) {
                // Spacing kept, the earliest at offset 0, the last at most on
                // the block's last frame.
                let first = fifo.queued.first().map(|e| e + fifo.len.get() as u64);
                self.seen.flushed += fifo.queued.len();
                for e in fifo.queued {
                    let spacing = e + fifo.len.get() as u64 - first.unwrap_or_default();
                    self.want
                        .push((e as u32, 0, Frame::new(at + spacing.min(n - 1))));
                }
            }
        }
        for (&key, &len) in &now {
            match key {
                Key::Direct | Key::EventFeedback(_) => {
                    self.fifos
                        .entry(key)
                        .and_modify(|f| {
                            self.seen.retuned_holding +=
                                usize::from(f.len != len && !f.queued.is_empty());
                            f.len = len;
                        })
                        .or_insert(Fifo {
                            len,
                            queued: Vec::new(),
                        });
                }
                Key::Ring | Key::AudioFeedback(_) => {
                    self.rings
                        .entry(key)
                        .and_modify(|r| {
                            self.seen.ring_retunes += usize::from(r.len != len);
                            r.kept_from = r
                                .kept_from
                                .max(at.saturating_sub(r.len.get().min(len.get()) as u64));
                            r.len = len;
                        })
                        .or_insert(Ring { len, kept_from: at });
                }
            }
        }
    }

    /// The block of `n` frames from `at`: what its sources emit enters their
    /// delays, and what falls due leaves them (an event already past due, on
    /// the block's first frame).
    fn advance(&mut self, cfg: &Config, at: u64, n: u64) {
        for f in at..at + n {
            match f % 16 {
                1 if cfg.emitter => self.send(&[Key::Direct], f),
                2 => {
                    let keys: Vec<Key> = self
                        .fifos
                        .keys()
                        .copied()
                        .filter(|k| matches!(k, Key::EventFeedback(_)))
                        .collect();
                    self.send(&keys, f);
                }
                // Through `EC` and `EL`: no delay of the graph's, a fixed one
                // of the node's own.
                3 => self
                    .want
                    .push((f as u32, 1, Frame::new(f + REAL_EVENT_LAG))),
                _ => {}
            }
        }
        for fifo in self.fifos.values_mut() {
            let len = fifo.len.get() as u64;
            let gone = fifo.queued.partition_point(|e| e + len < at + n);
            for e in fifo.queued.drain(..gone) {
                self.want.push((e as u32, 0, Frame::new((e + len).max(at))));
            }
        }
    }

    fn send(&mut self, keys: &[Key], frame: u64) {
        for key in keys {
            if let Some(fifo) = self.fifos.get_mut(key) {
                fifo.queued.push(frame);
                self.sent += 1;
            }
        }
    }

    /// The output at frame `t`, from the input `x`.
    fn output(&self, t: u64, x: &[f64]) -> f64 {
        let at = |frame: i64| usize::try_from(frame).map_or(0.0, |i| x[i]);
        let held = |ring: &Ring| {
            let from = t as i64 - ring.len.get() as i64;
            if from >= ring.kept_from as i64 {
                at(from)
            } else {
                0.0
            }
        };
        let mut y = at(t as i64 - REAL_LAG as i64);
        for (key, ring) in &self.rings {
            y += match key {
                Key::AudioFeedback(_) => FEEDBACK_GAIN * held(ring),
                _ => held(ring),
            };
        }
        y
    }

    fn in_flight(&self) -> usize {
        self.fifos.values().map(|f| f.queued.len()).sum()
    }
}

impl Config {
    /// No wiring at all, for the model to start from.
    fn none(self) -> Self {
        Self {
            emitter: false,
            direct: false,
            event_feedback: None,
            ring: false,
            audio_feedback: None,
            ..self
        }
    }
}

/// The graph at `Config::start`, its units created in the reverse of key
/// order.
fn rig(workers: usize, inbox: &Inbox) -> Harness {
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    let ed = &mut harness.editor;
    let kinds = [
        (SUM, Kind::Sum { inputs: 3 }),
        (
            GAIN,
            Kind::Gain {
                gain: FEEDBACK_GAIN as f32,
                width: 1,
            },
        ),
        (
            LAG,
            Kind::Lag {
                latency: REAL_LAG as usize,
            },
        ),
    ];
    for (key, kind) in kinds {
        let (key, unit) = node(key, kind);
        ed.insert(key, "unit", unit);
    }
    ed.insert(REC, "recorder", Unforkable(Recorder(inbox.clone())));
    let rest = [
        (
            EL,
            Kind::EventLag {
                latency: REAL_EVENT_LAG as usize,
            },
        ),
        (EC, emitter(3)),
        (EB, emitter(2)),
        (EA, emitter(1)),
    ];
    for (key, kind) in rest {
        let (key, unit) = node(key, kind);
        ed.insert(key, "unit", unit);
    }
    let spec = ed.spec_mut();
    spec.topology.inputs = ChannelLayout::MONO;
    let input = vec![Edge::Direct(PortSource::MainInput(0))];
    let edges = &mut spec.topology.edges;
    edges.insert(InputPort::new(LAG, 0), input.clone());
    edges.insert(InputPort::new(GAIN, 0), input.clone());
    edges.insert(InputPort::new(SUM, 0), vec![Edge::Direct(audio(LAG))]);
    edges.insert(InputPort::new(SUM, 1), input);
    edges.insert(
        InputPort::new(SUM, 2),
        vec![Edge::Feedback(FeedbackFrom::new(
            OutputPort::new(GAIN, 0),
            Samples::new(MAX_BLOCK),
        ))],
    );
    spec.topology.set_outputs(vec![audio(SUM)]);
    spec.connect_events(EventInputPort::new(EL, 0), EventEdge::Direct(src(EC)));
    let at = EventInputPort::new(REC, 0);
    spec.connect_events(at, EventEdge::Direct(src(EA)));
    spec.connect_events(at, EventEdge::Direct(src(EL)));
    spec.connect_events(at, EventEdge::feedback(src(EB), Samples::new(MAX_BLOCK)));
    land(&mut harness);
    harness.outputs = 1;
    harness.inputs = 1;
    harness
}

/// `op` on the editor's spec and units; the commit is the caller's.
fn apply(harness: &mut Harness, op: Op) {
    let ed = &mut harness.editor;
    let sink = EventInputPort::new(REC, 0);
    let latency = |frames: Samples| Latency::new(frames);
    match op {
        Op::EventLatency(l) => ed.set_latency(EL, latency(l)).expect("sets"),
        Op::AudioLatency(l) => ed.set_latency(LAG, latency(l)).expect("sets"),
        Op::Direct(true) => ed
            .spec_mut()
            .connect_events(sink, EventEdge::Direct(src(EA))),
        Op::Direct(false) => {
            ed.spec_mut().disconnect_events(sink, src(EA));
        }
        Op::Emitter(true) => {
            let (key, unit) = node(EA, emitter(1));
            ed.insert(key, "unit", unit);
        }
        Op::Emitter(false) => ed.remove(EA),
        Op::Ring(on) => {
            let at = InputPort::new(SUM, 1);
            let edges = &mut ed.spec_mut().topology.edges;
            if on {
                edges.insert(at, vec![Edge::Direct(PortSource::MainInput(0))]);
            } else {
                edges.remove(&at);
            }
        }
        Op::EventFeedback(d) => {
            let spec = ed.spec_mut();
            spec.disconnect_events(sink, src(EB));
            if let Some(d) = d {
                spec.connect_events(sink, EventEdge::feedback(src(EB), d));
            }
        }
        Op::AudioFeedback(d) => {
            let at = InputPort::new(SUM, 2);
            let edges = &mut ed.spec_mut().topology.edges;
            match d {
                Some(d) => edges.insert(
                    at,
                    vec![Edge::Feedback(FeedbackFrom::new(
                        OutputPort::new(GAIN, 0),
                        d,
                    ))],
                ),
                None => edges.remove(&at),
            };
        }
    }
}

/// One scripted run: every sample of the output and every event, against the
/// model, with conservation.
fn per_key_model(seed: u64, which: usize, workers: usize) -> Seen {
    let inbox: Inbox = tutti_fixtures::Record::new();
    let mut harness = rig(workers, &inbox);
    let mut rng = Rng::new(seed ^ 0x9E4);
    let blocks = schedule(which, seed, FRAMES);
    let edits: BTreeSet<usize> = (0..1 + rng.below(4))
        .map(|_| 1 + rng.below(blocks.len() as u64 - 1) as usize)
        .collect();
    let (mut cfg, mut at) = (Config::start(), 0u64);
    let mut model = Model::start(&cfg);
    let x = input_f64(blocks.iter().sum());
    for (i, &n) in blocks.iter().enumerate() {
        if edits.contains(&i) {
            let mut next = cfg;
            for _ in 0..1 + rng.below(2) {
                let op = random_op(&mut rng, &next);
                apply(&mut harness, op);
                next = next.after(op);
            }
            land(&mut harness);
            model.reconcile(&cfg, &next, at, n as u64);
            cfg = next;
        }
        model.advance(&cfg, at, n as u64);
        let out = harness.block(n, &input_signal::<f32>(at, n));
        for (j, &y) in out[0].iter().enumerate() {
            let frame = at + j as u64;
            assert_eq!(
                f64::from(y),
                model.output(frame, &x),
                "seed {seed}, workers {workers}, sample {frame}, after {cfg:?}"
            );
        }
        at += n as u64;
    }
    let end = Frame::new(at);
    let mut want: Vec<_> = model
        .want
        .iter()
        .copied()
        .filter(|&(_, _, f)| f < end)
        .collect();
    let mut got = inbox.lock().unwrap().to_vec();
    want.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, want, "seed {seed}, workers {workers}: events by frame");
    let delivered = got.iter().filter(|e| e.1 == 0).count();
    assert_eq!(
        model.sent,
        delivered + model.in_flight() + harness.exec.dropped_event_count() as usize,
        "seed {seed}, workers {workers}: sent = delivered + flushed + dropped + in flight"
    );
    assert_eq!(
        harness.exec.dropped_event_count(),
        0,
        "the model drops none"
    );
    model.seen
}

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(128), ..ProptestConfig::default() })]

    /// A random script of edits to event and audio delays and feedback, made
    /// mid-run, leaves every sample and every event where one ideal ring or
    /// FIFO per key puts it, on one participant and on a pool.
    #[test]
    fn a_script_of_delay_edits_keeps_what_each_key_holds(
        seed in any::<u64>(),
        which in 0usize..5,
        workers in workers(),
    ) {
        per_key_model(seed, which, 1);
        per_key_model(seed, which, workers);
    }
}

/// The scripts are not vacuous: over a fixed range of seeds they flush events
/// out of vanished FIFOs, retune FIFOs that hold events, and retune and drop
/// audio rings.
#[test]
fn the_scripts_flush_retune_and_drop_delays_that_hold_signal() {
    let mut seen = Seen::default();
    for seed in 0..128u64 {
        let one = per_key_model(seed, (seed % 5) as usize, 1);
        seen.flushed += one.flushed;
        seen.retuned_holding += one.retuned_holding;
        seen.ring_retunes += one.ring_retunes;
        seen.ring_vanished += one.ring_vanished;
    }
    assert!(seen.flushed > 100, "{seen:?}");
    assert!(seen.retuned_holding > 25, "{seen:?}");
    assert!(seen.ring_retunes > 25, "{seen:?}");
    assert!(seen.ring_vanished > 50, "{seen:?}");
}

// ---------------------------------------------------------------------------
// Flush order
// ---------------------------------------------------------------------------

/// Two event delays that vanish in one commit into one sink flush to it in
/// key order, each earliest event at offset 0 and each spaced as it was, though
/// the units and the edges were made in the reverse of key order.
fn flush_order(workers: usize) {
    const FIRST: NodeKey = NodeKey(20);
    const SECOND: NodeKey = NodeKey(21);
    const BLOCK: usize = 128;
    const LEN: usize = 40;
    let inbox: Inbox = tutti_fixtures::Record::new();
    let mut harness = Harness::with_workers(BLOCK, workers);
    let ed = &mut harness.editor;
    for (key, kind) in [
        (SECOND, emitter(2)),
        (FIRST, emitter(1)),
        (EC, emitter(3)),
        (EL, Kind::EventLag { latency: LEN }),
    ] {
        let (key, unit) = node(key, kind);
        ed.insert(key, "unit", unit);
    }
    ed.insert(REC, "recorder", Unforkable(Recorder(inbox.clone())));
    let spec = ed.spec_mut();
    spec.connect_events(EventInputPort::new(EL, 0), EventEdge::Direct(src(EC)));
    let at = EventInputPort::new(REC, 0);
    for from in [SECOND, FIRST, EL] {
        spec.connect_events(at, EventEdge::Direct(src(from)));
    }
    land(&mut harness);
    let run = |harness: &mut Harness| {
        harness
            .exec
            .process(BLOCK, &TransportPosition::default(), &[], &mut []);
    };
    for _ in 0..3 {
        run(&mut harness);
    }
    let seen = inbox.lock().unwrap().len();
    let spec = harness.editor.spec_mut();
    spec.disconnect_events(at, src(SECOND));
    spec.disconnect_events(at, src(FIRST));
    land(&mut harness);
    run(&mut harness);
    let got: Vec<_> = inbox.lock().unwrap().to_vec()[seen..].to_vec();
    let from = 3 * BLOCK as u64;
    // What each FIFO held when the commit landed: the events emitted in the
    // `LEN` frames before it, due on or after it, in flush order of key.
    let queued = |source: u64| (from - LEN as u64..from).filter(move |f| f % 16 == source);
    let flushed = |source: u64| -> Vec<(u64, u64)> {
        let first = queued(source).next().expect("queued") + LEN as u64;
        queued(source)
            .map(|e| (e + LEN as u64 - first, e))
            .collect()
    };
    let mut want: Vec<(u64, u64, u64)> = Vec::new();
    for (rank, source) in [1, 2].into_iter().enumerate() {
        want.extend(
            flushed(source)
                .into_iter()
                .map(|(offset, e)| (offset, rank as u64, e)),
        );
    }
    // By offset, ties in key order.
    want.sort_by_key(|&(offset, rank, _)| (offset, rank));
    let want: Vec<(u32, Frame)> = want
        .into_iter()
        .map(|(offset, _, e)| (e as u32, Frame::new(from + offset)))
        .collect();
    let direct: Vec<(u32, Frame)> = got
        .iter()
        .filter(|e| e.1 == 0 && e.2 >= Frame::new(from))
        .map(|&(tag, _, frame)| (tag, frame))
        .collect();
    assert!(
        want.len() >= 4 && want[0].1 == want[1].1,
        "a tie at offset 0: {want:?}"
    );
    assert_eq!(direct, want);
}

#[test]
fn two_vanishing_fifos_flush_in_key_order_at_offset_zero_with_their_spacing() {
    flush_order(1);
    for workers in common::WORKERS {
        flush_order(workers);
    }
}

// ---------------------------------------------------------------------------
// A main output change with delay compensation
// ---------------------------------------------------------------------------

/// The old main output is `[input, LAG.out]` with `LAG` declaring 48 frames,
/// so the input channel is held 48 frames behind by a delay compensation ring.
/// A commit declares a new main output that has no latency while impulses are
/// in that ring and more follow: the old main output's bank channels go on
/// as `x[t - 48]`, the history that was in flight and everything after,
/// through a second commit that leaves the main output alone.
fn bank_continues_the_old_history(workers: usize) {
    const BLOCK: usize = 64;
    const LATENCY: usize = 48;
    const LAG_NODE: NodeKey = NodeKey(1);
    const NEW: NodeKey = NodeKey(2);
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    if workers > 1 {
        exec.set_pool(Some(common::test_pool(workers)));
    }
    ed.insert(
        NEW,
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.25,
            width: 1,
        })),
    );
    ed.insert(
        LAG_NODE,
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: LATENCY })),
    );
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.spec_mut().topology.edges.insert(
        InputPort::new(LAG_NODE, 0),
        vec![Edge::Direct(PortSource::MainInput(0))],
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::MainInput(0), audio(LAG_NODE)]);
    ed.commit().expect("commits");
    // Impulses at 162 (in the ring when the commit lands at 192), 197 and 282.
    let impulses = [(162usize, 1.0f32), (197, 0.5), (282, 0.25)];
    let x = |t: usize| {
        impulses
            .iter()
            .find(|&&(at, _)| at == t)
            .map_or(0.0, |&(_, v)| v)
    };
    let delayed = |t: usize| t.checked_sub(LATENCY).map_or(0.0, x);
    let step = |ed: &mut Editor, exec: &mut Executor, at: usize| -> Vec<Vec<f32>> {
        exec.apply_pending();
        let rendered = exec.plan().expect("a plan").rendered_outputs();
        let input: Vec<f32> = (at..at + BLOCK).map(x).collect();
        let mut bufs = vec![vec![0.0f32; BLOCK]; rendered];
        let mut outs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
        exec.process(
            BLOCK,
            &TransportPosition::default(),
            &[&input[..]],
            &mut outs,
        );
        ed.collect();
        bufs
    };
    let check = |bufs: &[Vec<f32>], at: usize, channels: &[usize]| {
        for (c, &channel) in channels.iter().enumerate() {
            for (i, &y) in bufs[channel].iter().enumerate() {
                assert_eq!(y, delayed(at + i), "channel {c} at frame {}", at + i);
            }
        }
    };
    for b in 0..3 {
        check(&step(&mut ed, &mut exec, b * BLOCK), b * BLOCK, &[0, 1]);
    }
    ed.set_main_output_fade(Some(
        Fade::new(Samples::new(8 * BLOCK)).law(FadeLaw::EqualAmplitude),
    ));
    let id = ed.spec().main_output_id.next();
    ed.spec_mut().main_output_id = id;
    ed.spec_mut().topology.set_outputs(vec![audio(NEW)]);
    ed.commit().expect("declares the new main output");
    for b in 3..5 {
        let bufs = step(&mut ed, &mut exec, b * BLOCK);
        assert_eq!(
            bufs.len(),
            3,
            "the new main output, then the old one's two channels"
        );
        assert!(
            bufs[0].iter().all(|&y| y == 0.25),
            "the new main output sounds"
        );
        check(&bufs, b * BLOCK, &[1, 2]);
    }
    // A commit that leaves the main output alone carries the bank on.
    ed.insert(
        NodeKey(9),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        })),
    );
    ed.commit().expect("commits");
    for b in 5..7 {
        let bufs = step(&mut ed, &mut exec, b * BLOCK);
        assert_eq!(bufs.len(), 3);
        check(&bufs, b * BLOCK, &[1, 2]);
    }
}

#[test]
fn the_bank_continues_the_old_outputs_delay_compensation_history() {
    bank_continues_the_old_history(1);
    for workers in common::WORKERS {
        bank_continues_the_old_history(workers);
    }
}

// ---------------------------------------------------------------------------
// A rate change
// ---------------------------------------------------------------------------

/// A rate change at frame `T` (48 kHz to 96 kHz, so the clock doubles):
///
/// - the audio feedback ring (128 frames) reads zero for its 128 frames;
/// - the 40-frame event delay's queue flushes at offset 0 of the first block
///   after, spaced as it was (events 4 frames apart), and the events after
///   keep their 40-frame delay on the new clock;
/// - the edge weight `0.5` (a lane's) is in force on the first frame after,
///   its node's ring and the rest carried;
/// - a raw tap on a node reads one unbroken stream across the change, and
///   its frames follow the executor's clock;
/// - a meter reads the first block after the change, as it read the ones
///   before.
fn a_rate_change(workers: usize) {
    const BLOCK: usize = 64;
    const GAIN_NODE: NodeKey = NodeKey(32);
    const LAG_NODE: NodeKey = NodeKey(33);
    const SUM_NODE: NodeKey = NodeKey(34);
    const EMIT: NodeKey = NodeKey(35);
    const EVENT_LAG: NodeKey = NodeKey(36);
    const SINK: NodeKey = NodeKey(38);
    const LEN: usize = 40;
    let inbox: Inbox = tutti_fixtures::Record::new();
    let mut harness = Harness::with_workers(BLOCK, workers);
    let ed = &mut harness.editor;
    for (key, kind) in [
        (SUM_NODE, Kind::Sum { inputs: 2 }),
        (
            GAIN_NODE,
            Kind::Gain {
                gain: 0.5,
                width: 1,
            },
        ),
        (LAG_NODE, Kind::Lag { latency: 30 }),
        (
            EMIT,
            Kind::Emitter {
                period: 4,
                phase: 0,
            },
        ),
        (EVENT_LAG, Kind::EventLag { latency: LEN }),
    ] {
        let (key, unit) = node(key, kind);
        ed.insert(key, "unit", unit);
    }
    ed.insert(SINK, "recorder", Unforkable(Recorder(inbox.clone())));
    let spec = ed.spec_mut();
    spec.topology.inputs = ChannelLayout::MONO;
    let input = vec![Edge::Direct(PortSource::MainInput(0))];
    let weighted = InputPort::new(SUM_NODE, 0);
    let edges = &mut spec.topology.edges;
    edges.insert(InputPort::new(LAG_NODE, 0), input.clone());
    edges.insert(InputPort::new(GAIN_NODE, 0), input);
    edges.insert(weighted, vec![Edge::Direct(audio(LAG_NODE))]);
    edges.insert(
        InputPort::new(SUM_NODE, 1),
        vec![Edge::Feedback(FeedbackFrom::new(
            OutputPort::new(GAIN_NODE, 0),
            Samples::new(MAX_BLOCK),
        ))],
    );
    spec.topology.set_outputs(vec![audio(SUM_NODE)]);
    let sink = EventInputPort::new(SINK, 0);
    spec.connect_events(sink, EventEdge::Direct(src(EMIT)));
    spec.connect_events(sink, EventEdge::Direct(src(EVENT_LAG)));
    spec.connect_events(
        EventInputPort::new(EVENT_LAG, 0),
        EventEdge::Direct(src(EMIT)),
    );
    let lane = EdgeId {
        sink: PortSink::Port(weighted),
        from: Edge::Direct(audio(LAG_NODE)),
    };
    spec.set_edge_gain(lane, EdgeGain::default());
    harness.start_edge_gain(lane, Gain::new(0.5));
    let mut tap = harness
        .editor
        .open_tap(
            TapId::of(1),
            vec![audio(GAIN_NODE)],
            TapOptions::new(Samples::new(1 << 14)).align(TapAlign::Raw),
        )
        .expect("the source exists");
    let mut meter = harness
        .editor
        .open_meter(MeterId::of(1), MeterSpec::raw(vec![audio(GAIN_NODE)]))
        .expect("the source exists");
    meter.enable();
    land(&mut harness);
    harness.outputs = 1;
    harness.inputs = 1;

    let (before, after) = (6usize, 10usize);
    let total = (before + after) * BLOCK;
    let x = input_f64(total);
    let mut y: Vec<f32> = Vec::new();
    let mut streamed: Vec<f32> = Vec::new();
    let mut drain = |tap: &mut tutti_graph::TapReader| {
        let mut buf = vec![0.0f32; 4096];
        loop {
            let n = tap
                .pop_into(InterleavedMut::new(&mut buf, tap.layout()).unwrap())
                .unwrap()
                .get();
            if n == 0 {
                break;
            }
            streamed.extend_from_slice(&buf[..n]);
        }
    };
    let mut levels = Vec::new();
    let mut frame = 0u64;
    let mut render = |harness: &mut Harness, frames: usize, y: &mut Vec<f32>| {
        let out = harness.block(frames, &input_signal::<f32>(frame, frames));
        frame += frames as u64;
        y.extend(&out[0]);
    };
    for _ in 0..before {
        render(&mut harness, BLOCK, &mut y);
        drain(&mut tap);
        levels.push(meter.read());
    }
    assert_eq!(
        harness.exec.frame(),
        Frame::new(before as u64 * BLOCK as u64)
    );
    // The lane's weight starts moving to 0.25 on the first frame after the
    // change, from the 0.5 it held.
    harness.set_edge_gain(lane, Gain::new(0.25));
    harness
        .editor
        .reprepare(Prepare::new(
            SampleRate::SR_96K,
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("reprepares");
    harness.exec.apply_pending();
    harness.editor.collect();
    harness.exec.apply_pending();
    harness.editor.collect();
    for _ in 0..after {
        render(&mut harness, BLOCK, &mut y);
        drain(&mut tap);
        levels.push(meter.read());
    }
    let change = before * BLOCK;
    let g = |i: usize| 0.5 * x[i];
    // The audio: the lane moves from the 0.5 it held to 0.25 across the first
    // block after the change, as a weight moves across a block; the lag node's
    // ring is carried and the feedback ring reads zero for its 128 frames.
    let weight = |t: usize| match t.checked_sub(change) {
        None => 0.5,
        Some(j) if j < BLOCK => 0.5 + (0.25 - 0.5) * (j + 1) as f64 / BLOCK as f64,
        Some(_) => 0.25,
    };
    for (t, &got) in y.iter().enumerate() {
        let lag = t.checked_sub(30).map_or(0.0, |i| weight(t) * x[i]);
        let ring = t
            .checked_sub(MAX_BLOCK)
            .filter(|&i| t < change || i >= change)
            .map_or(0.0, g);
        assert!(
            (f64::from(got) - (lag + ring)).abs() < 4e-6,
            "output at frame {t}: {got} against {}",
            lag + ring
        );
    }
    // The tap: one stream, every frame of the node's output once.
    let want: Vec<f32> = (0..total).map(|i| g(i) as f32).collect();
    assert_eq!(streamed, want, "the tap reads one unbroken stream");
    assert_eq!(tap.take_lost(), Samples::ZERO);
    assert_eq!(
        tap.next_frame(),
        harness.exec.frame(),
        "the tap's frames follow the executor's clock through the change"
    );
    // The meter: each block's peak and RMS, those after the change too.
    for (b, read) in levels.iter().enumerate() {
        let block: Vec<f64> = (b * BLOCK..(b + 1) * BLOCK).map(g).collect();
        let peak = block.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let rms = (block.iter().map(|v| v * v).sum::<f64>() / BLOCK as f64).sqrt();
        let level = read[0];
        assert_eq!(
            (level.peak.get(), level.rms.get(), level.frames),
            (peak, rms, Samples::new(BLOCK)),
            "meter, block {b}"
        );
    }
    assert!(meter.is_enabled() && !meter.is_closed());
    // The event delay: the queue at the change goes out spaced as it was.
    let clock_after = 2 * change as u64;
    let queued = |first: u64, last: u64| (first..last).filter(|f| f % 4 == 0);
    let mut want_events: Vec<(u32, Frame)> = Vec::new();
    let first_due = queued(change as u64 - LEN as u64, change as u64)
        .next()
        .expect("queued")
        + LEN as u64;
    for e in queued(0, change as u64) {
        let due = e + LEN as u64;
        let frame = if due < change as u64 {
            due
        } else {
            clock_after + (due - first_due)
        };
        want_events.push((e as u32, Frame::new(frame)));
    }
    let end = clock_after + (after * BLOCK) as u64;
    for e in queued(clock_after, end) {
        if e + (LEN as u64) < end {
            want_events.push((e as u32, Frame::new(e + LEN as u64)));
        }
    }
    let mut got: Vec<(u32, Frame)> = inbox
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.1 == 0)
        .map(|&(tag, _, frame)| (tag, frame))
        .collect();
    got.sort_unstable();
    want_events.sort_unstable();
    assert_eq!(got, want_events, "events by frame");
}

#[test]
fn a_rate_change_resets_the_feedback_ring_and_flushes_the_queue_as_spaced_and_the_rest_runs_on() {
    a_rate_change(1);
    for workers in common::WORKERS {
        a_rate_change(workers);
    }
}
