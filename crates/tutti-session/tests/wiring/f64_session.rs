//! A session rendered at `f64`: a figure `f32` cannot hold survives the graph, a
//! node set through `Session::set` and the offline `render`, unrounded.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{ForkByClone, Params};
use tutti_rt::Param;
use tutti_session::{MainOutput, Offline, Session, Workers};
use tutti_units::{Amplitude, Samples};

#[path = "../support/f64_nodes.rs"]
mod f64_nodes;
use f64_nodes::{Dc, FINE, Gain};

const BLOCK: usize = 64;

fn session(workers: Workers) -> Session<&'static str, Offline<f64>> {
    Session::builder()
        .float::<f64>()
        .outputs(ChannelLayout::MONO)
        .outputs_floor(ChannelLayout::MONO)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .render_workers(workers)
        .build()
        .expect("an f64 session builds")
}

/// `FINE` through `lanes` gains into the root: set each lane's gain to
/// `0.125 * (lane + 1)` with `Session::set`, render one block and return it.
fn render(workers: Workers, lanes: usize) -> Vec<f64> {
    let mut s = session(workers);
    let dc = s.graph().add(ForkByClone(Dc(FINE)));
    let mut gains = Vec::new();
    for _ in 0..lanes {
        let gain = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
        s.graph()
            .connect(dc.out(0), gain.input(0))
            .unwrap()
            .unfaded();
        s.graph()
            .connect(gain.out(0), MainOutput::channel(0))
            .unwrap()
            .unfaded();
        gains.push(gain);
    }
    let _ = s.update();
    for (lane, gain) in gains.iter().enumerate() {
        let value = 0.125 * (lane + 1) as f64;
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
    // Setup: `f32` loses the figure, so a pass is not a rounded one.
    assert_ne!(f64::from(FINE as f32), FINE, "FINE is not an f32");
    assert_ne!(f64::from((0.75 * FINE) as f32), 0.75 * FINE);
    let mut s = session(Workers::SERIAL);
    let dc = s.graph().add(ForkByClone(Dc(FINE)));
    let gain = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
    s.graph()
        .connect(dc.out(0), gain.input(0))
        .unwrap()
        .unfaded();
    s.graph()
        .connect(gain.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
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
