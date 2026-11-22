//! A bounce of an `f64` session: the fork renders at `f64`, and a constant
//! `f32` cannot hold reaches the planes and the Float64 file unrounded.

use crate::bounced::bounced;
use tutti_buffers::ChannelLayout;
use tutti_codec::{Format, Pcm};
use tutti_graph::{ForkByClone, Params};
use tutti_rt::Param;
use tutti_session::{Bounce, BounceTail, MainOutput, Range, Session};
use tutti_units::{Amplitude, SampleRate, Seconds};

#[path = "../support/f64_nodes.rs"]
mod f64_nodes;
use f64_nodes::{Dc, FINE, Gain};

/// A detached `f64` session of `0.75 * FINE` on one output: a gain set through
/// `Session::set`, so the fork starts from the value set.
fn session() -> tutti_session::DetachedSession<&'static str, f64> {
    let mut s = Session::builder()
        .float::<f64>()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("an f64 session builds");
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
    s
}

/// **A bounce into memory holds `0.75 * FINE` in every frame, as `f64`.**
#[test]
fn an_f64_bounce_into_planes_keeps_the_figure_f32_rounds_away() {
    // Setup: `f32` loses the figure, so a pass is not a rounded one.
    assert_ne!(f64::from((0.75 * FINE) as f32), 0.75 * FINE);
    let planes = bounced(
        &mut session(),
        Bounce::new()
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.01)))
            .main_output_buffers()
            .channels(ChannelLayout::MONO)
            .tail(BounceTail::None),
    )
    .expect("the f64 bounce runs")
    .into_main_output()
    .into_planes();
    assert!(!planes[0].is_empty());
    assert!(
        planes[0].iter().all(|&x| x == 0.75 * FINE),
        "{:?}",
        &planes[0][..3]
    );
}

/// **A bounce to a Float64 file reads back exactly.** The file is written from
/// the render's own `f64`, with no rounding through `f32` on the way.
#[test]
fn an_f64_bounce_to_a_float64_file_reads_back_exactly() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.wav");
    bounced(
        &mut session(),
        Bounce::new()
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.01)))
            .main_output(&path, Format::Wav(Pcm::Float64))
            .tail(BounceTail::None),
    )
    .expect("the f64 bounce runs");
    let wave = tutti_codec::read_wave::<f64>(&path).expect("the file reads back");
    let samples = wave.channel(0).samples();
    assert!(!samples.is_empty());
    assert!(
        samples.iter().all(|&x| x == 0.75 * FINE),
        "{:?}",
        &samples[..3]
    );
}
