//! A graph rendered at `f64` end to end: a figure `f32` cannot hold survives the
//! builder, the executor's fan-in sum and the renderer's views unrounded.

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_graph::contract::{BlockRig, Direct, drive};
use tutti_graph::testing::{bits, test_pool};
use tutti_graph::{
    Cx, ForkByClone, ForkTarget, GraphBuilder, Io, Node, ParamNode, ParamSet, Params, Prepare,
    Renderer, Shape, Status,
};
use tutti_rt::Param;
use tutti_units::{Amplitude, SampleRate, Samples, UnitParam};

/// One part in 2^40 above one: past `f32`'s 24-bit significand, inside `f64`'s 53.
const FINE: f64 = 1.0 + 1.0 / 1_099_511_627_776.0;

/// A constant source in any sample format.
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

/// Halves a mono signal, in any sample format.
#[derive(Clone)]
struct Half;

impl<F: Float> Node<F> for Half {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = *i * F::lit(0.5);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn prepare() -> Prepare {
    Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(16)))
}

/// Two equal sources summed at an output port is exactly twice `FINE`, and
/// through a halving node exactly half of it: at `f32` both would read 2 and 0.5.
#[test]
fn a_fan_in_sum_and_a_gain_keep_the_figure_f32_rounds_away() {
    assert_ne!(f64::from(FINE as f32), FINE, "FINE is not an f32");
    let mut g = GraphBuilder::<f64>::with_format(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let (a, b) = (g.add(ForkByClone(Dc(FINE))), g.add(ForkByClone(Dc(FINE))));
    let half = g.add(ForkByClone(Half));
    g.connect(a, 0, half, 0);
    g.connect_output(a, 0, 0).connect_output(b, 0, 0);
    g.connect_output(half, 0, 1);
    let mut r = g.renderer(prepare()).expect("builds at f64");
    let out = r.render(40);
    assert!(
        out[0].iter().all(|&x| x == 2.0 * FINE),
        "{:?}",
        &out[0][..3]
    );
    assert!(
        out[1].iter().all(|&x| x == 0.5 * FINE),
        "{:?}",
        &out[1][..3]
    );
}

/// A caller's `f64` planes are read as they are.
#[test]
fn caller_planes_cross_the_renderer_at_f64() {
    let mut g = GraphBuilder::<f64>::with_format(ChannelLayout::MONO, ChannelLayout::MONO);
    let half = g.add(ForkByClone(Half));
    g.connect_input(0, half, 0).connect_output(half, 0, 0);
    let mut r = g.renderer(prepare()).expect("builds at f64");
    let input = vec![FINE; 40];
    let out = r.render_input(Planes::new(&[&input[..]]).expect("one plane"));
    assert!(out[0].iter().all(|&x| x == 0.5 * FINE));
}

/// `render_into` writes the caller's `f64` planes whole: a buffer pre-filled
/// with a sentinel reads `FINE` in every sample afterwards.
#[test]
fn render_into_overwrites_the_callers_f64_planes() {
    let mut g = GraphBuilder::<f64>::with_format(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let dc = g.add(ForkByClone(Dc(FINE)));
    g.connect_output(dc, 0, 0);
    let mut r = g.renderer(prepare()).expect("builds at f64");
    let mut into = vec![-7.0f64; 40];
    r.render_into(PlanesMut::new(&mut [&mut into[..]]).expect("one plane"));
    assert!(into.iter().all(|&x| x == FINE), "{:?}", &into[..3]);
}

/// `drive` hands a node `f64` planes whole and returns its `f64` outputs
/// unrounded: halving `FINE` reads `0.5 * FINE`, which `f32` cannot hold.
#[test]
fn drive_runs_a_generic_node_at_f64() {
    let mut node = Half;
    Node::<f64>::prepare(&mut node, &prepare());
    let input = [FINE; 16];
    let out = drive(
        &mut node as &mut dyn Node<f64>,
        SampleRate::SR_48K,
        Planes::new(&[&input[..]]).expect("one plane"),
        &[],
    );
    assert!(
        out[0].iter().all(|&x| x == 0.5 * FINE),
        "{:?}",
        &out[0][..3]
    );
}

/// `Direct` and `BlockRig` in `f64` fill their input planes, run the node and
/// read its output planes without rounding through `f32`.
#[test]
fn the_rigs_run_a_generic_node_at_f64() {
    let rate = SampleRate::SR_48K;
    let mut direct = Direct::<Half, f64>::with_format(Half, rate, 16);
    direct.inputs_mut().plane_mut(0).samples_mut().fill(FINE);
    direct.block();
    assert!(direct.output().plane(0).iter().all(|&x| x == 0.5 * FINE));

    let (mut rig, ()) = BlockRig::<f64>::with_format(ForkByClone(Half), rate, 16);
    rig.inputs_mut().plane_mut(0).samples_mut().fill(FINE);
    rig.block();
    assert!(rig.output().plane(0).iter().all(|&x| x == 0.5 * FINE));
}

/// Multiplies a mono signal by its `Amplitude` param, in any sample format: a
/// [`ParamNode`] at `f64`.
#[derive(Clone)]
struct Gain(Param<Amplitude>);

impl<F: Float> Node<F> for Gain {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let gain = F::from_f64(self.0.load().get());
        let (ins, mut outs) = io.split();
        for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = *i * gain;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl<F: Float> ParamNode<F> for Gain {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Volume, self.0.as_atomic())
            .build()
    }
    fn fork_fresh(&self) -> Self {
        let mut f = self.clone();
        f.0.detach();
        f
    }
}

/// `FINE` through `lanes` gains into one output: the sum of `FINE * g` over the lanes.
fn gain_graph(lanes: usize) -> (GraphBuilder<f64>, Vec<ParamSet>) {
    let mut g = GraphBuilder::<f64>::with_format(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let dc = g.add(ForkByClone(Dc(FINE)));
    let mut sets = Vec::new();
    for _ in 0..lanes {
        let (gain, set) = g.add_with_controls(Params(Gain(Param::new(Amplitude::new(1.0)))));
        g.connect(dc, 0, gain, 0).connect_output(gain, 0, 0);
        sets.push(set);
    }
    (g, sets)
}

/// A `ParamNode` at `f64`: a gain set through its controls scales the signal
/// exactly (`0.75 * FINE`, a product `f32` cannot hold), and a fork starts at
/// the value the cell holds when it is taken, not at the value at insert and
/// not at a value set after.
#[test]
fn an_f64_graph_hosts_a_param_node_and_forks_it_from_its_value() {
    let (g, sets) = gain_graph(1);
    let mut r = g.renderer(prepare()).expect("builds at f64");
    assert!(sets[0].set(UnitParam::Volume, 0.75));
    let out = r.render(40);
    assert!(
        out[0].iter().all(|&x| x == 0.75 * FINE),
        "{:?}",
        &out[0][..3]
    );

    let (fork_ed, fork_exec) = r
        .editor()
        .fork(ForkTarget::MainOutput, prepare())
        .expect("forks at f64");
    assert!(sets[0].set(UnitParam::Volume, 0.5));
    let mut forked = Renderer::new(fork_ed, fork_exec);
    let out = forked.render(40);
    assert!(
        out[0].iter().all(|&x| x == 0.75 * FINE),
        "{:?}",
        &out[0][..3]
    );
    let live = r.render(40);
    assert!(
        live[0].iter().all(|&x| x == 0.5 * FINE),
        "{:?}",
        &live[0][..3]
    );
}

/// Params nodes on a parallel `f64` executor render bit for bit what the serial one does.
#[test]
fn the_parallel_f64_render_of_param_nodes_is_bit_identical_to_the_serial_one() {
    let render = |workers: usize| {
        let (g, sets) = gain_graph(8);
        let mut r = g.renderer(prepare()).expect("builds at f64");
        if workers > 1 {
            r.executor_mut().set_pool(Some(test_pool(workers)));
            assert!(r.executor_mut().is_parallel(), "the plan runs on the pool");
        }
        for (k, set) in sets.iter().enumerate() {
            assert!(set.set(UnitParam::Volume, 0.125 * (k + 1) as f64));
        }
        r.render(64)
    };
    let serial = render(1);
    assert!(
        serial[0].iter().all(|&x| x == 4.5 * FINE),
        "the lanes sum exactly"
    );
    assert_eq!(bits(&serial), bits(&render(4)));
}
