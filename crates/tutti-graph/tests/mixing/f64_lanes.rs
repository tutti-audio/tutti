//! A param lane carries the graph's float: in an `f64` graph a driven param
//! reaches its node as `&[f64]`, the port ramps in `f64`, and the range clamps
//! in `f64`; the edge-weight and mix lanes ramp in `f64`. The lane's event
//! words (a ramp's target) are `f32` by spec.

use tutti_buffers::Float;
use tutti_graph::testing as common;

use common::prepare;
use tutti_graph::{
    Cx, Editor, Event, EventOut, Executor, ForkByClone, Io, Node, NodeKey, OutPort, ParamIn,
    ParamNode, ParamRamp, ParamRange, ParamSet, Params, Prepare, Shape, Source, Status,
    TransportPosition, declare_params,
};
use tutti_types::{ChannelLayout, Param};
use tutti_units::{Hz, ParamKey, Samples, UnitParam};

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
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_param_addrs(self.declared_ports().as_slice())
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

impl<F: Float> Node<F> for Ramp {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(tutti_graph::testing::cap_of(4))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        if cx.env.frame.get() == 0 {
            let offset = io.offset(self.at).expect("inside the block");
            let ramp = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(self.target), Samples(self.len));
            let _ = io.event_out(0).push(Event::ramp(offset, ramp));
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
    ed.insert(LANE, "lane", Params(Lane(Param::new(Hz(BASE)))));
    ed.insert(SRC, "ramp", ForkByClone(ramp));
    let at = ParamIn {
        node: LANE,
        param: UnitParam::Cutoff.into(),
    };
    ed.spec_mut().topology.set_outputs([Source::Node(OutPort {
        node: LANE,
        port: 0,
    })]);
    ed.spec_mut()
        .connect_param(at, EventOut { node: SRC, port: 0 });
    if let Some(range) = range {
        ed.spec_mut().set_param_range(at, range);
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    let mut out = vec![0.0f64; FRAMES];
    exec.process(
        FRAMES,
        &TransportPosition::default(),
        &[],
        &mut [&mut out[..]],
    );
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
    let out = render(Ramp { at, target, len }, Some(ParamRange::new(0.0, hi)));
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
        Some(ParamRange::new(0.0, hi)),
    );
    assert_eq!(up[at + len - 1], hi, "held at the exact upper bound");
    assert!(up[at + len - 1] > 2.0, "past the bound f32 would hold");
    let down = render(
        Ramp {
            at,
            target: -3.0,
            len,
        },
        Some(ParamRange::new(-hi, 1.0)),
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
    ed.insert(LANE, "lane", Params(Lane(Param::new(Hz(BASE)))));
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
    ed.spec_mut().topology.set_outputs([Source::Node(OutPort {
        node: LANE,
        port: 0,
    })]);
    ed.spec_mut()
        .connect_param(at, EventOut { node: SRC, port: 0 });
    ed.commit().expect("commits");
    exec.apply_pending();
    let mut block = |ed: &mut Editor<f64>| {
        let mut out = vec![0.0f64; FRAMES];
        exec.process(
            FRAMES,
            &TransportPosition::default(),
            &[],
            &mut [&mut out[..]],
        );
        ed.collect();
        out
    };
    let last = |out: &[f64]| out[FRAMES - 1];
    assert_eq!(last(&block(&mut ed)), 3.0, "unbounded: the ramp's target");
    for (lo, hi) in [(0.0, 2.0), (-1.0, 1.5), (0.0, 2.5), (0.5, 4.0)] {
        ed.set_param_range(at, ParamRange::new(lo, hi))
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
    let mut out = vec![0.0f64; FRAMES];
    exec.process(
        FRAMES,
        &TransportPosition::default(),
        &[],
        &mut [&mut out[..]],
    );
    ed.collect();
    out
}

/// **An edge weight moved between blocks ramps in `f64`.** `FINE` through a
/// weight that goes from 0.5 to 1.5 is `FINE * (0.5 + (k + 1) / n)` at frame
/// `k` of the next block, to `f64` rounding.
#[test]
fn an_f64_edge_weight_ramps_in_f64() {
    use tutti_graph::{Edge, EdgeGain, EdgeId, Sink};
    use tutti_units::Amplitude;
    let (mut ed, mut exec): (Editor<f64>, Executor<f64>) = Editor::with_format(prepare(FRAMES));
    ed.insert(LANE, "dc", ForkByClone(Dc(FINE)));
    let from = Source::Node(OutPort {
        node: LANE,
        port: 0,
    });
    ed.spec_mut().topology.set_outputs([from]);
    let id = EdgeId {
        sink: Sink::Output(0),
        from: Edge::Direct(from),
    };
    ed.spec_mut()
        .set_edge_gain(id, EdgeGain::new(Amplitude::new(0.5)));
    ed.commit().expect("commits");
    exec.apply_pending();
    let first = block(&mut exec, &mut ed);
    assert_eq!(first[FRAMES - 1], FINE * 0.5, "a steady weight");
    ed.set_edge_gain(id, Amplitude::new(1.5)).expect("an edge");
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
    ed.spec_mut().topology.connect(
        tutti_graph::InPort {
            node: LANE,
            port: 0,
        },
        tutti_graph::Edge::Direct(Source::Node(OutPort {
            node: NodeKey(5),
            port: 0,
        })),
    );
    ed.spec_mut().topology.set_outputs([Source::Node(OutPort {
        node: LANE,
        port: 0,
    })]);
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
