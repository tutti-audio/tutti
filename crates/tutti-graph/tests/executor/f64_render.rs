//! A graph rendered at `f64` end to end: a figure `f32` cannot hold survives the
//! builder, the executor's fan-in sum and the renderer's views unrounded.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

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
            *o = *i * F::from_f64(0.5);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn prepare() -> Prepare {
    Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(16)))
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
