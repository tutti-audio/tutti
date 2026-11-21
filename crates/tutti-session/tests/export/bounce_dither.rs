//! A bounce's dither: triangular unless it says otherwise, and none when it
//! says none.
//!
//! A constant that falls between two 16-bit steps shows what each setting
//! does to the quantization: undithered, every sample is the plain rounding;
//! dithered, the samples scatter around it by at most the shape's peak.

use tutti_buffers::ChannelLayout;
use tutti_codec::{Format, IntDepth, Pcm};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{Bounce, DitherShape, Range, Root, Session};
use tutti_units::{SampleRate, Seconds};

/// 9830.1 steps of 16 bit: a tenth of a step off a code.
const LEVEL: f32 = 9_830.1 / 32_768.0;

/// The 16-bit samples of a bounce of a constant `LEVEL`, as `configure` sets
/// its dither.
fn bounced(
    configure: impl FnOnce(
        Bounce<tutti_graph::NodeKey, tutti_session::FileMaster>,
    ) -> Bounce<tutti_graph::NodeKey, tutti_session::FileMaster>,
) -> Vec<i32> {
    bounced_node(
        ForkByClone(Const::new(LEVEL, ChannelLayout::STEREO)),
        configure,
    )
}

/// The 16-bit samples of a bounce of `node`, as `configure` sets its options.
fn bounced_node<N: tutti_graph::Node + Clone + Send + 'static>(
    node: ForkByClone<N>,
    configure: impl FnOnce(
        Bounce<tutti_graph::NodeKey, tutti_session::FileMaster>,
    ) -> Bounce<tutti_graph::NodeKey, tutti_session::FileMaster>,
) -> Vec<i32> {
    let mut session = Session::builder()
        .inputs(ChannelLayout::EMPTY)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let dc = session.graph().add(node);
    session.graph().connect(dc, Root).unwrap().unfaded();
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.wav");
    let bounce = Bounce::project()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.1)))
        .master(&path, Format::Wav(Pcm::Int16))
        .tail(tutti_session::BounceTail::None);
    session.bounce(configure(bounce)).expect("renders");
    hound::WavReader::open(&path)
        .expect("readable")
        .samples::<i16>()
        .map(|s| i32::from(s.expect("a sample")))
        .collect()
}

/// **`no_dither` leaves the plain rounding.** Every sample is the quantization
/// of the level, with nothing added.
#[test]
fn no_dither_writes_the_plain_quantization() {
    let plain = IntDepth::Int16.quantize(LEVEL);
    let samples = bounced(Bounce::no_dither);
    assert!(!samples.is_empty());
    assert!(samples.iter().all(|&s| s == plain), "{:?}", &samples[..8]);
}

/// The mean squared distance of `samples` from the exact level, in steps.
fn error_power(samples: &[i32]) -> f64 {
    let exact = f64::from(LEVEL) * 32_768.0;
    samples
        .iter()
        .map(|&s| (f64::from(s) - exact).powi(2))
        .sum::<f64>()
        / samples.len() as f64
}

/// **Triangular is the default, with the error power triangular dither has.**
/// Triangular dither on top of rounding leaves a mean squared error of 1/4
/// step^2 whatever the level (1/12 for the rounding, 1/6 for the dither's own
/// variance), which neither the plain rounding of a level off the step (0.01
/// here) nor rectangular dither (under 1/6 at this level) has. A rectangular
/// dither moves a sample by at most one step.
#[test]
fn the_default_dither_is_triangular() {
    let default = bounced(|b| b);
    assert!(
        (error_power(&default) - 0.25).abs() < 0.02,
        "{}",
        error_power(&default)
    );

    let plain = IntDepth::Int16.quantize(LEVEL);
    let rectangular = bounced(|b| b.dither(DitherShape::Rectangular));
    assert!(rectangular.iter().any(|&s| s != plain), "it dithers");
    assert!(rectangular.iter().all(|&s| (s - plain).abs() <= 1));
    assert!(
        error_power(&rectangular) < 0.2,
        "{}",
        error_power(&rectangular)
    );
}

/// **`noise_shaping` reaches the file.** With no dither the rounding of a sine is
/// deterministic and `no_dither` alone writes it; with the error fed back through the shaping
/// filter a good part of the samples round the other way, and every one stays within the
/// feedback bound `(1 + sum |c|) / 2` steps of the sine (a hundredth of a step more for the
/// oscillator's own `f32` rounding).
#[test]
fn noise_shaping_feeds_the_rounding_error_back() {
    use tutti_nodes::testing::Osc;
    let tone = |n: usize| {
        let phase = 2.0 * std::f64::consts::PI * 997.0 * (n / 2) as f64 / 48_000.0;
        0.25 * phase.sin() * 32_768.0
    };
    let node = || {
        ForkByClone(
            Osc::sine(tutti_units::Hz::new(997.0))
                .with_amplitude(tutti_units::Amplitude::new(0.25))
                .with_layout(ChannelLayout::STEREO),
        )
    };
    let plain = bounced_node(node(), Bounce::no_dither);
    let shaped = bounced_node(node(), |b| b.no_dither().noise_shaping());
    assert_eq!(plain.len(), shaped.len());
    let moved = plain.iter().zip(&shaped).filter(|(a, b)| a != b).count();
    assert!(moved * 10 > plain.len(), "{moved} of {}", plain.len());
    let bound = 0.5
        * (1.0
            + tutti_dsp::NoiseShaping::new(SampleRate::SR_48K)
                .feedback_gain()
                .get());
    for (n, &s) in shaped.iter().enumerate() {
        assert!((f64::from(s) - tone(n)).abs() <= bound + 0.01, "sample {n}");
    }
}
