//! A control lane on a circular param (`Azimuth`): an automation curve
//! across ±180° turns the short way, and a modulated bearing is
//! `wrap(base + depth · arc length · m)`, never a clamp. A line param
//! (`Elevation`) on the same program is unchanged.
//!
//! Against closed forms written here, never another render.

use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_curve::{Breakpoint, Law, Trajectory};
use tutti_graph::{
    Cx, Editor, EventOutputPort, Executor, ForkByClone, Io, Node, NodeKey, ParamIn, ParamInput,
    ParamRange, Prepare, Shape, Status, TransportChanges, TransportPosition,
};
use tutti_graph::{OutputPort, PortSource};
use tutti_modulation::{
    Amount, ControlProgram, ControlProgramBuilder, Layer, LayerClock, LayerId, Polarity, Waveform,
};
use tutti_music::FrameClock;
use tutti_nodes::automation::{ControlLaneNode, UnitLaneConfig};
use tutti_units::{ArcDegrees, Azimuth, Beat, Bipolar, Bpm, SampleRate, Samples, UnitParam};

const RATE: f64 = 48_000.0;
const TEMPO: Bpm = Bpm::of(120.0);
/// 24 000 frames a beat.
const FPB: f64 = 24_000.0;

/// Declares `Azimuth` and `Elevation` (base 0); writes the value each port
/// reads, per frame, to outputs 0 and 1.
#[derive(Clone)]
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO)
            .params(&[UnitParam::Azimuth, UnitParam::Elevation])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for k in 0..2 {
            match io.param(k) {
                ParamInput::Base => io.output(k).fill(0.0),
                ParamInput::Frames { values, .. } => io.output(k).copy_from_slice(values),
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, _: usize) -> Option<f64> {
        Some(0.0)
    }
}

const PROBE: u64 = 2;

fn at(p: UnitParam) -> ParamIn {
    ParamIn {
        node: NodeKey(PROBE),
        param: p.into(),
    }
}

/// A probe with `azimuth` and `elevation` port ranges, committed.
fn rig(azimuth: ParamRange, elevation: ParamRange) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(2048)),
    ));
    ed.insert(NodeKey(PROBE), "probe", ForkByClone(Probe));
    for c in 0..2 {
        ed.spec_mut()
            .topology
            .outputs
            .push(vec![PortSource::Node(OutputPort::new(NodeKey(PROBE), c))]);
    }
    ed.set_param_range(at(UnitParam::Azimuth), azimuth)
        .expect("a probe");
    ed.set_param_range(at(UnitParam::Elevation), elevation)
        .expect("a probe");
    ed.commit().expect("commits");
    (ed, exec)
}

/// Wires a lane over `program` to `param` of the probe, with `knob` as its
/// base.
fn lane(ed: &mut Editor, key: u64, param: UnitParam, program: &ControlProgram, knob: f64) {
    let node = ControlLaneNode::from(
        UnitLaneConfig::new(param, program.clone())
            .knob(Arc::new(AtomicF64::new(knob)).into())
            .range(ed.param_range_cell(at(param)).expect("committed")),
    );
    ed.insert(NodeKey(key), "lane", node);
    ed.spec_mut()
        .connect_param(at(param), EventOutputPort::new(NodeKey(key), 0));
}

/// Rolls `blocks` blocks of `n` frames from `start`; each output's frames.
fn roll(
    ed: &mut Editor,
    exec: &mut Executor,
    start: f64,
    n: usize,
    blocks: usize,
) -> [Vec<f32>; 2] {
    let mut clock = FrameClock::new(Beat::new(start), TEMPO, SampleRate::of(RATE));
    let mut all = [Vec::new(), Vec::new()];
    for _ in 0..blocks {
        let t = TransportPosition::counted(true, TEMPO, clock.origin(), None);
        let mut bufs = [vec![0.0f32; n], vec![0.0f32; n]];
        {
            let [a, b] = &mut bufs;
            exec.process_with_changes(
                n,
                &t,
                &TransportChanges::NONE,
                &[],
                &mut [&mut a[..], &mut b[..]],
            );
        }
        ed.collect();
        clock.advance(Samples::new(n), None);
        for (o, b) in all.iter_mut().zip(bufs) {
            o.extend(b);
        }
    }
    all
}

/// A constant layer of `depth` of the range's span, positive.
fn constant(depth: f64) -> Layer {
    Layer {
        id: LayerId(1),
        clock: LayerClock::Seconds {
            rate: tutti_units::Hz::new(1.0),
            phase: tutti_units::PhaseIncrement::new(0.0),
            anchor: None,
        },
        wave: Waveform::Const(1.0),
        amount: Amount {
            depth: Bipolar::of(depth),
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
        },
        depth_by: None,
    }
}

/// A bearing of 170 modulated by +0.1 of the full circle (36 degrees) is 206,
/// which is -154 wrapped: not the clamp's 180.
#[test]
fn a_modulated_bearing_across_180_wraps_not_clamps() {
    let (mut ed, mut exec) = rig(ParamRange::UNBOUNDED, ParamRange::UNBOUNDED);
    let program = ControlProgramBuilder::new()
        .layers(vec![constant(0.1)])
        .build()
        .expect("valid");
    lane(&mut ed, 1, UnitParam::Azimuth, &program, 170.0);
    ed.commit().expect("commits");
    let [az, _] = roll(&mut ed, &mut exec, 0.0, 256, 3);
    let last = &az[512..];
    assert!(
        last.iter().all(|&v| (v + 154.0).abs() < 1e-3),
        "{:?}",
        &last[..4]
    );
}

/// The depth scales by the arc's length: 0.1 of a 40-degree arc is 4 degrees.
#[test]
fn a_bearings_modulation_depth_scales_by_the_arcs_length() {
    let arc = ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(20.0));
    let (mut ed, mut exec) = rig(arc, ParamRange::UNBOUNDED);
    let program = ControlProgramBuilder::new()
        .layers(vec![constant(0.1)])
        .build()
        .expect("valid");
    lane(&mut ed, 1, UnitParam::Azimuth, &program, 178.0);
    ed.commit().expect("commits");
    let [az, _] = roll(&mut ed, &mut exec, 0.0, 256, 3);
    // 178 + 0.1 * 40 = 182, inside 160..200, is 182 - 360 wrapped.
    assert!(
        az[512..].iter().all(|&v| (v + 178.0).abs() < 1e-3),
        "{:?}",
        &az[512..516]
    );
}

/// A line param on the same program is unchanged: elevation 80 plus 0.2 of
/// its 180-degree range is 116, clamped to the range's 90.
#[test]
fn a_modulated_height_is_clamped_as_ever() {
    let (mut ed, mut exec) = rig(ParamRange::UNBOUNDED, ParamRange::new(-90.0, 90.0));
    let program = ControlProgramBuilder::new()
        .layers(vec![constant(0.2)])
        .build()
        .expect("valid");
    lane(&mut ed, 1, UnitParam::Elevation, &program, 80.0);
    ed.commit().expect("commits");
    let [_, el] = roll(&mut ed, &mut exec, 0.0, 256, 3);
    assert!(el[512..].iter().all(|&v| v == 90.0), "{:?}", &el[512..516]);
    // And 0.05 of it (9) gives 89, a plain sum.
    let (mut ed, mut exec) = rig(ParamRange::UNBOUNDED, ParamRange::new(-90.0, 90.0));
    let program = ControlProgramBuilder::new()
        .layers(vec![constant(0.05)])
        .build()
        .expect("valid");
    lane(&mut ed, 1, UnitParam::Elevation, &program, 80.0);
    ed.commit().expect("commits");
    let [_, el] = roll(&mut ed, &mut exec, 0.0, 256, 3);
    assert!(el[512..].iter().all(|&v| (v - 89.0).abs() < 1e-3));
}

/// An automation curve from 170 at beat 0 to -170 at beat 4, with a layer of
/// no depth (so the lane samples the curve rather than sending its pieces).
fn seam_program() -> ControlProgram {
    let mut curve = Trajectory::new();
    curve.add_breakpoint(Breakpoint::new(Beat::new(0.0), Azimuth::new(170.0)));
    curve.add_breakpoint(Breakpoint::new(Beat::new(4.0), Azimuth::new(-170.0)));
    ControlProgramBuilder::new()
        .automation(Arc::new(curve))
        .layers(vec![constant(0.0)])
        .build()
        .expect("valid")
}

/// Rolling through beat 2, where the curve is at the seam, an azimuth lane
/// reads `wrap(170 + 5 b)` at every frame and a height lane `170 - 85 b`: the
/// bearing never nears 0, the height passes through it.
#[test]
fn an_automation_curve_across_180_turns_through_it_and_a_height_does_not() {
    let (mut ed, mut exec) = rig(ParamRange::UNBOUNDED, ParamRange::new(-200.0, 200.0));
    let program = seam_program();
    lane(&mut ed, 1, UnitParam::Azimuth, &program, 0.0);
    lane(&mut ed, 3, UnitParam::Elevation, &program, 0.0);
    ed.commit().expect("commits");
    // Beats 1.99 to 2.04: 1 200 frames.
    let [az, el] = roll(&mut ed, &mut exec, 1.99, 400, 3);
    let mut crossed = false;
    for (i, (&a, &e)) in az.iter().zip(&el).enumerate().skip(8) {
        let b = 1.99 + i as f64 / FPB;
        let want_az = Azimuth::new(170.0 + 5.0 * b).wrap().get();
        assert!(
            (f64::from(a) - want_az).abs() < 0.05,
            "frame {i}: {a} against {want_az}"
        );
        assert!(a.abs() > 179.0, "frame {i}: {a} left the seam");
        crossed |= a < 0.0;
        let want_el = 170.0 - 85.0 * b;
        assert!(
            (f64::from(e) - want_el).abs() < 0.05,
            "frame {i}: {e} against {want_el}"
        );
    }
    assert!(crossed, "the bearing crossed the seam");
    assert!(
        el.iter().any(|&e| e.abs() < 0.2),
        "the height passed through 0"
    );
}
