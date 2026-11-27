//! A bounce's dither is its format's: triangular unless the `IntPcm` says otherwise, and none
//! when it says none.
//!
//! A constant that falls between two 16-bit steps shows what each setting
//! does to the quantization: undithered, every sample is the plain rounding;
//! dithered, the samples scatter around it by at most the shape's peak.

use crate::bounced::bounced;
use tutti_buffers::ChannelLayout;
use tutti_codec::Encoding;
use tutti_codec::IntPcm;
use tutti_codec::{Format, IntDepth, Pcm};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::ToFile;
use tutti_session::{Bounce, DitherShape, MainOutput, Range, Session};
use tutti_units::{SampleRate, Seconds};

/// 9830.1 steps of 16 bit: a tenth of a step off a code.
const LEVEL: f32 = 9_830.1 / 32_768.0;

/// The 16-bit samples of a bounce of a constant `LEVEL` into a file of `pcm`.
fn dithered(pcm: IntPcm) -> Vec<i32> {
    bounced_node(ForkByClone(Const::new(LEVEL, ChannelLayout::STEREO)), pcm)
}

/// The 16-bit samples of a bounce of `node` into a file of `pcm`.
fn bounced_node<N: tutti_graph::Node + Clone + Send + 'static>(
    node: ForkByClone<N>,
    pcm: IntPcm,
) -> Vec<i32> {
    let mut session = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let dc = session.graph().add(node);
    session.graph().connect(dc, MainOutput).unwrap().unfaded();
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.wav");
    let bounce = Bounce::new()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.1)))
        .main_output(ToFile::new(
            Encoding::new(Format::Wav(Pcm::Int(pcm))),
            tutti_codec::ToFile::new(&path),
        ))
        .tail(tutti_session::BounceTail::None);
    bounced(&mut session, bounce).expect("renders");
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
    let samples = dithered(IntPcm::new(IntDepth::Int16).no_dither());
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
    let default = dithered(IntPcm::new(IntDepth::Int16));
    assert!(
        (error_power(&default) - 0.25).abs() < 0.02,
        "{}",
        error_power(&default)
    );

    let plain = IntDepth::Int16.quantize(LEVEL);
    let rectangular = dithered(IntPcm::new(IntDepth::Int16).dither(DitherShape::Rectangular));
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
    let plain = bounced_node(node(), IntPcm::new(IntDepth::Int16).no_dither());
    let shaped = bounced_node(
        node(),
        IntPcm::new(IntDepth::Int16).no_dither().noise_shaping(),
    );
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

/// **Each file of a bounce has its own dither noise.** The master and its stems are summed
/// later, and one noise sequence in two of them would add coherently. Silence into a dithered
/// 16-bit file is the dither alone, so equal files would be equal noise. **A seed the caller
/// states is used as given**: two stems stating one seed hold equal noise.
#[test]
fn the_files_of_a_bounce_are_dithered_with_different_noise() {
    use tutti_session::Stem;
    let mut session = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let silence = session
        .graph()
        .add(ForkByClone(Const::new(0.0, ChannelLayout::STEREO)));
    session
        .graph()
        .connect(silence, MainOutput)
        .unwrap()
        .unfaded();
    let dir = tempfile::tempdir().expect("dir");
    let paths = [
        "main.wav",
        "one.wav",
        "two.wav",
        "seven.wav",
        "seven_again.wav",
    ]
    .map(|name| dir.path().join(name));
    let format = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16)));
    let bounce = Bounce::new()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.1)))
        .main_output(ToFile::new(
            Encoding::new(format),
            tutti_codec::ToFile::new(&paths[0]),
        ))
        .stem_with(Stem::new(silence.erase()).to(ToFile::new(
            Encoding::new(format),
            tutti_codec::ToFile::new(&paths[1]),
        )))
        .stem_with(Stem::new(silence.erase()).to(ToFile::new(
            Encoding::new(format),
            tutti_codec::ToFile::new(&paths[2]),
        )))
        .stem_with(Stem::new(silence.erase()).to(ToFile::new(
            Encoding::new(format).dither_seed(7),
            tutti_codec::ToFile::new(&paths[3]),
        )))
        .stem_with(Stem::new(silence.erase()).to(ToFile::new(
            Encoding::new(format).dither_seed(7),
            tutti_codec::ToFile::new(&paths[4]),
        )))
        .tail(tutti_session::BounceTail::None);
    bounced(&mut session, bounce).expect("renders");
    let [main, one, two, seven, seven_again] = paths.map(|path| {
        hound::WavReader::open(path)
            .expect("readable")
            .samples::<i16>()
            .map(|s| s.expect("a sample"))
            .collect::<Vec<_>>()
    });
    assert!(main.iter().any(|&s| s != 0), "the file holds dither");
    assert_ne!(main, one, "the master and a stem shared one noise sequence");
    assert_ne!(main, two, "the master and a stem shared one noise sequence");
    assert_ne!(one, two, "two stems shared one noise sequence");
    assert_eq!(seven, seven_again, "a stated seed was replaced");
    assert_ne!(seven, one, "a stated seed matched a mixed one");
}
