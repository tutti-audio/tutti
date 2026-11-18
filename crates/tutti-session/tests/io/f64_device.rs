//! A session on a device clock rendered at `f64`: the stream's callback state
//! carries the graph's own format, so a figure `f32` cannot hold reaches the
//! device buffer unrounded.

use tutti_buffers::SampleFormat;
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_graph::{ForkByClone, Params};
use tutti_session::{Offline, Root, SessionBuilder};
use tutti_types::{ChannelLayout, Param};
use tutti_units::{Amplitude, SampleRate, Samples};

#[path = "../support/f64_nodes.rs"]
pub(super) mod f64_nodes;
use f64_nodes::{Dc, FINE, Gain};

/// **The device callback of an `f64` session hands the stream `0.75 * FINE`
/// exactly.** A gain set through `Session::set` scales a constant `f32` rounds
/// away, and the buffer the stream renders is `f64`.
#[test]
fn an_f64_device_session_renders_a_figure_f32_rounds_away_to_its_stream() {
    // Setup: `f32` loses the figure, so a pass is not a rounded one.
    assert_ne!(f64::from((0.75 * FINE) as f32), 0.75 * FINE);
    let (driver, stream) = ManualStreamDriver::<f64>::new();
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::MONO, SampleFormat::F64);
    let mut s = SessionBuilder::<Offline<f64>>::default()
        .outputs_floor(ChannelLayout::MONO)
        .device(Default::default())
        .build_device_on(spec, driver)
        .expect("an f64 device session builds with no device");
    let (dc, _) = s.graph().add(ForkByClone(Dc(FINE)));
    let (gain, _) = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
    s.graph().connect(dc.out(0), gain.input(0)).unwrap().hard();
    s.graph()
        .connect(gain.out(0), Root::output(0))
        .unwrap()
        .hard();
    let _ = s.update();
    s.set(gain.volume().to(Amplitude::new(0.75))).unwrap();

    let block = stream
        .render_block(Samples(64))
        .expect("the stream is open");
    assert!(block.iter().all(|&x| x == 0.75 * FINE), "{:?}", &block[..3]);
}
