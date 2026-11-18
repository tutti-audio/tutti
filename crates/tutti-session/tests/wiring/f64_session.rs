//! A session rendered at `f64`: a figure `f32` cannot hold survives the graph, a
//! node set through `Session::set` and the offline `render`, unrounded.

use tutti_buffers::{Float, InterleavedMut};
use tutti_graph::{Cx, ForkByClone, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status};
use tutti_session::{Offline, Root, Session, SessionBuilder, Workers};
use tutti_types::{ChannelLayout, MaxBlock, Param};
use tutti_units::{Amplitude, Samples};

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

/// Multiplies a mono signal by its `Amplitude` param, in any sample format.
#[derive(Clone)]
struct Gain(Param<Amplitude>);

impl<F: Float> Node<F> for Gain {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let gain = F::from_f64(f64::from(self.0.load().get()));
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
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.0.detach();
        fork
    }
}

tutti_graph::declare_params!([] Gain, n { Volume => Amplitude = &n.0 });

const BLOCK: usize = 64;

fn session(workers: Workers) -> Session<&'static str, Offline<f64>> {
    SessionBuilder::<Offline<f64>>::default()
        .outputs(ChannelLayout::MONO)
        .outputs_floor(ChannelLayout::MONO)
        .max_block(MaxBlock::of(Samples(BLOCK)))
        .render_workers(workers)
        .build()
        .expect("an f64 session builds")
}

/// `FINE` through `lanes` gains into the root: set each lane's gain to
/// `0.125 * (lane + 1)` with `Session::set`, render one block and return it.
fn render(workers: Workers, lanes: usize) -> Vec<f64> {
    let mut s = session(workers);
    let (dc, _) = s.graph().add(ForkByClone(Dc(FINE)));
    let mut gains = Vec::new();
    for _ in 0..lanes {
        let (gain, _) = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
        s.graph().connect(dc.out(0), gain.input(0)).unwrap().hard();
        s.graph()
            .connect(gain.out(0), Root::output(0))
            .unwrap()
            .hard();
        gains.push(gain);
    }
    let _ = s.update();
    for (lane, gain) in gains.iter().enumerate() {
        let value = 0.125 * (lane + 1) as f32;
        s.set(gain.volume().to(Amplitude::new(value))).unwrap();
    }
    let mut block = vec![f64::NAN; BLOCK];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
    block
}

/// **An `f64` session renders a constant `f32` cannot hold, scaled by a gain set
/// through `Session::set`, exactly.** `0.75 * FINE` is a product `f32` rounds
/// away.
#[test]
fn an_f64_session_renders_a_figure_f32_rounds_away_through_a_set_gain() {
    assert_ne!(f64::from(FINE as f32), FINE, "FINE is not an f32");
    assert_ne!(f64::from((0.75 * FINE) as f32), 0.75 * FINE);
    let mut s = session(Workers::SERIAL);
    let (dc, _) = s.graph().add(ForkByClone(Dc(FINE)));
    let (gain, _) = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
    s.graph().connect(dc.out(0), gain.input(0)).unwrap().hard();
    s.graph()
        .connect(gain.out(0), Root::output(0))
        .unwrap()
        .hard();
    let _ = s.update();
    s.set(gain.volume().to(Amplitude::new(0.75))).unwrap();

    let mut block = vec![f64::NAN; BLOCK];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
    assert!(block.iter().all(|&x| x == 0.75 * FINE), "{:?}", &block[..3]);
}

/// **Lanes of gains sum exactly at `f64`, and the pool renders them bit for bit
/// as the serial render does.** The lanes' gains `0.125 * (k + 1)` sum to 4.5 of
/// `FINE`.
#[test]
fn an_f64_session_sums_its_lanes_exactly_and_the_parallel_render_is_bit_identical() {
    let serial = render(Workers::SERIAL, 8);
    assert!(
        serial.iter().all(|&x| x == 4.5 * FINE),
        "the lanes sum exactly: {:?}",
        &serial[..3]
    );
    let parallel = render(Workers::of(4), 8);
    let bits = |b: &[f64]| b.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&serial), bits(&parallel));
}
