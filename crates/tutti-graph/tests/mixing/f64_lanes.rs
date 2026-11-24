//! A param lane carries the graph's float: in an `f64` graph a driven param
//! reaches its node as `&[f64]`, the port ramps in `f64`, and the range clamps
//! in `f64`; the edge-weight and mix lanes ramp in `f64`. A param event's ramp
//! or segment is rendered from its `f64` payload.

use tutti_buffers::Float;
use tutti_graph::EdgeId;
use tutti_graph::testing as common;

use common::prepare;
use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Cx, Editor, EventOutputPort, Executor, ForkByClone, Io, Node, NodeKey, OutputPort, ParamEvent,
    ParamIn, ParamNode, ParamRamp, ParamRange, ParamSegment, ParamSet, Params, PortSource, Prepare,
    Shape, Status, declare_params,
};
use tutti_rt::Param;
use tutti_units::{Azimuth, Hz, ParamKey, Samples, UnitParam};

/// One part in 2^40 above one: past `f32`'s 24-bit significand, inside `f64`'s 53.
const FINE: f64 = 1.0 + 1.0 / 1_099_511_627_776.0;

/// The lane node's base: a quarter, exact in every float.
const BASE: f64 = 0.25;

/// Writes its `Cutoff` lane (its base where nothing drives it) to its one
/// output, in the graph's float.
#[derive(Clone)]
struct Lane(Param<Hz>);

declare_params!([] Lane, n { Cutoff => Hz = &n.0 });

impl<F: Float> Node<F> for Lane {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let lane = self.declared_param::<{ UnitParam::Cutoff as u16 }, _>(&io);
        let base = F::from_f64(self.0.load().get());
        match lane.frames() {
            Some(values) => io.output(0).copy_from_slice(values),
            None => io.output(0).fill(base),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for Lane {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.0.detach();
        fork
    }
}

/// Starts a `ParamRamp` of `Cutoff` to `target` over `len` frames at `at`, in
/// the first block.
#[derive(Clone)]
struct Ramp {
    at: usize,
    target: f64,
    len: usize,
}

impl Ramp {
    const SHAPE: Shape = Shape::EMPTY
        .events(0, 1)
        .param_events()
        .event_capacity(tutti_graph::testing::cap_of(4));
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl<F: Float> Node<F> for Ramp {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        if cx.env.frame.get() == 0 {
            let offset = io.offset(self.at).expect("inside the block");
            let ramp = ParamRamp::new(
                ParamKey::<Hz>::CUTOFF,
                Hz::new(self.target),
                Samples::new(self.len),
            );
            let _ = io
                .event_out(Self::OUT)
                .push_param(ParamEvent::ramp(offset, ramp));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

const LANE: NodeKey = NodeKey(1);
const SRC: NodeKey = NodeKey(2);
const FRAMES: usize = 16;

/// Renders one block of `Lane` (base 0.25) in an `f64` graph, its `Cutoff`
/// driven by `ramp` and held to `range`.
fn render(ramp: Ramp, range: Option<ParamRange>) -> Vec<f64> {
    let (mut ed, mut exec): (Editor<f64>, Executor<f64>) = Editor::with_format(prepare(FRAMES));
    ed.insert(LANE, "lane", Params(Lane(Param::new(Hz::new(BASE)))));
    ed.insert(SRC, "ramp", ForkByClone(ramp));
    let at = ParamIn {
        node: LANE,
        param: UnitParam::Cutoff.into(),
    };
    ed.wire_outputs([PortSource::Node(OutputPort::new(LANE, 0))])
        .unwrap();
    ed.connect_param(at, EventOutputPort::new(SRC, 0)).unwrap();
    if let Some(range) = range {
        ed.set_param_range(at, range).expect("a range");
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    let out = common::block(&mut exec, FRAMES, 1).remove(0);
    ed.collect();
    out
}

/// The ramp's analytic value at its `k`th frame (1-based): `BASE + (target -
/// BASE) * k / len`.
fn analytic(target: f64, len: usize, k: usize) -> f64 {
    BASE + (target - BASE) * (k as f64 / len as f64)
}

/// **A ramp in an `f64` graph moves along `base + (target - base) * k / len`
/// in `f64`.** With `len` 3 the progress `k / 3` is a figure `f32` cannot
/// hold; the lane holds the base before the ramp and lands on the target on
/// the ramp's last frame.
#[test]
fn an_f64_lane_ramps_in_f64() {
    assert_ne!(
        f64::from((1.0f64 / 3.0) as f32),
        1.0 / 3.0,
        "f32 loses a third"
    );
    let (at, len, target) = (4, 3, 3.0);
    let out = render(Ramp { at, target, len }, None);
    for (frame, &v) in out.iter().enumerate().take(at) {
        assert_eq!(v, BASE, "frame {frame}: the base, before the ramp");
    }
    for k in 1..=len {
        let (got, want) = (out[at + k - 1], analytic(target, len, k));
        assert!(
            (got - want).abs() <= 4.0 * f64::EPSILON,
            "frame {}: {got} vs {want}",
            at + k - 1
        );
    }
    assert_ne!(
        out[at],
        f64::from(out[at] as f32),
        "the first step is past f32"
    );
    assert_eq!(out[at + len - 1], target, "the ramp lands on its target");
}

/// **A range clamps an `f64` lane in `f64`.** The range `0..=2` holds a ramp
/// to 3 at its upper bound, and the frame below it keeps its `f64` value.
#[test]
fn a_range_clamps_an_f64_lane_in_f64() {
    let (at, len, target, hi) = (4, 3, 3.0, 2.0);
    let out = render(Ramp { at, target, len }, Some(ParamRange::of(0.0, hi)));
    for k in 1..=len {
        let (got, want) = (out[at + k - 1], analytic(target, len, k).min(hi));
        assert!(
            (got - want).abs() <= 4.0 * f64::EPSILON,
            "frame {}: {got} vs {want}",
            at + k - 1
        );
    }
    assert_eq!(out[at + len - 1], hi, "the last frame is held at the bound");
    assert_eq!(out[0], BASE, "before the ramp the base is inside the range");
}

/// **A range bound past `f32` clamps exactly in `f64`.** The upper bound is
/// `2 + 2^-40`: `f32` rounds it to `2`, so a clamp through an `f32` word would
/// hold the ramp at `2`. The ramp to 3 is held at the exact bound, and so is
/// the lower bound `-(2 + 2^-40)` of a ramp down.
#[test]
fn a_range_bound_f32_cannot_hold_clamps_exactly() {
    let hi = 2.0 + 1.0 / 1_099_511_627_776.0;
    assert_ne!(f64::from(hi as f32), hi, "f32 loses the bound");
    let (at, len) = (4, 3);
    let up = render(
        Ramp {
            at,
            target: 3.0,
            len,
        },
        Some(ParamRange::of(0.0, hi)),
    );
    assert_eq!(up[at + len - 1], hi, "held at the exact upper bound");
    assert!(up[at + len - 1] > 2.0, "past the bound f32 would hold");
    let down = render(
        Ramp {
            at,
            target: -3.0,
            len,
        },
        Some(ParamRange::of(-hi, 1.0)),
    );
    assert_eq!(down[at + len - 1], -hi, "held at the exact lower bound");
}

/// **A range moved between blocks is the next block's, whole.** The ramp
/// holds its target (3) in every block; the clamp each block reads is the
/// bound published before it, both ends from one publish, and a move that
/// widens releases the value to the ramp's target again.
#[test]
fn a_range_change_lands_on_the_next_block() {
    let (mut ed, mut exec): (Editor<f64>, Executor<f64>) = Editor::with_format(prepare(FRAMES));
    ed.insert(LANE, "lane", Params(Lane(Param::new(Hz::new(BASE)))));
    ed.insert(
        SRC,
        "ramp",
        ForkByClone(Ramp {
            at: 0,
            target: 3.0,
            len: 1,
        }),
    );
    let at = ParamIn {
        node: LANE,
        param: UnitParam::Cutoff.into(),
    };
    ed.wire_outputs([PortSource::Node(OutputPort::new(LANE, 0))])
        .unwrap();
    ed.connect_param(at, EventOutputPort::new(SRC, 0)).unwrap();
    ed.commit().expect("commits");
    exec.apply_pending();
    let mut block = |ed: &mut Editor<f64>| {
        let out = common::block(&mut exec, FRAMES, 1).remove(0);
        ed.collect();
        out
    };
    let last = |out: &[f64]| out[FRAMES - 1];
    assert_eq!(last(&block(&mut ed)), 3.0, "unbounded: the ramp's target");
    for (lo, hi) in [(0.0, 2.0), (-1.0, 1.5), (0.0, 2.5), (0.5, 4.0)] {
        ed.set_param_range(at, ParamRange::of(lo, hi))
            .expect("a range");
        assert_eq!(
            last(&block(&mut ed)),
            3.0_f64.clamp(lo, hi),
            "{lo}..{hi} lands on the next block"
        );
        assert_eq!(ed.reclaim(), 0, "no block holds a retired range");
    }
}

/// A constant source in any float.
#[derive(Clone)]
struct Dc(f64);

impl<F: Float> Node<F> for Dc {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        io.output(0).fill(F::from_f64(self.0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Doubles its input, in any float.
#[derive(Clone)]
struct Double;

impl<F: Float> Node<F> for Double {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, &x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = x + x;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn block(exec: &mut Executor<f64>, ed: &mut Editor<f64>) -> Vec<f64> {
    let out = common::block(exec, FRAMES, 1).remove(0);
    ed.collect();
    out
}

/// **An edge weight moved between blocks ramps in `f64`.** `FINE` through a
/// weight that goes from 0.5 to 1.5 is `FINE * (0.5 + (k + 1) / n)` at frame
/// `k` of the next block, to `f64` rounding.
#[test]
fn an_f64_edge_weight_ramps_in_f64() {
    use tutti_graph::EdgeId;
    use tutti_units::Gain;
    let (mut ed, mut exec): (Editor<f64>, Executor<f64>) = Editor::with_format(prepare(FRAMES));
    ed.insert(LANE, "dc", ForkByClone(Dc(FINE)));
    let from = PortSource::Node(OutputPort::new(LANE, 0));
    ed.wire_outputs([from]).unwrap();
    let id = EdgeId::into_output(0, from);
    ed.set_edge_gain(id, Gain::new(0.5)).expect("an edge");
    ed.commit().expect("commits");
    exec.apply_pending();
    let first = block(&mut exec, &mut ed);
    assert_eq!(first[FRAMES - 1], FINE * 0.5, "a steady weight");
    ed.set_edge_gain(id, Gain::new(1.5)).expect("an edge");
    let second = block(&mut exec, &mut ed);
    for (k, &v) in second.iter().enumerate() {
        let want = FINE * (0.5 + (k + 1) as f64 / FRAMES as f64);
        assert!(
            (v - want).abs() <= 4.0 * f64::EPSILON * want,
            "frame {k}: {v} vs {want}"
        );
    }
}

/// **A node's mix moved between blocks ramps in `f64`.** A doubler on `FINE`
/// at mix `m` is `2 FINE m + FINE (1 - m)`; `m` ramps from 1 to 1/4.
#[test]
fn an_f64_mix_ramps_in_f64() {
    use tutti_units::Mix;
    let (mut ed, mut exec): (Editor<f64>, Executor<f64>) = Editor::with_format(prepare(FRAMES));
    ed.insert(NodeKey(5), "dc", ForkByClone(Dc(FINE)));
    ed.insert(LANE, "double", ForkByClone(Double));
    ed.connect_edge(EdgeId::into_port(
        tutti_graph::InputPort::new(LANE, 0),
        tutti_graph::Edge::Direct(PortSource::Node(OutputPort::new(NodeKey(5), 0))),
    ))
    .unwrap();
    ed.wire_outputs([PortSource::Node(OutputPort::new(LANE, 0))])
        .unwrap();
    ed.commit().expect("commits");
    exec.apply_pending();
    let first = block(&mut exec, &mut ed);
    assert_eq!(first[FRAMES - 1], 2.0 * FINE, "wet");
    ed.set_mix(LANE, Mix::new(0.25)).expect("a mix");
    let second = block(&mut exec, &mut ed);
    for (k, &v) in second.iter().enumerate() {
        let m = 1.0 + (0.25 - 1.0) * ((k + 1) as f64 / FRAMES as f64);
        let want = 2.0 * FINE * m + FINE * (1.0 - m);
        assert!(
            (v - want).abs() <= 8.0 * f64::EPSILON * want,
            "frame {k}: {v} vs {want}"
        );
    }
}

/// Starts a [`ParamSegment`] at the first frame of the first block.
#[derive(Clone)]
struct Segment(ParamSegment);

impl Segment {
    const SHAPE: Shape = Shape::EMPTY
        .events(0, 1)
        .param_events()
        .event_capacity(tutti_graph::testing::cap_of(4));
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl<F: Float> Node<F> for Segment {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        if cx.env.frame.get() == 0 {
            let offset = io.offset(0).expect("inside the block");
            let _ = io
                .event_out(Self::OUT)
                .push_param(ParamEvent::segment(offset, self.0));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Writes its `Azimuth` lane (zero where nothing drives it) to its one output.
#[derive(Clone)]
struct Bearing;

impl Node<f64> for Bearing {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).params(&[UnitParam::Azimuth])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, f64>) -> Status {
        match io.param(0).frames() {
            Some(values) => io.output(0).copy_from_slice(values),
            None => io.output(0).fill(0.0),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, _: usize) -> Option<f64> {
        Some(0.0)
    }
}

/// One `f64` block of `lane` with `segment` driving its param `param`.
fn render_segment(
    lane: impl tutti_graph::IntoNode<f64>,
    param: UnitParam,
    segment: ParamSegment,
) -> Vec<f64> {
    let (mut ed, mut exec): (Editor<f64>, Executor<f64>) = Editor::with_format(prepare(FRAMES));
    ed.insert(LANE, "lane", lane);
    ed.insert(SRC, "segment", ForkByClone(Segment(segment)));
    ed.wire_outputs([PortSource::Node(OutputPort::new(LANE, 0))])
        .unwrap();
    ed.connect_param(
        ParamIn {
            node: LANE,
            param: param.into(),
        },
        EventOutputPort::new(SRC, 0),
    )
    .unwrap();
    ed.commit().expect("commits");
    exec.apply_pending();
    block(&mut exec, &mut ed)
}

/// **A segment whose endpoints `f32` cannot hold renders exactly in `f64`.**
/// From `FINE` to just under 3, linear over four frames: every frame is
/// `from + (to - from) * (k / 4)` to the bit, and the end is the end.
#[test]
fn a_segment_f32_cannot_hold_renders_exactly_in_f64() {
    let (from, to) = (FINE, 3.0 - 1.0 / 1_099_511_627_776.0);
    assert_ne!(f64::from(from as f32), from, "f32 loses the start");
    assert_ne!(f64::from(to as f32), to, "f32 loses the end");
    let seg = ParamSegment::new(
        ParamKey::<Hz>::CUTOFF,
        Hz::new(from),
        Hz::new(to),
        Samples::new(4),
    );
    let out = render_segment(
        Params(Lane(Param::new(Hz::new(BASE)))),
        UnitParam::Cutoff,
        seg,
    );
    for k in 1..4 {
        assert_eq!(
            out[k - 1],
            from + (to - from) * (k as f64 / 4.0),
            "frame {}",
            k - 1
        );
    }
    assert_eq!(out[3], to, "it lands on its end");
    assert_eq!(out[15], to, "and holds it");
}

/// **A ten-minute segment advances one frame per frame at its end.** A ramp
/// of one Hz a frame over 2.88e7 frames (ten minutes at 48 kHz, past the 2^24
/// frames an `f32` count holds), entered 16 frames from its end: each frame
/// is one more than the last. A progress in `f32` would step by two there.
#[test]
fn a_ten_minute_segment_steps_one_frame_per_frame_at_its_end() {
    const LEN: usize = 28_800_000;
    let odd = LEN - 1;
    assert_ne!(f64::from(odd as f32), odd as f64, "f32 cannot count there");
    let seg = ParamSegment::new(
        ParamKey::<Hz>::CUTOFF,
        Hz::new(0.0),
        Hz::new(LEN as f64),
        Samples::new(LEN),
    )
    .resumed(Samples::new(LEN - 16), Samples::new(LEN));
    let out = render_segment(
        Params(Lane(Param::new(Hz::new(BASE)))),
        UnitParam::Cutoff,
        seg,
    );
    for (j, pair) in out.windows(2).enumerate() {
        assert!(
            (pair[1] - pair[0] - 1.0).abs() < 1e-7,
            "frame {}: {} then {}",
            j,
            pair[0],
            pair[1]
        );
    }
    assert!((out[0] - (LEN - 16) as f64).abs() < 1e-7, "{}", out[0]);
}

/// **A circular ramp across the seam takes the short way at `f64`.** From
/// 180 less 1e-8 to -180 plus 1e-8: `f32` rounds both ends to ±180 and has no
/// arc at all, where `f64` has one of 2e-8 through 180, and every frame is
/// `wrap(from + arc * k / 4)` to the bit, none anywhere near 0.
#[test]
fn a_circular_segment_across_the_seam_takes_the_short_way_in_f64() {
    let (from, to) = (180.0 - 1e-8, -180.0 + 1e-8);
    assert_eq!(from as f32, 180.0, "f32 rounds the start onto the seam");
    assert_eq!(to as f32, -180.0, "and the end");
    let seg = ParamSegment::unit(UnitParam::Azimuth, from, to, Samples::new(4));
    let out = render_segment(ForkByClone(Bearing), UnitParam::Azimuth, seg);
    let arc = UnitParam::Azimuth.arc(from, to);
    assert!((arc - 2e-8).abs() < 1e-12, "the short arc is {arc}");
    for k in 1..4 {
        let want = Azimuth::new(from + arc * (k as f64 / 4.0)).wrap().get();
        assert_eq!(out[k - 1], want, "frame {}", k - 1);
        assert!(out[k - 1].abs() > 179.0, "frame {}: {}", k - 1, out[k - 1]);
    }
    assert_eq!(out[3], to, "it lands on its end");
}
