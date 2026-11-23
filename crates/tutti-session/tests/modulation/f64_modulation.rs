//! A modulation lane drives a param on an `f64` session, and the render holds
//! a figure `f32` cannot: the lane's segments are `f32` by spec, the audio
//! they scale is `f64`.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{ForkByClone, Params};
use tutti_rt::Param;
use tutti_session::{MainOutput, OutputPort, ParamRef, RouteSpec, Session};
use tutti_units::{Amplitude, Bipolar, SampleRate, UnitParam};

#[path = "../support/f64_nodes.rs"]
mod f64_nodes;
use f64_nodes::{Dc, FINE, Gain};

const BLOCK: usize = 128;

/// **A route onto a native param on an `f64` session scales `FINE` by the
/// analytic gain.** The gain's base is 1; a macro at 0.5 routed at depth 0.25
/// over the range `0..=2` (span 2) offsets it by `0.25 * 0.5 * 2`, so the
/// render is `FINE * 1.25` in every frame past the lane's start.
#[test]
fn a_route_onto_a_param_scales_an_f64_render_by_the_analytic_gain() {
    assert_ne!(f64::from(FINE as f32), FINE, "f32 would round FINE away");
    let mut s = Session::builder()
        .float::<f64>()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("an f64 session builds");
    let dc = s.graph().add(ForkByClone(Dc(FINE)));
    let gain = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
    s.graph()
        .connect(OutputPort::new(dc, 0), gain.input(0))
        .unwrap()
        .unfaded();
    s.graph()
        .connect(OutputPort::new(gain, 0), MainOutput::channel(0))
        .unwrap()
        .unfaded();

    let target = ParamRef::unit(gain.erase(), UnitParam::Volume);
    let mut m = s.mods_mut();
    m.range_raw(target, 0.0, 2.0).unwrap();
    let source = m.macro_(0.5);
    m.route(RouteSpec::new(source, target, Bipolar::of(0.25)))
        .unwrap();
    for _ in 0..6 {
        let _ = s.update();
    }
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "the declarations apply"
    );

    let mut heard = Vec::new();
    let mut block = vec![0.0f64; BLOCK * 2];
    for _ in 0..12 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        heard.extend(block.chunks(2).map(|frame| frame[0]));
    }
    for (frame, &x) in heard.iter().enumerate().skip(512) {
        assert_eq!(x, FINE * 1.25, "frame {frame}");
    }
}
