//! Regression gate for `Engine::process` with the engine's own nodes in the
//! graph: timed transport commands, and the metronome, whose meter read is
//! the one non-scalar read on the audio thread.
//!
//! The metronome gate — the click beside a node walking its `Env`'s beat,
//! with timed seeks, tempo and loop changes on top — is
//! `graph_engine_with_env_clock_and_metronome_is_allocation_free`.
//!
//! The gate over a chain of real DSP nodes (EQ, strip, limiter) is
//! `tutti-nodes`' `tests/rt_no_alloc_engine.rs`: running it on the nodes the
//! engine ships means depending on `tutti-nodes`, which depends on this crate,
//! so it lives there rather than create a cycle.
//!
//! What this file deliberately does *not* test: that the audio thread never
//! *frees* a retired non-scalar value (a `MeterMap`, a routing table). That
//! property is carried by the type — `Reader::lend` returns a `Loan`, a
//! `!Send` borrow tied to the reader's lifetime, so there is no owning handle for
//! the callback to be left holding — and by `Publisher`'s reclamation protocol,
//! whose reader side has no path to a destructor. It cannot be pinned by a
//! no-alloc gate here: the hazard is a race, and a sampling schedule cannot
//! exhaust one; a test that tried would pass while asserting a property it
//! structurally could not observe. The race is covered where it can be: the
//! loom model in arc-lend's `tests/loom.rs` (against the shipped code) and miri
//! over arc-lend's stress test, both run by arc-lend's own CI.

use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_engine::Engine;
use tutti_fixtures::midi;
use tutti_music::LoopRange;
use tutti_nodes::{ClickNode, ClickNodeConfig, ClickSettings, MetronomeMode};
use tutti_timeline::{LoopEdit, Transport};
use tutti_units::SampleRate;

/// A stereo source that reads the transport per frame from its `Env`, so
/// the gated loop below exercises `transport_at` and the executor's change
/// list, not only a render that ignores time.
struct TransportTone;

impl tutti_graph::Node for TransportTone {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_tail(tutti_units::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let env = *cx.env;
        let bumps = io.events(0).len() as f32;
        for k in env.offsets() {
            let t = env.transport_at(k);
            let v = if t.playing {
                t.beat().get().fract() as f32
            } else {
                0.0
            } + bumps;
            io.output(0)[k.index()] = v;
            io.output(1)[k.index()] = -v;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// `Engine::process` over the graph, with timestamped transport
/// commands (play, seek, tempo, loop, a declick stop) scheduled and landing
/// inside the gate, and graph notes at beats landing in blocks with transport
/// changes: the walk, the change list, the executor and the fold never
/// allocate. Device blocks of 1 024 frames against a 512-frame `MaxBlock`,
/// so the split into graph blocks runs too.
#[test]
fn graph_engine_with_timed_transport_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, EventIn, EventKind, Prepare, Unforkable};
    use tutti_graph::{OutPort, PortSource};
    use tutti_timeline::{MotionCommand, TransportCommand};
    use tutti_ump::{MidiChannel, U7};
    use tutti_units::{At, Beat, Frame};

    let sample_rate = 48_000.0;
    let transport = Transport::new(SampleRate::of(sample_rate));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(sample_rate),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    let mut output = vec![0.0f32; 1024 * 2];
    // Applying the commit allocates; that is the control side's price and
    // happens before the gate.
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
    ed.collect();
    let to = EventIn {
        node: NodeKey(1),
        port: 0,
    };
    for b in 0..8 {
        ed.schedule(
            At::Beat(Beat::new(0.25 * b as f64)),
            to,
            EventKind::Midi(midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100))),
        )
        .expect("room");
    }

    let m = &transport.motion();
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..20u64 {
            let base = 1024 * (1 + 8 * round);
            m.schedule(At::Frame(Frame::new(base + 300)), MotionCommand::Play)
                .expect("room");
            m.schedule(
                At::Frame(Frame::new(base + 2100)),
                TransportCommand::Loop(LoopEdit::over(
                    LoopRange::new(Beat::new(0.0), Beat::new(4.0)).expect("ordered"),
                )),
            )
            .expect("room");
            m.schedule(
                At::Beat(Beat::new(0.5)),
                MotionCommand::locate(Beat::new(0.0)),
            )
            .expect("room");
            m.schedule(At::Frame(Frame::new(base + 6000)), MotionCommand::stop())
                .expect("room");
            m.schedule(
                At::Frame(Frame::new(base + 7000)),
                TransportCommand::Loop(LoopEdit::armed(false)),
            )
            .expect("room");
            // The snapshot publish: a bounds edit and a re-arm.
            m.schedule(
                At::Frame(Frame::new(base + 7500)),
                LoopEdit::bounds(LoopRange::new(Beat::new(1.0), Beat::new(3.0)).expect("ordered")),
            )
            .expect("room");
            m.schedule(At::Frame(Frame::new(base + 7600)), LoopEdit::armed(true))
                .expect("room");
            for _ in 0..8 {
                engine
                    .process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
            }
            m.cancel_scheduled();
        }
    });
    // The engine really ran every block, through its own clock.
    assert_eq!(transport.settings().steady_time(), 1024 * (1 + 20 * 8));
}

/// A tempo map published between blocks reaches the engine's clock without
/// the audio thread allocating or freeing: the block lends the cell once, and
/// the maps it replaced are freed by the control side's sweep. The publish and
/// the sweep run outside the gate (they allocate and free, as they may); the
/// blocks run inside it, and the clock really takes each tempo (a lend that
/// never happened would pass by measuring nothing).
#[test]
fn a_published_tempo_map_is_taken_without_allocating() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{OutPort, PortSource};
    use tutti_units::Bpm;

    let transport = Transport::new(SampleRate::SR_48K);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let mut output = vec![0.0f32; 1024 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
    for round in 0..20u32 {
        let bpm = Bpm::of(80.0 + f64::from(round));
        transport.settings().set_tempo(bpm);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                engine
                    .process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
            }
        });
        assert_eq!(transport.settings().current_tempo(), bpm, "round {round}");
        transport.settings().reclaim_tempo();
    }
}

/// A tempo map of steps followed on the audio thread: the walk lends the cell
/// per step (never held across a call), the look ahead and the cut blocks
/// (41 steps 24 frames apart, so a 512-frame graph block carries more than the
/// 8 changes it holds), loop wraps that re-seat on a step, a seek back across
/// the steps and a map swap all run inside the gate. The publishes run outside
/// it; the clock really takes the steps (`current_tempo` reaches the last
/// step's tempo), so a walk that never ran would not pass.
///
/// The ramp case runs the same calls over a ramp, whose
/// cells are a change each: the graph takes 4 096-frame blocks, so a call
/// crosses 32 cell edges and the engine cuts it at every 8th change; loop
/// wraps inside the ramp re-seat on a cell, and a seek back and a map swap
/// land in the middle of it. Its clock really takes the cells
/// (`current_tempo` is the chord of a ramp cell, strictly between the ends).
#[test]
fn a_step_map_is_followed_without_allocating() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{OutPort, PortSource};
    use tutti_tempo::{TempoMap, TempoPoint, Transition};
    use tutti_timeline::MotionCommand;
    use tutti_units::{At, Beat, Bpm};

    let dense = |last: f64| {
        let mut points: Vec<TempoPoint> = (0..41)
            .map(|i| TempoPoint {
                beat: Beat::new(f64::from(i) * 0.001),
                tempo: Bpm::of(120.0 + f64::from(i)),
                transition: Transition::Step,
            })
            .collect();
        points.push(TempoPoint {
            beat: Beat::new(1.0),
            tempo: Bpm::of(last),
            transition: Transition::Step,
        });
        Arc::new(TempoMap::new(points).expect("a map"))
    };
    let transport = Transport::new(SampleRate::SR_48K);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings().set_tempo_map(dense(90.0));
    transport
        .motion()
        .try_send(MotionCommand::play_from(Beat::new(0.0)))
        .expect("room");
    let mut output = vec![0.0f32; 1024 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
    // 1 024 frames: past all 41 steps (the last at frame 960).
    assert_eq!(transport.settings().current_tempo(), Bpm::of(160.0));
    for round in 0..10u32 {
        // Control side: a new map, a loop over the steps, a seek back.
        transport
            .settings()
            .set_tempo_map(dense(80.0 + f64::from(round)));
        let m = &transport.motion();
        m.schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(0.0), Beat::new(0.02)).expect("ordered")),
        )
        .expect("room");
        m.try_send(MotionCommand::play_from(Beat::new(0.0)))
            .expect("room");
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                engine
                    .process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
            }
        });
        // Looping over beats 0 to 0.02 keeps the tempo among the first 21.
        let tempo = transport.settings().current_tempo().get();
        assert!((120.0..=140.0).contains(&tempo), "round {round}: {tempo}");
        transport.settings().reclaim_tempo();
    }

    // The ramp case: 120 to `end` BPM over 2 beats, linear in time.
    let ramp = |end: f64, transition: Transition| {
        Arc::new(
            TempoMap::new([
                TempoPoint {
                    beat: Beat::new(0.0),
                    tempo: Bpm::of(120.0),
                    transition,
                },
                TempoPoint {
                    beat: Beat::new(2.0),
                    tempo: Bpm::of(end),
                    transition: Transition::Step,
                },
            ])
            .expect("a map"),
        )
    };
    let transport = Transport::new(SampleRate::SR_48K);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(4_096)),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport
        .settings()
        .set_tempo_map(ramp(60.0, Transition::RampTime { end: None }));
    transport
        .motion()
        .try_send(MotionCommand::play_from(Beat::new(0.0)))
        .expect("room");
    let mut output = vec![0.0f32; 4_096 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
    let slowed = transport.settings().current_tempo().get();
    assert!(
        (60.0..119.0).contains(&slowed),
        "a ramp cell's chord: {slowed}"
    );
    for round in 0..10u32 {
        let shape = if round % 2 == 0 {
            Transition::RampTime { end: None }
        } else {
            Transition::RampBeats { end: None }
        };
        transport
            .settings()
            .set_tempo_map(ramp(50.0 + f64::from(round), shape));
        let m = &transport.motion();
        m.schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(0.1), Beat::new(0.3)).expect("ordered")),
        )
        .expect("room");
        m.try_send(MotionCommand::play_from(Beat::new(0.2)))
            .expect("room");
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                engine
                    .process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
            }
        });
        // Looping over beats 0.1 to 0.3 of a ramp that starts at 120 keeps the
        // chord between the ramp's tempo there and its start.
        let tempo = transport.settings().current_tempo().get();
        assert!((90.0..120.0).contains(&tempo), "round {round}: {tempo}");
        transport.settings().reclaim_tempo();
    }
}

/// The tap after the fader on the audio thread: writing (rings that fill and
/// count what they drop), taking an install, replacing the writer and taking a
/// close never allocate, and the writer a replace or close retires is sent
/// back, not freed (the engine enters `RtScope`, where a `TapWriter` drop is a
/// debug panic). The installs and closes are queued before each gate; the
/// engine takes them inside it.
#[test]
fn the_heard_tap_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{OutPort, PortSource};
    use tutti_timeline::MotionCommand;

    let transport = Transport::new(SampleRate::SR_48K);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let _ = transport.motion().try_send(MotionCommand::Play);
    let taps = engine.output_taps();
    let mut output = vec![0.0f32; 1024 * 2];
    let layout = ChannelLayout::STEREO;
    engine.process(&mut InterleavedMut::new(&mut output, layout).unwrap());
    ed.collect();

    // 16 frames of ring against 1024-frame blocks: every block overflows.
    let first = taps
        .open(layout, tutti_units::Samples::new(16))
        .expect("opens")
        .reader()
        .expect("the audio thread is not behind");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            engine.process(&mut InterleavedMut::new(&mut output, layout).unwrap());
        }
    });
    drop(first);
    taps.reclaim();
    // The reclaim queued a close; a new open queues a replacement behind it.
    let second = taps
        .open(layout, tutti_units::Samples::new(4096))
        .expect("reopens")
        .reader()
        .expect("the audio thread is not behind");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            engine.process(&mut InterleavedMut::new(&mut output, layout).unwrap());
        }
    });
    taps.close().unwrap();
    assert_no_alloc::assert_no_alloc(|| {
        engine.process(&mut InterleavedMut::new(&mut output, layout).unwrap());
    });
    taps.reclaim();
    assert_eq!(
        second.available().get(),
        4096,
        "the second tap filled its ring"
    );
    assert!(
        second.take_lost().get() > 0,
        "and counted what it could not hold"
    );
}

/// The metronome on the graph: `ClickNode` and a node walking its
/// block's beat (`Env::for_each_beat`: whole beats and fraction to outputs 2
/// and 3) side by side, with timestamped seeks, tempo and loop changes landing
/// inside blocks, so both walk several segments per block. Neither segment
/// walk nor the click's meter read allocates.
///
/// The meter read goes through a [`Publisher`]: a slot CAS, a fence and a
/// load, not an allocation — but that is a claim about another crate's
/// internals, so it is pinned here rather than reasoned about. The click
/// must be *sounding* for it to count: a gated-off metronome returns before
/// touching the meter, hence the `clicked` check.
///
/// [`Publisher`]: tutti_rt::Publisher
#[test]
fn graph_engine_with_env_clock_and_metronome_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Unforkable};
    use tutti_graph::{OutPort, PortSource};
    use tutti_timeline::{MotionCommand, TransportCommand};
    use tutti_units::Tail;
    use tutti_units::{At, Beat, Frame};

    /// The beat of every frame, whole beats then the fraction.
    struct BeatWalk;
    impl Node for BeatWalk {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::STEREO).with_tail(Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            cx.env.for_each_beat(|i, beat| {
                io.output(0)[i] = beat.floor().get() as f32;
                io.output(1)[i] = beat.fract().get() as f32;
            });
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    let sample_rate = 48_000.0;
    let transport = Transport::new(SampleRate::of(sample_rate));
    let settings = Arc::new(ClickSettings::new());
    settings.mode().store(MetronomeMode::Always);
    settings.set_volume(tutti_nodes::ClickLevel::of(tutti_units::Amplitude::new(
        1.0,
    )));
    let click = ClickNode::from(ClickNodeConfig::new(Arc::clone(&settings)));

    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(sample_rate),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    let (clock, sink) = (NodeKey(1), NodeKey(2));
    ed.insert(clock, "clock", Unforkable(BeatWalk));
    let _ = ed.insert(sink, "click", click);
    let topology = &mut ed.spec_mut().topology;
    topology.set_outputs(
        [(sink, 0), (sink, 1), (clock, 0), (clock, 1)]
            .into_iter()
            .map(|(node, port)| PortSource::Node(OutPort { node, port })),
    );
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    let four = ChannelLayout::from_count(4);
    let mut output = vec![0.0f32; 1024 * 4];
    // Applying the commit allocates; that is the control side's price.
    engine.process(&mut InterleavedMut::new(&mut output, four).unwrap());
    ed.collect();
    let _ = transport.motion().try_send(MotionCommand::Play);

    let m = &transport.motion();
    let mut clicked = false;
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..20u64 {
            let base = 1024 * (1 + 8 * round);
            m.schedule(
                At::Frame(Frame::new(base + 1500)),
                TransportCommand::Loop(LoopEdit::over(
                    LoopRange::new(Beat::new(0.0), Beat::new(2.0)).expect("ordered"),
                )),
            )
            .expect("room");
            m.schedule(
                At::Beat(Beat::new(1.75)),
                MotionCommand::locate(Beat::new(0.9)),
            )
            .expect("room");
            m.schedule(
                At::Frame(Frame::new(base + 7000)),
                TransportCommand::Loop(LoopEdit::armed(false)),
            )
            .expect("room");
            for _ in 0..8 {
                engine.process(&mut InterleavedMut::new(&mut output, four).unwrap());
                // Channel 0 of each frame: the click's left.
                clicked |= output.iter().step_by(4).any(|&s| s != 0.0);
            }
            m.cancel_scheduled();
        }
    });
    // Not vacuous: the click sounded inside the gate.
    assert!(clicked, "the metronome clicked");
}

/// A [`WorkerPool`](tutti_graph::WorkerPool) whose every participant runs its
/// share of a job inside `assert_no_alloc` — the helper threads too, which a
/// gate around the callback alone would not see (`assert_no_alloc` is per
/// thread).
struct GatedPool(tutti_graph::WorkerPool);

impl tutti_graph::Pool for GatedPool {
    fn participants(&self) -> usize {
        self.0.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.0
            .run(wake, &|i| assert_no_alloc::assert_no_alloc(|| job(i)));
    }
    fn waits(&self) -> tutti_rt::BlockWaits {
        self.0.waits()
    }
}

/// The engine on the parallel executor, on the engine's own pool: a block's
/// tasks spread over four participants, the callback wakes parked helpers
/// (the pool's one system call), and neither the callback nor any helper
/// allocates or frees while a block runs. The helpers' idle loop between
/// blocks (spin, yield, park) runs outside the job and is not gated; it
/// touches only atomics and `Thread::park`.
#[test]
fn graph_engine_on_a_worker_pool_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Edge, InPort, OutPort, PortSource};
    use tutti_graph::{Editor, ForkByClone, Prepare};

    let sample_rate = 48_000.0;
    let transport = Transport::new(SampleRate::of(sample_rate));
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(sample_rate),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    // Parks quickly, so the gated blocks below wake parked helpers.
    let pool = tutti_graph::WorkerPool::builder()
        .num_threads(4)
        .spin_budget(std::time::Duration::from_micros(1))
        .idle(std::time::Duration::from_micros(1))
        .build()
        .expect("a worker pool");
    exec.set_pool(Some(Arc::new(GatedPool(pool))));
    for k in 0..6u64 {
        ed.insert(
            NodeKey(k + 1),
            "sine",
            ForkByClone(crate::support::Sine::new(tutti_units::Hz::new(
                220.0 * (k + 1) as f64,
            ))),
        );
        ed.insert(
            NodeKey(k + 11),
            "gain",
            ForkByClone(crate::support::Gain(0.1 * (k + 1) as f32)),
        );
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(k + 11),
                port: 0,
            },
            vec![Edge::Direct(PortSource::Node(OutPort {
                node: NodeKey(k + 1),
                port: 0,
            }))],
        );
    }
    ed.spec_mut().topology.set_outputs(
        [11, 12]
            .map(|k| {
                PortSource::Node(OutPort {
                    node: NodeKey(k),
                    port: 0,
                })
            })
            .to_vec(),
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(exec.is_parallel(), "the plan spreads over the pool");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let mut output = vec![0.0f32; 512 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
    ed.collect();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
        }
    });
    // Idle long enough for every helper to park, then render again: the
    // wake is gated too.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..20 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap());
        }
    });
    assert!(output.iter().any(|&x| x != 0.0), "the graph rendered");
}

/// A source of twelve channels, one constant per channel.
struct TwelveWide;

impl tutti_graph::Node for TwelveWide {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::from(12u16))
            .with_tail(tutti_units::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for c in 0..12 {
            io.output(c).fill((c + 1) as f32 / 16.0);
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// A 7.1.4 root (twelve channels, the widest the cap allows past the old
/// eight) renders into a twelve-wide device straight and into a stereo one
/// through its fold without allocating, and the blocks carry the signal.
#[test]
fn a_714_root_block_is_allocation_free() {
    use tutti_graph::{Editor, NodeKey, OutPort, PortSource, Prepare, Unforkable};

    let transport = Transport::new(SampleRate::of(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(48_000.0),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "wide", Unforkable(TwelveWide));
    ed.spec_mut().topology.set_outputs((0..12).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("twelve is within the cap");
    engine
        .support_output(ChannelLayout::from(12u16))
        .expect("a width");

    let mut wide = vec![0.0f32; 512 * 12];
    let mut stereo = vec![0.0f32; 512 * 2];
    let layout = ChannelLayout::from(12u16);
    // The commit applies on the first block, which may allocate.
    engine.process(&mut InterleavedMut::new(&mut wide, layout).unwrap());
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..16 {
            engine.process(&mut InterleavedMut::new(&mut wide, layout).unwrap());
            engine.process(&mut InterleavedMut::new(&mut stereo, ChannelLayout::STEREO).unwrap());
        }
    });
    // Every channel of the twelve-wide device is its own constant, in every frame.
    for frame in wide.as_chunks::<12>().0 {
        for (c, &s) in frame.iter().enumerate() {
            assert_eq!(s, (c + 1) as f32 / 16.0, "channel {c}");
        }
    }
    // The 7.1.4 root (FL FR FC LFE SL SR BL BR TFL TFR TBL TBR) into stereo by
    // `Fold`'s documented rule: ITU-R BS.775 for the fronts (straight), the
    // centre (-3 dB into both) and the surrounds (-3 dB into the front on their
    // side), the LFE omitted, and tutti's table for the heights (-3 dB into the
    // front on their side).
    let v = |c: usize| (c + 1) as f32 / 16.0;
    let m = <f32 as tutti_buffers::Float>::MINUS_3_DB;
    let left = v(0) + m * (v(2) + v(4) + v(6) + v(8) + v(10));
    let right = v(1) + m * (v(2) + v(5) + v(7) + v(9) + v(11));
    for frame in stereo.as_chunks::<2>().0 {
        assert!(
            (frame[0] - left).abs() < 1e-5,
            "left {} vs {left}",
            frame[0]
        );
        assert!(
            (frame[1] - right).abs() < 1e-5,
            "right {} vs {right}",
            frame[1]
        );
    }
}

/// A published root and a stated device are looked up and applied without
/// allocating: a 7.1.4 root declared as the next generation, into a twelve-wide
/// device stated with its front pair swapped, renders the swapped fronts and
/// every other channel straight, in blocks that allocate nothing.
#[test]
fn a_declared_root_into_a_stated_device_is_allocation_free() {
    use tutti_buffers::{Bed, ChannelTopology, Speaker};
    use tutti_graph::{Editor, NodeKey, OutPort, PortSource, Prepare, Unforkable};

    let transport = Transport::new(SampleRate::of(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(48_000.0),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "wide", Unforkable(TwelveWide));
    ed.spec_mut().topology.set_outputs((0..12).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("twelve is within the cap");
    let control = engine.output_control();
    let twelve = ChannelLayout::from(12u16);
    let mut speakers = Bed::Surround714.topology().positions().unwrap().to_vec();
    speakers.swap(0, 1);
    assert_eq!(speakers[0], Speaker::FrontRight);
    control
        .state_device(twelve, Some(ChannelTopology::new(speakers).unwrap()))
        .expect("a stated device");
    let id = ed.spec().root_id.next();
    control
        .publish_root(id, Some(Bed::Surround714.topology()), twelve)
        .expect("places");
    ed.spec_mut().root_id = id;
    ed.commit().expect("commits");

    let mut out = vec![0.0f32; 512 * 12];
    // The first guarded block adopts the plan that carries the new root, lends
    // its folds and renders with them: the swap block allocates nothing.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..16 {
            engine.process(&mut InterleavedMut::new(&mut out, twelve).unwrap());
        }
    });
    let v = |c: usize| (c + 1) as f32 / 16.0;
    for frame in out.as_chunks::<12>().0 {
        assert_eq!(frame[0], v(1), "the left channel carries the right speaker");
        assert_eq!(frame[1], v(0));
        for (c, &s) in frame.iter().enumerate().skip(2) {
            assert_eq!(s, v(c), "channel {c}");
        }
    }
}

/// A routing set live (a stereo monitor on outputs 3 and 4) and output gains
/// ramping (a mute and a dim, retargeted in mid-ramp) are applied without
/// allocating: the 64-frame blocks through both ramps and after them are
/// guarded, each `set_levels` (which publishes on the control side) runs
/// between guarded calls, and the settled figures are checked.
#[test]
fn a_live_routing_and_ramping_output_gains_are_allocation_free() {
    use tutti_buffers::Bed;
    use tutti_engine::{OutputLevels, OutputMap, OutputStrip, Routing};
    use tutti_graph::{Editor, NodeKey, OutPort, PortSource, Prepare, Unforkable};
    use tutti_units::Amplitude;

    let transport = Transport::new(SampleRate::of(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(48_000.0),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "wide", Unforkable(TwelveWide));
    ed.spec_mut().topology.set_outputs((0..12).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("twelve is within the cap");
    let twelve = ChannelLayout::from(12u16);
    engine.support_output(twelve).expect("a width");
    let control = engine.output_control();
    // Blocks of 64 frames: the 240-frame declick crosses four of them.
    let mut out = vec![0.0f32; 64 * 12];
    let mut block = |engine: &Engine| {
        assert_no_alloc::assert_no_alloc(|| {
            engine.process(&mut InterleavedMut::new(&mut out, twelve).unwrap());
        });
    };
    // The first block adopts the plan (the commit apply is guarded elsewhere).
    engine.process(&mut InterleavedMut::new(&mut vec![0.0f32; 64 * 12], twelve).unwrap());

    control.set_routing(Routing {
        monitor: Some(Bed::Stereo.topology()),
        map: OutputMap::channels([2, 3]).unwrap(),
        ..Routing::default()
    });
    let strip = |mute| OutputStrip {
        mute,
        ..OutputStrip::default()
    };
    let mut levels = OutputLevels::new();
    levels.set_strip(2, strip(true)).unwrap();
    control.set_levels(&levels.clone().with_dim(Some(Amplitude::new(0.5))).unwrap());
    // Two blocks into the first ramp...
    block(&engine);
    block(&engine);
    // ...the targets turn: output 2 is heard again and output 3 is muted, at
    // another dim.
    levels.set_strip(2, strip(false)).unwrap();
    levels.set_strip(3, strip(true)).unwrap();
    control.set_levels(&levels.with_dim(Some(Amplitude::new(0.25))).unwrap());
    for _ in 0..8 {
        block(&engine);
    }
    // Settled: output 3 is silent, output 4 is the stereo fold's left channel
    // at the dim, the rest are silent.
    let v = |c: usize| (c + 1) as f32 / 16.0;
    let m = <f32 as tutti_buffers::Float>::MINUS_3_DB;
    let left = v(0) + m * (v(2) + v(4) + v(6) + v(8) + v(10));
    for frame in out.as_chunks::<12>().0 {
        assert!(
            (frame[2] - left * 0.25).abs() < 1e-5,
            "{} vs {}",
            frame[2],
            left * 0.25
        );
        for (c, &s) in frame.iter().enumerate() {
            if c != 2 {
                assert_eq!(s, 0.0, "output {c}");
            }
        }
    }
}

/// A routing change crossfades over the declick length (240 frames at 48 kHz, nearly four 64-frame blocks),
/// without allocating on the audio thread: the blocks through the fade (a
/// second change landing in mid-fade, which cuts) and after it are guarded,
/// each `set_routing` and `reclaim` (which free the replaced table) runs on
/// the control side between guarded calls, and the settled figure is checked.
#[test]
fn a_routing_crossfade_is_allocation_free() {
    use tutti_engine::{OutputMap, Routing};
    use tutti_graph::{Editor, NodeKey, OutPort, PortSource, Prepare, Unforkable};

    let transport = Transport::new(SampleRate::of(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(48_000.0),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "wide", Unforkable(TwelveWide));
    ed.spec_mut().topology.set_outputs((0..12).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("twelve is within the cap");
    let twelve = ChannelLayout::from(12u16);
    engine.support_output(twelve).expect("a width");
    let control = engine.output_control();
    let mut out = vec![0.0f32; 64 * 12];
    let mut block = |engine: &Engine| {
        assert_no_alloc::assert_no_alloc(|| {
            engine.process(&mut InterleavedMut::new(&mut out, twelve).unwrap());
        });
    };
    let to = |outputs: [usize; 12]| Routing {
        map: OutputMap::channels(outputs).unwrap(),
        ..Routing::default()
    };
    // The first block adopts the plan and the routing in force.
    engine.process(&mut InterleavedMut::new(&mut vec![0.0f32; 64 * 12], twelve).unwrap());

    // Reversed: a full fade, over blocks that straddle its length.
    control.set_routing(to([11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0]));
    for _ in 0..6 {
        block(&engine);
        let _ = engine.reclaim();
    }
    // A second change two blocks in, and a third one block after it.
    control.set_routing(to([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]));
    block(&engine);
    block(&engine);
    control.set_routing(to([1, 0, 3, 2, 5, 4, 7, 6, 9, 8, 11, 10]));
    block(&engine);
    for _ in 0..6 {
        block(&engine);
        let _ = engine.reclaim();
    }
    // Settled: heard channel `c` is output `c ^ 1`, at `(c + 1) / 16`.
    let v = |c: usize| (c + 1) as f32 / 16.0;
    for frame in out.as_chunks::<12>().0 {
        for (c, &s) in frame.iter().enumerate() {
            assert_eq!(s, v(c ^ 1), "output {c}");
        }
    }
}

/// Silence that reports 16 frames of latency, so the graph delays every other
/// output of a root it is in by 16 with a ring.
struct Latent;

impl tutti_graph::Node for Latent {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
            .with_latency(tutti_units::Latency::new(tutti_units::Samples::new(16)))
            .with_tail(tutti_units::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// A root change that crossfades over several blocks is applied without
/// allocating, a latent source in the old root included, whose delay rings
/// the install block copies into the old root's channels: a 7.1.4 root fades
/// out as a 5.1 root fades in over the 240-frame declick, which 64-frame blocks
/// cross in four, and every block (the one that installs the plan with the old
/// root's channels, the ones that fold both roots, and the one that ends the
/// fade) is guarded. The blocks after the
/// fade are the new root's fold alone, by its own rule.
#[test]
fn a_root_change_crossfaded_across_several_blocks_is_allocation_free() {
    use tutti_buffers::{Bed, MaxBlock};
    use tutti_graph::{Editor, Fade, NodeKey, OutPort, PortSource, Prepare, Unforkable};
    use tutti_units::{Samples, Seconds};

    let transport = Transport::new(SampleRate::of(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(48_000.0),
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(NodeKey(1), "wide", Unforkable(TwelveWide));
    ed.insert(NodeKey(2), "latent", Unforkable(Latent));
    let wire = |ed: &mut Editor, width: u16| {
        ed.spec_mut().topology.set_outputs((0..width).map(|port| {
            // The old root's last channel is the latent source's, so every
            // other channel of it is aligned by a ring; the new root has none.
            let node = if width == 12 && port == 11 { 2 } else { 1 };
            PortSource::Node(OutPort {
                node: NodeKey(node),
                port: if node == 2 { 0 } else { port },
            })
        }));
    };
    wire(&mut ed, 12);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("twelve is within the cap");
    ed.set_root_fade(Some(Fade::seconds(
        Seconds::new(0.005),
        SampleRate::of(48_000.0),
    )));
    let mut out = vec![0.0f32; 64 * 2];
    // The first block adopts the 7.1.4 plan.
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());

    let control = engine.output_control();
    let id = ed.spec().root_id.next();
    control
        .publish_root(id, Some(Bed::Surround51.topology()), ChannelLayout::STEREO)
        .expect("places");
    ed.spec_mut().root_id = id;
    wire(&mut ed, 6);
    ed.commit().expect("commits");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        }
    });
    let v = |c: usize| (c + 1) as f32 / 16.0;
    let m = <f32 as tutti_buffers::Float>::MINUS_3_DB;
    let (left, right) = (v(0) + m * (v(2) + v(4)), v(1) + m * (v(2) + v(5)));
    for frame in out.as_chunks::<2>().0 {
        assert!(
            (frame[0] - left).abs() < 1e-5,
            "left {} vs {left}",
            frame[0]
        );
        assert!(
            (frame[1] - right).abs() < 1e-5,
            "right {} vs {right}",
            frame[1]
        );
    }
}
