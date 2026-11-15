//! A circular param (`Azimuth`) on a param port:
//!
//! - a curve segment across ±180° turns through 180, never through 0, at
//!   every frame of the closed form `wrap(from + arc · shape)`;
//! - the port wraps a value onto the circle rather than clamping it, and
//!   holds it to a bounded arc (`ParamRange::arc`) by the short way round;
//! - a line param (`Elevation`) is unchanged: the same ends pass through 0
//!   and a value past the bound is clamped;
//! - a bounded range of a circular param written crossed is refused.

use tutti_graph::testing as common;
use tutti_graph::{
    CommitError, Cx, Editor, Event, EventOut, Executor, FadeLaw, GraphInvalid, Io, Law, Node,
    NodeKey, ParamIn, ParamInput, ParamRange, ParamSegment, Prepare, Shape, Status,
    TransportPosition, Unforkable,
};
use tutti_graph::{OutPort, Source};
use tutti_types::{ArcDegrees, Azimuth, ChannelLayout, MaxBlock, SampleRate, Samples, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;
const PROBE: u64 = 10;
const SENDER: u64 = 1;

/// Declares `Azimuth` and `Elevation`; writes each one's value at every frame
/// to output 0 and 1 (its base, `.0`, where nothing drives it).
#[derive(Clone)]
struct Probe(f32);

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::buses(
            tutti_graph::Buses::unassigned(ChannelLayout::MONO),
            tutti_graph::Buses::unassigned(ChannelLayout::MONO),
        )
        .with_ports(ChannelLayout::MONO, ChannelLayout::from_count(2))
        .with_params(&[UnitParam::Azimuth, UnitParam::Elevation])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let spare = self.shape().spare_out().expect("one readout");
        let base = self.0;
        let write = |out: &mut [f32], p: ParamInput<'_>| match p {
            ParamInput::Base => out.fill(base),
            ParamInput::Frames { values, .. } => out.copy_from_slice(values),
        };
        let (a, e) = (io.param(0), io.param(1));
        write(io.output(0), a);
        write(io.bus_out(spare).get(0), e);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, _: usize) -> Option<f32> {
        Some(self.0)
    }
}

/// Sends each `(absolute frame, segment)` on its frame.
#[derive(Clone)]
struct Sender(Vec<(u64, ParamSegment)>);

impl Node for Sender {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(16)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for &(at, seg) in &self.0 {
            if at >= start && at < start + io.frames() as u64 {
                let o = io.offset((at - start) as usize).expect("inside");
                let _ = io.event_out(0).push(Event::segment(o, seg));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn at(p: UnitParam) -> ParamIn {
    ParamIn {
        node: NodeKey(PROBE),
        param: p.into(),
    }
}

/// A graph with the probe, driven by `sends`, with each param's `range`.
fn graph(
    sends: Vec<(u64, ParamSegment)>,
    ranges: &[(UnitParam, ParamRange)],
) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(RATE, MaxBlock::of(Samples(256))));
    ed.insert(NodeKey(PROBE), "probe", Unforkable(Probe(0.0)));
    ed.insert(NodeKey(SENDER), "sender", Unforkable(Sender(sends)));
    let s = ed.spec_mut();
    s.topology.set_outputs((0..2).map(|c| {
        Source::Node(OutPort {
            node: NodeKey(PROBE),
            port: c,
        })
    }));
    for p in [UnitParam::Azimuth, UnitParam::Elevation] {
        s.connect_param(
            at(p),
            EventOut {
                node: NodeKey(SENDER),
                port: 0,
            },
        );
    }
    for &(p, r) in ranges {
        s.set_param_range(at(p), r);
    }
    (ed, exec)
}

/// `frames` frames in one block: the two outputs.
fn render(ed: &mut Editor, exec: &mut Executor, frames: usize) -> [Vec<f32>; 2] {
    let mut a = vec![0.0; frames];
    let mut b = vec![0.0; frames];
    exec.process(
        frames,
        &TransportPosition::default(),
        &[],
        &mut [&mut a[..], &mut b[..]],
    );
    ed.collect();
    [a, b]
}

/// One sender of the same segment to both params (their raw values), at frame 0.
fn both(from: f32, to: f32, len: usize, shape: Law) -> Vec<(u64, ParamSegment)> {
    [UnitParam::Azimuth, UnitParam::Elevation]
        .into_iter()
        .map(|p| (0, ParamSegment::unit(p, from, to, Samples(len), shape)))
        .collect()
}

/// A bearing segment from 170 to -170 reads `wrap(170 + 20 t)` at every frame,
/// passing through ±180 at its midpoint and never within 169 degrees of 0.
#[test]
fn an_azimuth_segment_from_170_to_minus_170_turns_through_180_not_0() {
    let (mut ed, mut exec) = graph(both(170.0, -170.0, 100, Law::Linear), &[]);
    ed.commit().expect("commits");
    let [az, _] = render(&mut ed, &mut exec, 120);
    for (j, &v) in az.iter().enumerate().take(99) {
        // `position` 1: frame j is `(1 + j) / 100` of the way.
        let t = (1 + j) as f64 / 100.0;
        let want = Azimuth((170.0 + 20.0 * t) as f32).wrap().get();
        assert!((v - want).abs() < 1e-3, "frame {j}: {v} against {want}");
        assert!(v.abs() >= 169.0, "frame {j}: {v} went the long way");
    }
    assert_eq!(az[49], -180.0, "the midpoint is the seam");
    assert_eq!(az[99], -170.0, "it lands on its target");
    assert!(az[119] == -170.0, "and holds it");
}

/// A shaped segment: the shape is applied to the arc, not to the raw gap.
#[test]
fn an_azimuth_segment_applies_its_shape_to_the_arc() {
    let (mut ed, mut exec) = graph(both(-160.0, 160.0, 100, Law::Smootherstep), &[]);
    ed.commit().expect("commits");
    let [az, _] = render(&mut ed, &mut exec, 100);
    for (j, &v) in az.iter().enumerate().take(99) {
        let t = (1 + j) as f64 / 100.0;
        let g = t * t * t * (t * (6.0 * t - 15.0) + 10.0);
        // The short arc from -160 to 160 is -40.
        let want = Azimuth((-160.0 - 40.0 * g) as f32).wrap().get();
        assert!((v - want).abs() < 1e-3, "frame {j}: {v} against {want}");
    }
}

/// A line param over the same ends is unchanged: elevation passes through 0.
#[test]
fn an_elevation_segment_between_the_same_ends_passes_through_0() {
    let (mut ed, mut exec) = graph(both(170.0, -170.0, 100, Law::Linear), &[]);
    ed.commit().expect("commits");
    let [_, el] = render(&mut ed, &mut exec, 100);
    for (j, &v) in el.iter().enumerate().take(99) {
        let want = 170.0 - 340.0 * ((1 + j) as f64 / 100.0);
        assert!(
            (f64::from(v) - want).abs() < 1e-3,
            "frame {j}: {v} against {want}"
        );
    }
    assert!(el[49].abs() < 1e-3, "the midpoint is 0, got {}", el[49]);
}

/// The port wraps a bearing past 180 onto the circle, where a line clamps it.
#[test]
fn the_port_wraps_a_bearing_rather_than_clamping_it() {
    let both_step = |v: f32| {
        [UnitParam::Azimuth, UnitParam::Elevation]
            .into_iter()
            .map(|p| (0, ParamSegment::unit(p, v, v, Samples(0), Law::Linear)))
            .collect::<Vec<_>>()
    };
    let (mut ed, mut exec) = graph(
        both_step(190.0),
        &[(UnitParam::Elevation, ParamRange::new(-90.0, 90.0))],
    );
    ed.commit().expect("commits");
    let [az, el] = render(&mut ed, &mut exec, 8);
    assert!(az.iter().all(|&v| v == -170.0), "190 is -170: {az:?}");
    assert!(el.iter().all(|&v| v == 90.0), "a height clamps: {el:?}");
}

/// A bounded arc across ±180 holds a value inside it as it is and an outside
/// one to the nearer end, by the short way round.
#[test]
fn the_port_holds_a_bearing_to_an_arc_across_180() {
    let behind = ParamRange::arc(Azimuth(180.0), ArcDegrees(20.0));
    // 195 is -165, inside 160..200: not clamped to a line's 200 or 160.
    // 100 is outside, nearer the 160 end; -100 the 200 (-160) end.
    for (v, want) in [
        (195.0, -165.0),
        (100.0, 160.0),
        (-100.0, -160.0),
        (180.0, -180.0),
    ] {
        let (mut ed, mut exec) = graph(
            vec![(
                0,
                ParamSegment::unit(UnitParam::Azimuth, v, v, Samples(0), Law::Linear),
            )],
            &[(UnitParam::Azimuth, behind)],
        );
        ed.commit().expect("commits");
        let [az, _] = render(&mut ed, &mut exec, 4);
        assert!(az.iter().all(|&x| x == want), "{v}: {az:?} against {want}");
    }
}

/// Crossed bounds on a circular param read the opposite arc once ordered, so
/// they are refused; the same on a line is ordered as ever, and `arc` is the
/// way to bound a bearing.
#[test]
fn a_crossed_range_on_a_circular_param_is_refused() {
    let (mut ed, _exec) = graph(Vec::new(), &[]);
    let crossed = ParamRange::new(170.0, -170.0);
    ed.spec_mut()
        .set_param_range(at(UnitParam::Azimuth), crossed);
    match ed.commit() {
        Err(CommitError::Invalid(errs)) => assert!(
            errs.iter()
                .any(|e| matches!(e, GraphInvalid::BadParamRange { at: a, .. }
                if *a == at(UnitParam::Azimuth))),
            "{errs:?}"
        ),
        other => panic!("a crossed arc was not refused: {other:?}"),
    }
    assert!(matches!(
        ed.set_param_range(at(UnitParam::Azimuth), crossed),
        Err(GraphInvalid::BadParamRange { .. })
    ));
    ed.spec_mut().set_param_range(
        at(UnitParam::Azimuth),
        ParamRange::arc(Azimuth(180.0), ArcDegrees(10.0)),
    );
    ed.spec_mut()
        .set_param_range(at(UnitParam::Elevation), crossed);
    ed.commit()
        .expect("an arc, and a line's crossed bounds, commit");
}

/// The bearing a port reads through its declick: a probe whose base is
/// `base` runs one block alone, then a source that steps to `target` on the
/// next block's first frame is connected, so the port fades from the base to
/// the source over the declick. The next block's frames, on `workers`
/// participants (1: the serial executor).
fn declick(base: f32, target: f32, workers: usize) -> Vec<f32> {
    let (mut ed, mut exec) = Editor::new(Prepare::new(RATE, MaxBlock::of(Samples(256))));
    if workers > 1 {
        exec.set_pool(Some(common::TestPool::new(workers)));
    }
    ed.insert(NodeKey(PROBE), "probe", Unforkable(Probe(base)));
    let step = ParamSegment::unit(UnitParam::Azimuth, target, target, Samples(0), Law::Linear);
    ed.insert(
        NodeKey(SENDER),
        "sender",
        Unforkable(Sender(vec![(64, step)])),
    );
    ed.spec_mut().topology.set_outputs([Source::Node(OutPort {
        node: NodeKey(PROBE),
        port: 0,
    })]);
    ed.commit().expect("commits");
    let mut one = |frames: usize| {
        let mut out = vec![0.0; frames];
        exec.process(
            frames,
            &TransportPosition::default(),
            &[],
            &mut [&mut out[..]],
        );
        ed.collect();
        out
    };
    assert!(
        one(64).iter().all(|&v| v == base),
        "undriven reads the base"
    );
    drop(one);
    ed.spec_mut().connect_param(
        at(UnitParam::Azimuth),
        EventOut {
            node: NodeKey(SENDER),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    let mut out = vec![0.0; 256];
    exec.process(256, &TransportPosition::default(), &[], &mut [&mut out[..]]);
    out
}

/// A bearing's declick between 170 and -170 (and back) crosses at 180 along the
/// short arc, `wrap(hold + arc · gain)`: no frame of the fade is near 0 as an
/// equal-amplitude blend of the two bearings would be, and the pool's render
/// is the serial one bit for bit.
#[test]
fn a_bearings_declick_crosses_the_seam_and_the_pool_agrees_bit_for_bit() {
    let d = tutti_graph::declick_frames(RATE).get();
    for (base, target) in [(170.0f32, -170.0f32), (-170.0, 170.0)] {
        let serial = declick(base, target, 1);
        let pooled = declick(base, target, 4);
        assert_eq!(
            serial.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            pooled.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "{base} to {target}: the pool differs from the serial render"
        );
        let arc = UnitParam::Azimuth.arc(base, target);
        assert_eq!(arc.abs(), 20.0);
        for (j, &v) in serial.iter().enumerate().take(d) {
            let (_, inn) = FadeLaw::EqualAmplitude.gains(j, d);
            let want = UnitParam::Azimuth.wrap(base + arc * inn);
            assert!((v - want).abs() < 1e-3, "frame {j}: {v} against {want}");
            assert!(v.abs() > 169.0, "frame {j}: {v} went through the middle");
        }
        let fade = &serial[..d];
        assert!(
            fade.iter().any(|&v| v > 179.0) && fade.iter().any(|&v| v < -179.0),
            "{base} to {target}: the fade crossed the seam"
        );
        assert_eq!(serial[d], target, "and lands on the source");
    }
}
