//! The control lanes (`ControlLaneNode`, automation-only ones too) driving param ports through a
//! graph never allocate on the audio thread, with no pool and on a pool: four automation lanes,
//! and 200 lanes of every layer kind with republishes, knob writes and range edits between
//! blocks. The LFO, the delays and the modulation effects are rows of the table.

use crate::harness::bus;
use tutti_buffers::MaxBlock;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_fixtures::{cycle_blocks, midi};
use tutti_graph::EdgeId;
use tutti_graph::contract::Direct;
use tutti_graph::{Node, ParamNode, ParamSet, TransportPosition};
use tutti_music::{NoteDuration, NoteValue};
use tutti_nodes::{
    ChannelTimes, DelayConfig, DelayControls, DelayLayout, DelayNode, FeedbackRouting,
    InterpolationMode, MaxDelay, NoteTimes, PhaserConfig, PhaserNode, Stages, Stagger, SweepRange,
};
use tutti_ump::{MidiChannel, U7};
use tutti_units::{
    Ballistics, Beat, BeatsPerCycle, Bpm, Feedback, Hz, ParamId, PhaseIncrement, Real, SampleRate,
    Seconds, UnitParam,
};

// ---- an event-output lane on the audio thread ---------------------------------

/// Writes the `Cutoff` it reads (base 0) per frame, allocation-free.
#[derive(Clone)]
struct ParamProbe;

impl tutti_graph::Node for ParamProbe {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
            .params(&[tutti_units::UnitParam::Cutoff])
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for i in 0..io.frames() {
            let v = io.param(0).frames().map_or(0.0, |f| f[i]);
            io.output(0)[i] = v;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        (port == 0).then_some(0.0)
    }
}

/// A sine of the beat: a curve with no pieces (the lane's fallback).
struct SineCurve;

impl tutti_nodes::automation::Curve for SineCurve {
    fn value_at(&self, beat: tutti_units::Beat) -> Option<f64> {
        Some((beat.get() * 7.0).sin())
    }
}

/// Four event-output lanes, each driving its own probe's cutoff (outputs
/// 0..4): exponential, cubic-out and S-curve pieces, a range-clamped piece
/// (the linear fallback), and a curve with no pieces.
fn lane_graph(max: usize) -> (tutti_graph::Editor, tutti_graph::Executor) {
    use std::sync::Arc;
    use tutti_graph::NodeKey;
    use tutti_graph::{EventOutputPort, ParamIn, Prepare, Unforkable};
    use tutti_graph::{OutputPort, PortSource};
    use tutti_modulation::ControlProgram;
    use tutti_nodes::automation::Exponent;
    use tutti_nodes::automation::{
        Breakpoint, ControlLaneNode, Curve, Law, Trajectory, UnitLaneConfig,
    };
    use tutti_units::{Beat, Hz, ParamKey, Samples, UnitParam};
    let mut shaped = Trajectory::new();
    shaped.add_breakpoint(
        Breakpoint::new(Beat::new(0.0), Hz::new(0.0)).law(Law::PowerIn(Exponent::of(2.0))),
    );
    shaped.add_breakpoint(
        Breakpoint::new(Beat::new(0.25), Hz::new(1.0)).law(Law::PowerOut(Exponent::of(3.0))),
    );
    shaped.add_breakpoint(
        Breakpoint::new(Beat::new(0.4), Hz::new(0.2)).law(Law::PowerInOut(Exponent::of(2.0))),
    );
    shaped.add_breakpoint(Breakpoint::new(Beat::new(0.55), Hz::new(0.9)));
    let mut clamped = Trajectory::new().with_range(Hz::new(0.0), Hz::new(0.5));
    clamped.add_breakpoint(
        Breakpoint::new(Beat::new(0.1), Hz::new(0.0)).law(Law::PowerIn(Exponent::of(2.0))),
    );
    clamped.add_breakpoint(Breakpoint::new(Beat::new(0.45), Hz::new(1.0)));
    let key = ParamKey::<Hz>::CUTOFF;
    let lane = |curve: Arc<dyn Curve>| {
        ControlLaneNode::from(UnitLaneConfig::new(
            key.addr(),
            ControlProgram::from_curve(curve),
        ))
    };
    let lanes = [
        lane(Arc::new(shaped.clone())),
        lane(Arc::new(clamped)),
        lane(Arc::new(SineCurve)),
        lane(Arc::new(shaped)),
    ];
    let (mut ed, exec) = tutti_graph::Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(max)),
    ));
    let mut outputs = Vec::new();
    for (k, lane) in lanes.into_iter().enumerate() {
        let (l, p) = (NodeKey(10 + k as u64), NodeKey(20 + k as u64));
        ed.insert(l, "lane", lane);
        ed.insert(p, "probe", Unforkable(ParamProbe));
        ed.connect_param(
            ParamIn {
                node: p,
                param: UnitParam::Cutoff.into(),
            },
            EventOutputPort::new(l, 0),
        )
        .unwrap();
        outputs.push(PortSource::Node(OutputPort::new(p, 0)));
    }
    ed.wire_outputs(outputs).unwrap();
    ed.commit().expect("commits");
    (ed, exec)
}

/// The block schedule the lane rigs render: ragged, up to 128 frames.
fn lane_blocks(total: usize) -> Vec<usize> {
    cycle_blocks(&[1, 128, 7, 64, 127, 33, 2, 100], total)
}

/// One block of the lane graph at the host's clock `clock` (113 BPM,
/// looping 0.1 to 0.5 beats, so wraps land inside blocks), the clock moved
/// on; the four outputs appended to `out`.
fn lane_block(
    exec: &mut tutti_graph::Executor,
    clock: &mut tutti_music::FrameClock,
    n: usize,
    bufs: &mut [Vec<f32>; 4],
) {
    let looping =
        tutti_music::LoopRange::new(tutti_units::Beat::new(0.1), tutti_units::Beat::new(0.5));
    let t = tutti_graph::TransportPosition::counted(true, clock.tempo(), clock.origin(), looping);
    let [a, b, c, d] = bufs;
    exec.process(
        n,
        &t,
        &[],
        &mut [&mut a[..n], &mut b[..n], &mut c[..n], &mut d[..n]],
    );
    clock.advance(tutti_units::Samples::new(n), looping);
}

/// An automation lane's event output (a `ControlLaneNode` of automation alone)
/// wired to a param port never allocates on the audio thread: curve
/// segments at breakpoints, loop wraps inside blocks, the linear fallback
/// for a range-clamped piece and for a curve with no pieces, ragged blocks;
/// one-participant, and on a pool whose helpers run under the gate too.
#[test]
fn event_lanes_are_allocation_free_serial_and_on_a_pool() {
    use std::sync::Arc;
    for workers in [None, Some(4)] {
        let (mut ed, mut exec) = lane_graph(128);
        if let Some(w) = workers {
            let pool = tutti_graph::WorkerPool::builder()
                .num_threads(w)
                .spin_budget(std::time::Duration::from_micros(1))
                .idle(std::time::Duration::from_micros(1))
                .build()
                .expect("a worker pool");
            exec.set_pool(Some(Arc::new(pool)));
        }
        let mut clock = tutti_music::FrameClock::new(
            tutti_units::Beat::new(0.0),
            tutti_units::Bpm::of(113.0),
            SampleRate::SR_48K,
        );
        let mut bufs: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.0; 128]);
        for n in lane_blocks(4_000) {
            lane_block(&mut exec, &mut clock, n, &mut bufs);
            ed.collect();
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        assert!(
            bufs.iter().all(|b| b.iter().any(|v| v.abs() > 1e-4)),
            "every lane moved its param: the gate walks the segment path"
        );
        let blocks = lane_blocks(30_000);
        for &n in &blocks {
            lane_block(&mut exec, &mut clock, n, &mut bufs);
        }
        ed.collect();
    }
}

/// The lanes render bit for bit the same with no pool and on pools of 2, 3, 4
/// and 8 participants, under ragged blocks with loop wraps inside them.
///
/// It does not test a property of the lane: a lane's state is
/// its own (one node, one op), so nothing it does can differ between the
/// two; this pins that the event path keeps it that way (a lane sharing
/// state with another would diverge here).
#[test]
fn event_lanes_render_the_same_serial_and_parallel() {
    use std::sync::Arc;
    let render = |workers: Option<usize>| {
        let (mut ed, mut exec) = lane_graph(128);
        if let Some(w) = workers {
            exec.set_pool(Some(Arc::new(
                tutti_graph::WorkerPool::builder()
                    .num_threads(w)
                    .build()
                    .expect("a worker pool"),
            )));
        }
        let mut clock = tutti_music::FrameClock::new(
            tutti_units::Beat::new(0.0),
            tutti_units::Bpm::of(113.0),
            SampleRate::SR_48K,
        );
        let mut bufs: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.0; 128]);
        let mut all: [Vec<u32>; 4] = Default::default();
        for n in lane_blocks(20_000) {
            lane_block(&mut exec, &mut clock, n, &mut bufs);
            ed.collect();
            for (a, b) in all.iter_mut().zip(&bufs) {
                a.extend(b[..n].iter().map(|v| v.to_bits()));
            }
        }
        all
    };
    let serial = render(None);
    for w in [2, 3, 4, 8] {
        assert!(render(Some(w)) == serial, "{w} participants");
    }
}

// ---- 200 control lanes ----------------------------------------------------

/// The eight params a [`Probe8`] declares, one per lane of a group.
const PROBE8: [tutti_units::UnitParam; 8] = {
    use tutti_units::UnitParam::*;
    [Cutoff, Q, GainDb, Wet, Feedback, Depth, Rate, Threshold]
};

/// Declares the eight [`PROBE8`] params (base 0) and writes each port's
/// value to its own output, allocation-free.
struct Probe8;

impl tutti_graph::Node for Probe8 {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::from_count(8)).params(&PROBE8)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for p in 0..PROBE8.len() {
            for i in 0..io.frames() {
                let v = io.param(p).frames().map_or(0.0, |f| f[i]);
                io.output(p)[i] = v;
            }
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        (port < PROBE8.len()).then_some(0.0)
    }
}

/// Lane `k`'s program in generation `generation` (a republish moves its depths):
/// five kinds in turn, beats, seconds, drawn, random, and automation under
/// a layer.
fn lane_program(k: usize, generation: u32) -> tutti_modulation::ControlProgram {
    use std::sync::Arc;
    use tutti_modulation::{
        Amount, ControlProgramBuilder, Law, Layer, LayerClock, LayerId, LfoShape, PeriodPoint,
        Polarity, Waveform,
    };
    use tutti_nodes::automation::{Breakpoint, Exponent, Trajectory};
    use tutti_units::{Beat, Bipolar, Hz, Phase, PhaseIncrement};
    let depth = 0.05 + 0.01 * f64::from((k as u32 + generation) % 7);
    let amount = Amount {
        depth: Bipolar::of(depth),
        polarity: Polarity::Bipolar,
        curve: if k.is_multiple_of(3) {
            Law::PowerIn(Exponent::of(2.0))
        } else {
            Law::Linear
        },
    };
    let beats = LayerClock::Beats {
        per_cycle: BeatsPerCycle::new(0.25 + 0.125 * (k % 5) as f64).expect("above zero"),
        phase: PhaseIncrement::new(0.0),
    };
    let seconds = LayerClock::Seconds {
        rate: Hz::new(1.0 + (k % 11) as f64),
        phase: PhaseIncrement::new(0.0),
        anchor: None,
    };
    let shape = |shape| Waveform::Shape {
        shape,
        seed: k as u64,
    };
    let layer = |clock, wave| Layer {
        id: LayerId(1),
        clock,
        wave,
        amount,
        depth_by: None,
    };
    let (automation, layers) = match k % 5 {
        0 => (None, vec![layer(beats, shape(LfoShape::Sine))]),
        1 => (None, vec![layer(seconds, shape(LfoShape::Triangle))]),
        2 => (
            None,
            vec![layer(
                beats,
                Waveform::Drawn(Arc::from(vec![
                    PeriodPoint {
                        at: Phase::new(0.0),
                        value: -0.5,
                        curve: Law::Linear,
                    },
                    PeriodPoint {
                        at: Phase::new(0.4),
                        value: 1.0,
                        curve: Law::PowerInOut(Exponent::of(2.0)),
                    },
                ])),
            )],
        ),
        3 => (None, vec![layer(seconds, shape(LfoShape::RandomSmooth))]),
        _ => {
            let mut e = Trajectory::new();
            e.add_breakpoint(
                Breakpoint::new(Beat::new(0.0), Hz::new(200.0))
                    .law(Law::PowerIn(Exponent::of(2.0))),
            );
            e.add_breakpoint(Breakpoint::new(Beat::new(1.3), Hz::new(800.0)));
            let curve: Arc<dyn tutti_modulation::Curve> = Arc::new(e);
            (Some(curve), vec![layer(seconds, shape(LfoShape::Sawtooth))])
        }
    };
    let mut builder = ControlProgramBuilder::new().layers(layers);
    if let Some(automation) = automation {
        builder = builder.automation(automation);
    }
    builder.build().expect("a valid program")
}

/// Lane `k` for param `k % 8` of its probe, built against that port's
/// range cell, with a knob of its own.
fn lane_for(
    k: usize,
    range: tutti_graph::ParamRangeCell,
) -> (
    tutti_nodes::automation::ControlLaneNode<tutti_nodes::automation::PortLane>,
    tutti_rt::ParamCell,
) {
    use atomic_float::AtomicF64;
    use std::sync::Arc;
    use tutti_nodes::automation::ControlLaneNode as L;
    use tutti_nodes::automation::UnitLaneConfig;
    use tutti_units::{Bipolar, Db, Feedback, Hz, Mix, ParamKey, Q};
    let addr = match k % 8 {
        0 => ParamKey::<Hz>::CUTOFF.addr(),
        1 => ParamKey::<Q>::Q.addr(),
        2 => ParamKey::<Db>::GAIN_DB.addr(),
        3 => ParamKey::<Mix>::WET.addr(),
        4 => ParamKey::<Feedback>::FEEDBACK.addr(),
        5 => ParamKey::<Bipolar>::DEPTH.addr(),
        6 => ParamKey::<Hz>::RATE.addr(),
        _ => ParamKey::<Db>::THRESHOLD.addr(),
    };
    let knob: tutti_rt::ParamCell = Arc::new(AtomicF64::new(400.0 + k as f64)).into();
    let lane = L::from(
        UnitLaneConfig::new(addr, lane_program(k, 0))
            .knob(knob.clone())
            .range(range),
    );
    (lane, knob)
}

/// 200 lanes into the params of 25 [`Probe8`]s (one lane per port: a port
/// takes one event source), ranged 0..1000; the 200 outputs in lane order.
fn lanes_200(
    max: usize,
) -> (
    tutti_graph::Editor,
    tutti_graph::Executor,
    Vec<(tutti_nodes::automation::LaneControls, tutti_rt::ParamCell)>,
) {
    use tutti_graph::NodeKey;
    use tutti_graph::{EventOutputPort, ParamIn, ParamRange, Prepare, Unforkable};
    use tutti_graph::{OutputPort, PortSource};
    use tutti_units::Samples;
    let (mut ed, exec) = tutti_graph::Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(max)),
    ));
    let at = |k: usize| ParamIn {
        node: NodeKey(1000 + (k / 8) as u64),
        param: PROBE8[k % 8].into(),
    };
    for j in 0..25u64 {
        ed.insert(NodeKey(1000 + j), "probe", Unforkable(Probe8));
        for p in 0..8 {
            {
                let channel = u16::try_from(ed.spec().topology.outputs.len()).expect("a width");
                ed.resize_outputs(ChannelLayout::from_count(channel + 1));
                ed.connect_edge(EdgeId::into_output(
                    channel,
                    PortSource::Node(OutputPort::new(NodeKey(1000 + j), p)),
                ))
                .unwrap()
            };
        }
    }
    for k in 0..200 {
        ed.set_param_range(at(k), ParamRange::of(0.0, 1000.0))
            .expect("a probe");
    }
    ed.commit().expect("commits");
    let mut controls = Vec::new();
    for k in 0..200 {
        let (lane, knob) = lane_for(k, ed.param_range_cell(at(k)).expect("committed"));
        let key = NodeKey(2000 + k as u64);
        controls.push((ed.insert(key, "lane", lane).controls, knob));
        ed.connect_param(at(k), EventOutputPort::new(key, 0))
            .unwrap();
    }
    ed.commit().expect("commits");
    (ed, exec, controls)
}

/// The control-thread moves between blocks, the same in every render:
/// every 10th block a republish of every program (new depths), every block
/// a knob write on every 7th lane, every 15th block a range edit on every
/// 9th port.
fn lane_moves(
    ed: &mut tutti_graph::Editor,
    controls: &[(tutti_nodes::automation::LaneControls, tutti_rt::ParamCell)],
    block: usize,
) {
    use tutti_graph::NodeKey;
    use tutti_graph::{ParamIn, ParamRange};
    if block % 10 == 9 {
        for (k, (c, _)) in controls.iter().enumerate() {
            c.publish(lane_program(k, block as u32));
        }
    }
    for (k, (_, knob)) in controls.iter().enumerate().skip(block % 7).step_by(7) {
        knob.store(
            f64::from(300.0 + ((k + block) % 13) as f32 * 20.0),
            std::sync::atomic::Ordering::Release,
        );
    }
    if block % 15 == 14 {
        for k in (block % 9..200).step_by(9) {
            let at = ParamIn {
                node: NodeKey(1000 + (k / 8) as u64),
                param: PROBE8[k % 8].into(),
            };
            let hi = 800.0 + ((k + block) % 5) as f32 * 100.0;
            ed.set_param_range(at, ParamRange::of(0.0, f64::from(hi)))
                .expect("a probe");
        }
    }
}

/// The 120 BPM transport the 200-lane rigs roll, looping 0.5 to 1.7 beats
/// (wraps inside blocks).
fn lanes_transport(clock: &tutti_music::FrameClock) -> tutti_graph::TransportPosition {
    tutti_graph::TransportPosition::counted(
        true,
        clock.tempo(),
        clock.origin(),
        Some(
            tutti_music::LoopRange::new(tutti_units::Beat::new(0.5), tutti_units::Beat::new(1.7))
                .expect("a loop"),
        ),
    )
}

fn lanes_loop() -> Option<tutti_music::LoopRange> {
    tutti_music::LoopRange::new(tutti_units::Beat::new(0.5), tutti_units::Beat::new(1.7))
}

/// 200 control lanes render bit for bit the same with no pool and
/// on pools of 2, 3, 4 and 8 participants, under ragged blocks with loop
/// wraps inside them, republishes of every program, knob writes and range
/// edits between blocks.
///
/// It does not test a property of the lane: each lane's state
/// is its own (one node, one op, its cells read once per block), so nothing
/// it does can differ between schedules. This pins that it stays so: two
/// lanes sharing mutable state outside the graph
/// would diverge here.
#[test]
fn two_hundred_lanes_render_the_same_serial_and_parallel() {
    use std::sync::Arc;
    let render = |workers: Option<usize>| {
        let (mut ed, mut exec, controls) = lanes_200(128);
        if let Some(w) = workers {
            exec.set_pool(Some(Arc::new(
                tutti_graph::WorkerPool::builder()
                    .num_threads(w)
                    .build()
                    .expect("a worker pool"),
            )));
        }
        let mut clock = tutti_music::FrameClock::new(
            tutti_units::Beat::new(0.0),
            tutti_units::Bpm::of(120.0),
            SampleRate::SR_48K,
        );
        let mut bufs = vec![vec![0.0f32; 128]; 200];
        let mut all = vec![Vec::new(); 200];
        for (b, n) in lane_blocks(12_000).into_iter().enumerate() {
            lane_moves(&mut ed, &controls, b);
            let t = lanes_transport(&clock);
            {
                let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
                exec.process(n, &t, &[], &mut refs);
            }
            clock.advance(tutti_units::Samples::new(n), lanes_loop());
            ed.collect();
            for (a, b) in all.iter_mut().zip(&bufs) {
                a.extend(b[..n].iter().map(|v| v.to_bits()));
            }
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        all
    };
    let serial = render(None);
    assert!(
        serial.iter().all(|c| c.iter().any(|&v| v != c[0])),
        "every lane moves its port"
    );
    for w in [2, 3, 4, 8] {
        let par = render(Some(w));
        for (k, (a, b)) in serial.iter().zip(&par).enumerate() {
            assert!(a == b, "{w} participants, lane {k}");
        }
    }
}

/// 200 control lanes never allocate on the audio thread, with no pool and on a
/// pool whose helpers run under the gate too, with a republish of every
/// program, knob writes and range edits between gated blocks (on the
/// control thread, outside the gate).
#[test]
fn two_hundred_lanes_are_allocation_free_serial_and_on_a_pool() {
    use std::sync::Arc;
    for workers in [None, Some(4)] {
        let (mut ed, mut exec, controls) = lanes_200(128);
        if let Some(w) = workers {
            let pool = tutti_graph::WorkerPool::builder()
                .num_threads(w)
                .spin_budget(std::time::Duration::from_micros(1))
                .idle(std::time::Duration::from_micros(1))
                .build()
                .expect("a worker pool");
            exec.set_pool(Some(Arc::new(pool)));
        }
        let mut clock = tutti_music::FrameClock::new(
            tutti_units::Beat::new(0.0),
            tutti_units::Bpm::of(120.0),
            SampleRate::SR_48K,
        );
        let mut bufs = vec![vec![0.0f32; 128]; 200];
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        let blocks = lane_blocks(24_000);
        for (b, &n) in blocks.iter().enumerate() {
            lane_moves(&mut ed, &controls, b);
            let t = lanes_transport(&clock);
            exec.process(n, &t, &[], &mut refs);
            clock.advance(tutti_units::Samples::new(n), lanes_loop());
            ed.collect();
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        drop(refs);
        assert!(
            bufs.iter().all(|b| b.iter().any(|&v| v != 0.0)),
            "every lane drove its port: the gate walked the lanes"
        );
    }
}

// ---- signal sources into lanes -------------------------------

/// Stereo bursts on the executor's clock: 1.0 (the right channel at half)
/// on the first `width` frames of every `period`, allocation-free.
#[derive(Clone)]
struct Bursts {
    period: u64,
    width: u64,
}

impl tutti_graph::Node for Bursts {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let base = cx.env.frame.get();
        for i in 0..io.frames() {
            let v = if (base + i as u64) % self.period < self.width {
                1.0
            } else {
                0.0
            };
            io.output(0)[i] = v;
            io.output(1)[i] = v * 0.5;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// Two overlapping notes every `period` frames of the executor's clock (A
/// on at 0, B on at a quarter, A off at a half, B off at three quarters),
/// allocation-free.
#[derive(Clone)]
struct Notes {
    period: u64,
}

impl Notes {
    const SHAPE: tutti_graph::Shape = tutti_graph::Shape::EMPTY.events(0, 1);
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl tutti_graph::Node for Notes {
    fn shape(&self) -> tutti_graph::Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let (base, n) = (cx.env.frame.get(), cx.env.block_len);
        let q = self.period / 4;
        for i in 0..n.get() {
            let (c, v) = (MidiChannel::FIRST, U7::of(100));
            let ump = match (base + i as u64) % self.period {
                0 => midi::m1_note_on(c, 60, v),
                x if x == q => midi::m1_note_on(c, 62, v),
                x if x == 2 * q => midi::m1_note_off(c, 60, U7::of(0)),
                x if x == 3 * q => midi::m1_note_off(c, 62, U7::of(0)),
                _ => continue,
            };
            let at = tutti_graph::Offset::of(tutti_units::Samples::new(i), n);
            let _ = io
                .event_out(Self::OUT)
                .push(tutti_graph::Event::midi(at, ump));
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// `CHAINS` followers (stereo bursts in) and `CHAINS` note envelopes (two
/// overlapping notes in), each into a lane on a probe's cutoff (knob 0.5,
/// range 0..1, depth ±0.4 on source `k`); the probes' outputs in chain
/// order, followers first.
const CHAINS: usize = 12;

fn signal_graph(max: usize) -> (tutti_graph::Editor, tutti_graph::Executor) {
    use atomic_float::AtomicF64;
    use std::sync::Arc;
    use tutti_graph::NodeKey;
    use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
    use tutti_graph::{
        EventEdge, EventInputPort, EventOutputPort, ForkByClone, ParamIn, ParamRange, Prepare,
    };
    use tutti_modulation::{
        Adsr, Amount, ControlProgramBuilder, InputLayer, Law, LayerId, Polarity, SourceId,
    };
    use tutti_nodes::automation::ControlLaneNode;
    use tutti_nodes::automation::UnitLaneConfig;
    use tutti_nodes::{
        EnvelopeFollowerConfig, EnvelopeFollowerNode, NoteEnvelopeConfig, NoteEnvelopeNode,
    };
    use tutti_units::{Amplitude, Bipolar, Hz, ParamKey, Samples, Seconds, UnitParam};
    let (mut ed, exec) = tutti_graph::Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(max)),
    ));
    let probe = |k: usize| NodeKey(5000 + k as u64);
    let cutoff = |k: usize| ParamIn {
        node: probe(k),
        param: UnitParam::Cutoff.into(),
    };
    for k in 0..2 * CHAINS {
        ed.insert(probe(k), "probe", ForkByClone(ParamProbe));
        {
            let channel = u16::try_from(ed.spec().topology.outputs.len()).expect("a width");
            ed.resize_outputs(ChannelLayout::from_count(channel + 1));
            ed.connect_edge(EdgeId::into_output(
                channel,
                PortSource::Node(OutputPort::new(probe(k), 0)),
            ))
            .unwrap()
        };
        ed.set_param_range(cutoff(k), ParamRange::of(0.0, 1.0))
            .expect("a probe");
    }
    ed.commit().expect("commits");
    for k in 0..2 * CHAINS {
        let (src, node) = (NodeKey(3000 + k as u64), NodeKey(4000 + k as u64));
        let id = SourceId::new(ParamId::new(k as u32));
        if k < CHAINS {
            ed.insert(
                src,
                "bursts",
                ForkByClone(Bursts {
                    period: 1_500 + 97 * k as u64,
                    width: 300 + 11 * k as u64,
                }),
            );
            let follower = EnvelopeFollowerNode::from(EnvelopeFollowerConfig::new(
                ChannelLayout::STEREO,
                id,
                Ballistics::new(Seconds::new(0.001 + 0.0005 * k as f64), Seconds::new(0.03))
                    .unwrap(),
            ));
            ed.insert(node, "follower", ForkByClone(follower));
            for port in 0..2 {
                ed.connect_edge(EdgeId::into_port(
                    InputPort::new(node, port),
                    Edge::Direct(PortSource::Node(OutputPort::new(src, port))),
                ))
                .unwrap();
            }
        } else {
            ed.insert(
                src,
                "notes",
                ForkByClone(Notes {
                    period: 2_000 + 64 * k as u64,
                }),
            );
            let adsr = Adsr {
                attack: Seconds::new(0.002),
                decay: Seconds::new(0.004),
                sustain: Amplitude::new(0.6),
                release: Seconds::new(0.005),
            };
            ed.insert(
                node,
                "envelope",
                ForkByClone(NoteEnvelopeNode::from(NoteEnvelopeConfig::new(id, adsr))),
            );
            ed.connect_events(
                EventInputPort::new(node, 0),
                EventEdge::Direct(EventOutputPort::new(src, 0)),
            )
            .unwrap();
        }
        let input = InputLayer {
            id: LayerId(1),
            source: id,
            amount: Amount {
                depth: Bipolar::of(if k % 2 == 0 { -0.4 } else { 0.4 }),
                polarity: Polarity::Bipolar,
                curve: Law::Linear,
            },
        };
        let lane = ControlLaneNode::from(
            UnitLaneConfig::new(
                ParamKey::<Hz>::CUTOFF.addr(),
                ControlProgramBuilder::new()
                    .inputs(vec![input])
                    .build()
                    .expect("valid"),
            )
            .knob(Arc::new(AtomicF64::new(0.5)).into())
            .range(ed.param_range_cell(cutoff(k)).expect("committed")),
        );
        let lane_key = NodeKey(6000 + k as u64);
        ed.insert(lane_key, "lane", lane);
        ed.connect_events(
            EventInputPort::new(lane_key, 0),
            EventEdge::Direct(EventOutputPort::new(node, 0)),
        )
        .unwrap();
        ed.connect_param(cutoff(k), EventOutputPort::new(lane_key, 0))
            .unwrap();
    }
    ed.commit().expect("commits");
    (ed, exec)
}

/// Block `b` of the signal rig: stopped for 20 blocks, a play start inside
/// block 20 (both kinds of source reset on its frame), then rolling. `n`
/// frames; the clock moved on while rolling.
fn signal_block(
    exec: &mut tutti_graph::Executor,
    clock: &mut tutti_music::FrameClock,
    b: usize,
    n: usize,
    refs: &mut [&mut [f32]],
) {
    use tutti_graph::{Offset, TransportChanges, TransportPosition};
    let rolling =
        |c: &tutti_music::FrameClock| TransportPosition::counted(true, c.tempo(), c.origin(), None);
    let stopped = TransportPosition::new(false, clock.tempo(), tutti_units::Beat::new(0.0), None);
    let mut ch = TransportChanges::NONE;
    let t = match b {
        0..20 => stopped,
        20 => {
            let at = (n / 2).min(n - 1);
            ch.push(
                Offset::of(tutti_units::Samples::new(at), tutti_units::Samples::new(n)),
                rolling(clock),
            )
            .expect("room");
            clock.advance(tutti_units::Samples::new(n - at), None);
            stopped
        }
        _ => {
            let t = rolling(clock);
            clock.advance(tutti_units::Samples::new(n), None);
            t
        }
    };
    exec.process_with_changes(n, &t, &ch, &[], refs);
}

/// Followers and note envelopes into lanes render bit for bit the same
/// with no pool and on pools of 2, 3, 4 and 8, under ragged blocks with a play
/// start inside one.
///
/// It does not test a property of the sources: each node's
/// state is its own (one node, one op), so nothing it does can differ
/// between schedules. This pins that it stays so.
#[test]
#[cfg_attr(miri, ignore = "threaded worker pools")]
fn signal_sources_render_the_same_serial_and_parallel() {
    use std::sync::Arc;
    let render = |workers: Option<usize>| {
        let (mut ed, mut exec) = signal_graph(128);
        if let Some(w) = workers {
            exec.set_pool(Some(Arc::new(
                tutti_graph::WorkerPool::builder()
                    .num_threads(w)
                    .build()
                    .expect("a worker pool"),
            )));
        }
        let mut clock = tutti_music::FrameClock::new(
            tutti_units::Beat::new(0.0),
            tutti_units::Bpm::of(120.0),
            SampleRate::SR_48K,
        );
        let mut bufs = vec![vec![0.0f32; 128]; 2 * CHAINS];
        let mut all = vec![Vec::new(); 2 * CHAINS];
        for (b, n) in lane_blocks(20_000).into_iter().enumerate() {
            {
                let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..n]).collect();
                signal_block(&mut exec, &mut clock, b, n, &mut refs);
            }
            ed.collect();
            for (a, b) in all.iter_mut().zip(&bufs) {
                a.extend(b[..n].iter().map(|v| v.to_bits()));
            }
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        all
    };
    let serial = render(None);
    assert!(
        serial.iter().all(|c| c.iter().any(|&v| v != c[0])),
        "every source moves its port"
    );
    for w in [2, 3, 4, 8] {
        let par = render(Some(w));
        for (k, (a, b)) in serial.iter().zip(&par).enumerate() {
            assert!(a == b, "{w} participants, chain {k}");
        }
    }
}

/// Followers and note envelopes (and the lanes they feed) never allocate
/// on the audio thread, with no pool and on a pool whose helpers run under the
/// gate too, through a play start inside a block.
#[test]
#[cfg_attr(miri, ignore = "threaded worker pools")]
fn signal_sources_are_allocation_free_serial_and_on_a_pool() {
    use std::sync::Arc;
    for workers in [None, Some(4)] {
        let (mut ed, mut exec) = signal_graph(128);
        if let Some(w) = workers {
            let pool = tutti_graph::WorkerPool::builder()
                .num_threads(w)
                .spin_budget(std::time::Duration::from_micros(1))
                .idle(std::time::Duration::from_micros(1))
                .build()
                .expect("a worker pool");
            exec.set_pool(Some(Arc::new(pool)));
        }
        let mut clock = tutti_music::FrameClock::new(
            tutti_units::Beat::new(0.0),
            tutti_units::Bpm::of(120.0),
            SampleRate::SR_48K,
        );
        let mut bufs = vec![vec![0.0f32; 128]; 2 * CHAINS];
        let blocks = lane_blocks(24_000);
        // Warm-up outside the gate: the first blocks (and the first
        // commit's leftovers) on the control thread.
        for (b, &n) in blocks.iter().enumerate().take(4) {
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..n]).collect();
            signal_block(&mut exec, &mut clock, b, n, &mut refs);
            drop(refs);
            ed.collect();
        }
        // The executor takes output slices longer than the block (it
        // writes the first `n` frames of each).
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        // Each chain's first gated value, and whether it ever left it (on
        // the stack: the check runs inside the gate).
        let mut first = [f32::NAN; 2 * CHAINS];
        let mut moved = [false; 2 * CHAINS];
        for (b, &n) in blocks.iter().enumerate().skip(4) {
            signal_block(&mut exec, &mut clock, b, n, &mut refs);
            for (k, r) in refs.iter().enumerate() {
                for &v in &r[..n] {
                    if first[k].is_nan() {
                        first[k] = v;
                    }
                    moved[k] |= v != first[k];
                }
            }
        }
        drop(refs);
        ed.collect();
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        assert!(
            moved.iter().all(|&m| m),
            "every source moved its port inside the gate: {moved:?}"
        );
    }
}

/// **An `apply` that republishes allocates nothing on the audio thread.**
/// A saw into a low-pass SVF; `tutti_nodes::Modulation` puts a 2 Hz LFO on
/// its cutoff and a follower of the saw on its Q. Between gated blocks a
/// depth edit is applied (on the control thread, outside the gate): each
/// republishes the LFO's lane and sends no plan, and the blocks after each
/// one read the new program inside the gate. (That the lane adopts it, the
/// render moving with the depth, is `modulation_reconcile.rs`' depth test;
/// a single-threaded gate pins only the reader's path.)
#[test]
fn an_apply_that_republishes_is_allocation_free_on_the_audio_thread() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
    use tutti_graph::{Editor, ForkByClone, Prepare, TransportPosition};
    use tutti_modulation::{
        Declarations, Law, LayerClock, Polarity, RouteDecl, RouteKey, RouteTarget,
        Source as ModSource, SourceKey, TargetDecl, TargetKey, Waveform,
    };
    use tutti_nodes::testing::Osc;
    use tutti_nodes::{Feed, ModHost, Modulation, SvfFilterConfig, SvfFilterNode, SvfType};
    use tutti_units::{
        Beat, Bipolar, Bpm, Hz, ParamAddr, PhaseIncrement, Q, Samples, Seconds, UnitParam,
    };

    struct Host {
        filter: NodeKey,
        saw: NodeKey,
    }
    impl ModHost for Host {
        fn node(&self, _: u64) -> Option<NodeKey> {
            Some(self.filter)
        }
        fn feed(&self, _: SourceKey) -> Option<Feed> {
            Some(Feed::Audio(self.saw))
        }
    }
    let at = |p| TargetKey::param(1, ParamAddr::Unit(p));
    let route = |s: u64, p, depth: f64| RouteDecl {
        source: SourceKey(s),
        to: RouteTarget::Param(at(p)),
        depth: tutti_rt::Param::new(Bipolar::of(depth)),
        polarity: Polarity::Bipolar,
        curve: Law::Linear,
    };

    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(128)),
    ));
    let (saw, filter) = (NodeKey(1 << 48), NodeKey((1 << 48) + 1));
    ed.insert(saw, "saw", ForkByClone(Osc::saw(Hz::new(110.0))));
    let params = ed
        .insert(
            filter,
            "svf",
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(1_000.0))
                    .q(Q::new(0.7)),
            ),
        )
        .controls;
    ed.connect_edge(EdgeId::into_port(
        InputPort::new(filter, 0),
        Edge::Direct(PortSource::Node(OutputPort::new(saw, 0))),
    ))
    .unwrap();
    ed.wire_outputs(vec![PortSource::Node(OutputPort::new(filter, 0))])
        .unwrap();
    ed.commit().expect("commits");
    let transport = TransportPosition::new(false, Bpm::of(120.0), Beat::new(0.0), None);
    let mut out = vec![0.0f32; 128];
    exec.process(128, &transport, &[], &mut [&mut out[..]]);
    ed.collect();

    let host = Host { filter, saw };
    let cell = |at: tutti_graph::ParamIn| params.cell(at.param);
    let mirror = |_: NodeKey, _: tutti_units::ParamId| None;
    let lfo = |rate: f64| {
        (
            SourceKey(1),
            ModSource::Lfo {
                wave: Waveform::Shape {
                    shape: tutti_modulation::LfoShape::Sine,
                    seed: 0,
                },
                clock: LayerClock::Seconds {
                    rate: Hz::new(rate),
                    phase: PhaseIncrement::new(0.0),
                    anchor: None,
                },
            },
        )
    };
    let mut sources = [
        lfo(2.0),
        (
            SourceKey(2),
            ModSource::Follower {
                attack: Seconds::new(0.001),
                release: Seconds::new(0.05),
            },
        ),
    ];
    let routes = [
        (RouteKey(1), route(1, UnitParam::Cutoff, 0.2)),
        (RouteKey(2), route(2, UnitParam::Q, 0.3)),
    ];
    let targets = [
        (
            at(UnitParam::Cutoff),
            TargetDecl {
                min: 100.0,
                max: 4_000.0,
                automation: None,
            },
        ),
        (
            at(UnitParam::Q),
            TargetDecl {
                min: 0.5,
                max: 4.0,
                automation: None,
            },
        ),
    ];
    let mut m = Modulation::new();
    let decl = Declarations {
        sources: &sources,
        routes: &routes,
        targets: &targets,
    };
    assert!(
        m.apply(&decl, &mut ed, &host, &cell, &mirror)
            .expect("applies")
            .commit
    );
    m.commit(&mut ed).expect("commits");
    // Warm-up outside the gate: the commit lands, the connects' declick
    // runs out.
    for _ in 0..8 {
        exec.process(128, &transport, &[], &mut [&mut out[..]]);
        ed.collect();
    }
    let mut sounded = false;
    for i in 0..40 {
        sources[0] = lfo(if i % 2 == 0 { 3.0 } else { 2.0 });
        let decl = Declarations {
            sources: &sources,
            routes: &routes,
            targets: &targets,
        };
        let applied = m
            .apply(&decl, &mut ed, &host, &cell, &mirror)
            .expect("applies");
        assert!(!applied.commit, "a rate edit sends no plan");
        assert_eq!(applied.republished.len(), 1, "the LFO's lane republished");
        for _ in 0..4 {
            exec.process(128, &transport, &[], &mut [&mut out[..]]);
        }
        // The next `apply`'s publish frees the retired program, here on the
        // control thread.
        ed.collect();
        sounded |= out.iter().any(|x| x.abs() > 1e-3);
    }
    assert!(sounded, "the filter rendered; the gate walked the graph");
}

/// A node through `Direct` at `rate`, its cells written by `between(i)` before block `i` and
/// its ports fed `signals` (one slice of 64 frames per port, `None` where it is not driven):
/// eight blocks of warm-up, then 256 blocks. Every sample of every
/// output channel over the 256 is finite and at most `bound` in magnitude.
fn corner<N: Node + ParamNode>(
    node: N,
    rate: SampleRate,
    signals: &[Option<&[f32]>],
    bound: f32,
    between: impl Fn(&ParamSet, usize),
) {
    let cells = node.param_set();
    let mut rig = Direct::new(node, rate, 64);
    let width = rig.inputs_mut().channels();
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { INPUT } else { -INPUT };
        }
    }
    for (k, signal) in signals.iter().enumerate() {
        rig.feed(k, *signal);
    }
    let mut heard = 0.0f32;
    for i in 0..8 {
        between(&cells, i);
        rig.block();
        heard += rig
            .output()
            .plane(0)
            .samples()
            .iter()
            .map(|s| s.abs())
            .sum::<f32>();
    }
    assert!(
        heard > 1e-6,
        "the node rendered silence; the gate would walk no DSP"
    );
    let (mut peak, mut finite) = (0.0f32, true);
    for i in 0..256 {
        between(&cells, i);
        rig.block();
        for c in 0..width {
            for s in rig.output().plane(c).samples() {
                finite &= s.is_finite();
                peak = peak.max(s.abs());
            }
        }
    }
    assert!(finite, "the output is finite");
    assert!(peak <= bound, "the output peaks at {peak}, past {bound}");
}

/// The corner sweeps' input: an alternating signal of this amplitude.
const INPUT: f32 = 0.5;

/// A guard on the delay's output, `INPUT / (1 - g)` times 1.25, `g` the greatest stable
/// feedback. For the convex reads (none, linear) `INPUT / (1 - g)` is a proof: a read's taps
/// sum to one in magnitude, so the loop's gain is at most `g` and its geometric series is
/// that. For the cubic Hermite read the taps sum in magnitude to at most 1.25, so the
/// loop's worst-case gain is `1.25 g`, which exceeds 1: the 1.25 is a steady-state guard
/// the sweep must stay under, not a proof of it.
fn loop_bound() -> f32 {
    INPUT * 1.25 / (1.0 - f32::from_unit(Feedback::MAX_STABLE))
}

/// A guard on the phaser's output, `INPUT / (1 - g)`: the peak gain of the frozen chain
/// (its `H-infinity`, at DC). Under a swept coefficient the chain is time-varying and this
/// is a guard the sweep must stay under, not a proof of it.
fn phaser_bound() -> f32 {
    INPUT / (1.0 - f32::from_unit(Feedback::MAX_STABLE))
}

/// A control signal of 64 frames cycling through `values`.
fn cycle(values: &[f32]) -> Vec<f32> {
    (0..64).map(|i| values[i % values.len()]).collect()
}

/// The delay at the corners of everything it is built to and told: each longest delay at
/// its extreme (a microsecond, and the 10 s cap at 48 kHz and at 192 kHz), each width (1, 2
/// and the 16 of a bus), every interpolation, every cell at both ends of its range, rewritten
/// every block, and the time, feedback and cross-feedback ports fed per frame with values at both ends and
/// past them (a delay of nothing, one past the line, one that is not a number).
#[test]
fn the_delay_corners_are_allocation_free() {
    let tiny = MaxDelay::of(Seconds::new(1e-6));
    let cap = MaxDelay::of(MaxDelay::CAP);
    let time = cycle(&[0.0, 1e9, f32::NAN, 10.0, -1.0, 1e-6]);
    let feedback = cycle(&[-1.0, 5.0, f32::NAN, 0.99, 0.0]);
    let cross = cycle(&[0.0, 1.0, f32::NAN, 0.5, -1.0]);
    for (width, max, rate) in [
        // One frame of line: a mono one, and a coupled stereo one.
        (1u16, tiny, SampleRate::SR_48K),
        (2, tiny, SampleRate::SR_48K),
        (2, cap, SampleRate::SR_192K),
        (16, cap, SampleRate::SR_48K),
    ] {
        for interpolation in [
            InterpolationMode::None,
            InterpolationMode::Linear,
            InterpolationMode::CubicHermite,
        ] {
            let node = DelayNode::<f32>::from(DelayConfig::new(
                DelayLayout::new(bus(width), max).interpolation(interpolation),
            ));
            let write = |cells: &ParamSet, i: usize| {
                assert!(cells.set(UnitParam::DelayTime, if i & 1 == 1 { 1e9 } else { 0.0 }));
                assert!(cells.set(UnitParam::Feedback, if i & 2 == 2 { 1.0 } else { 0.0 }));
                assert!(cells.set(UnitParam::CrossFeedback, if i & 4 == 4 { 1.0 } else { 0.0 }));
            };
            corner(node.clone(), rate, &[], loop_bound(), write);
            corner(
                node.clone(),
                rate,
                &[Some(&time[..]), Some(&feedback[..]), Some(&cross[..])],
                loop_bound(),
                write,
            );
        }
    }
    // Every channel of a bus recirculating into the next, each with a time of its own.
    let wide = bus(16);
    let layout = DelayLayout::builder(wide, cap)
        .interpolation(InterpolationMode::CubicHermite)
        .routing(FeedbackRouting::ring(wide))
        .times(ChannelTimes::try_from(&[Seconds::new(10.0); 15][..]).expect("fifteen times"))
        .build()
        .unwrap();
    corner(
        DelayNode::<f32>::from(DelayConfig::new(layout)),
        SampleRate::SR_48K,
        &[Some(&time[..]), Some(&feedback[..]), Some(&cross[..])],
        loop_bound(),
        |cells, i| {
            assert!(cells.set(UnitParam::Feedback, if i & 1 == 1 { 1.0 } else { 0.0 }));
            assert!(cells.set(UnitParam::CrossFeedback, if i & 2 == 2 { 1.0 } else { 0.0 }));
        },
    );
}

/// A tempo-synced delay: its times follow a tempo that moves every block, from a crawl to a
/// sprint, and the retarget and the ramps it aims allocate nothing. 400 blocks of the input
/// are run before the gate is armed, so the loop walks real DSP.
#[test]
fn the_synced_delay_following_a_moving_tempo_is_allocation_free_and_finite() {
    let notes = [
        NoteDuration::new(NoteValue::SIXTEENTH).triplet(),
        NoteDuration::new(NoteValue::of(64)).dotted(),
    ];
    let layout = DelayLayout::builder(ChannelTopology::STEREO, MaxDelay::of(MaxDelay::CAP))
        .interpolation(InterpolationMode::CubicHermite)
        .notes(NoteTimes::try_from(&notes[..]).expect("two notes"))
        .build()
        .unwrap();
    let node = DelayNode::<f32>::from(DelayConfig::new(layout).controls(DelayControls {
        sync: tutti_nodes::DelaySync::Synced,
        feedback: Feedback::new(0.5),
        cross_feedback: Feedback::new(0.25),
        ..DelayControls::DEFAULT
    }));
    let mut rig = Direct::new(node, SampleRate::SR_48K, 64);
    for c in rig.inputs_mut().iter_mut() {
        for (i, x) in c.into_samples_mut().iter_mut().enumerate() {
            *x = if i % 2 == 0 { INPUT } else { -INPUT };
        }
    }
    let tempo_at = |i: usize| Bpm::of(if i.is_multiple_of(2) { 20.0 } else { 300.0 });
    let mut heard = 0.0f32;
    for i in 0..400 {
        rig.set_transport(TransportPosition::new(
            true,
            tempo_at(i),
            Beat::new(0.0),
            None,
        ));
        rig.block();
        heard += rig
            .output()
            .plane(0)
            .samples()
            .iter()
            .map(|s| s.abs())
            .sum::<f32>();
    }
    assert!(heard > 1e-6, "the delay rendered silence");
    let (mut peak, mut finite) = (0.0f32, true);
    for i in 0..400 {
        rig.set_transport(TransportPosition::new(
            true,
            tempo_at(i),
            Beat::new(0.0),
            None,
        ));
        rig.block();
        for c in 0..2 {
            for s in rig.output().plane(c).samples() {
                finite &= s.is_finite();
                peak = peak.max(s.abs());
            }
        }
    }
    assert!(finite && peak <= loop_bound(), "peak {peak}");
}

/// The phaser at the corners of everything it is built to and told: the fewest and the most
/// stages, one channel and 16, the narrowest sweep and the widest (past Nyquist at the low
/// rate), the slowest and the fastest rate, both ends of the depth and the feedback in the
/// cells, rewritten every block, and the three ports fed per frame past their ranges.
#[test]
fn the_phaser_corners_are_allocation_free() {
    let rate_signal = cycle(&[-3.0, 0.0, 0.01, 400.0, f32::NAN, 20_000.0]);
    let depth_signal = cycle(&[-8.0, 8.0, f32::NAN, 0.0, 1.0]);
    let feedback_signal = cycle(&[-1.0, 5.0, f32::NAN, 0.99, 0.0]);
    for (width, stages, range, step, sample_rate) in [
        (
            1u16,
            2usize,
            SweepRange::of(Hz::new(20.0), Hz::new(20.0)),
            0.0,
            8_000.0,
        ),
        (
            16,
            12,
            SweepRange::of(Hz::new(20.0), Hz::new(40_000.0)),
            0.5,
            48_000.0,
        ),
        (2, 12, SweepRange::DEFAULT, 0.25, 192_000.0),
    ] {
        let node = PhaserNode::<f32>::try_from(
            PhaserConfig::new(bus(width), Stages::of(stages))
                .range(range)
                .stagger(Stagger::of_step(PhaseIncrement::new(step))),
        )
        .expect("a step fits any width");
        let rate = SampleRate::of(sample_rate);
        let write = |cells: &ParamSet, i: usize| {
            assert!(cells.set(UnitParam::Rate, if i & 1 == 1 { 400.0 } else { 0.01 }));
            assert!(cells.set(UnitParam::Depth, if i & 2 == 2 { 1.0 } else { -1.0 }));
            assert!(cells.set(UnitParam::Feedback, if i & 4 == 4 { 0.99 } else { 0.0 }));
        };
        corner(node.clone(), rate, &[], phaser_bound(), write);
        corner(
            node,
            rate,
            &[
                Some(&rate_signal[..]),
                Some(&depth_signal[..]),
                Some(&feedback_signal[..]),
            ],
            phaser_bound(),
            write,
        );
    }
}
