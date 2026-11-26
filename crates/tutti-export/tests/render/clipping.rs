//! Clipping is counted, and a normalized export leaves an integer format its
//! headroom so that it does not clip.

use crate::support::{EncodeConfig, ExportConfig, RenderConfig};

use crate::support::{render_normalized_to_file, render_to_file};
use tutti_buffers::ChannelLayout;
use tutti_buffers::FloatWidth;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{Format, Pcm};
use tutti_dsp::DitherShape;
use tutti_export::{Normalize, RenderGraph, Setup};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::{Const, Osc};
use tutti_timeline::FrozenClock;
use tutti_units::Seconds;
use tutti_units::{Db, Hz, SampleRate};

const RATE: SampleRate = SampleRate::SR_48K;

/// An integer type of `depth`, dithered with `dither` (none for `None`) and noise-shaped when `shaped`.
fn int(depth: IntDepth, dither: Option<DitherShape>, shaped: bool) -> Pcm {
    let pcm = IntPcm::new(depth);
    let pcm = match dither {
        Some(shape) => pcm.dither(shape),
        None => pcm.no_dither(),
    };
    Pcm::Int(if shaped { pcm.noise_shaping() } else { pcm })
}

fn config(pcm: Pcm) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds::new(0.1),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(pcm),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    }
}

fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("paired")
}

fn dc(value: f32) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[value, value]));
    g.pipe_output(id);
    built(g)
}

/// A sine at a quarter of the rate, whose samples are 0, 1, 0, -1: its sample
/// peak is its true peak, so a normalization to 0 dBTP puts its crests exactly
/// on full scale.
fn quarter_rate_sine() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Osc::sine(Hz::new(RATE.get() / 4.0)));
    g.pipe_output(id);
    built(g)
}

/// `Written::clipped_samples` is the number of values the conversion clipped:
/// every sample of a 1.2 DC (two channels, 4,800 frames) at 16 bit, every
/// sample of a 1.0 (`+1.0` is one step past `i16::MAX`), none of the top step
/// `32767/32768`, and none for a format that does not quantize.
#[test]
fn the_samples_the_quantizer_clamps_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clip.wav");
    let frames = 4_800u64;

    let over = render_to_file(
        dc(1.2),
        &config(int(IntDepth::Int16, None, false)),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(over.clipped_samples, 2 * frames);

    let at = render_to_file(
        dc(1.0),
        &config(int(IntDepth::Int16, None, false)),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(at.clipped_samples, 2 * frames, "+1.0 is past the top step");

    let top = render_to_file(
        dc(32_767.0 / 32_768.0),
        &config(int(IntDepth::Int16, None, false)),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(top.clipped_samples, 0, "the top step is not clipping");

    let float = render_to_file(
        dc(1.2),
        &config(Pcm::Float(FloatWidth::F32)),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(float.clipped_samples, 0, "a float file stores 1.2 as it is");
}

/// The largest absolute code in a 16- or 24-bit WAV.
fn max_code(path: &std::path::Path) -> i64 {
    let reader = hound::WavReader::open(path).unwrap();
    reader
        .into_samples::<i32>()
        .map(|s| i64::from(s.unwrap()).abs())
        .max()
        .unwrap()
}

/// A peak normalized to 0 dBTP at an integer depth, with dither, clips nothing:
/// the target is pulled down to one LSB plus the dither amplitude below full
/// scale (1, 1.5 or 2 LSB), so the crest plus its dither stays at or under the
/// top code (the top code is a value, not a clip: `clipped_samples` is 0). Every
/// depth and dither. The target the gain lands the measured peak on is the
/// clamped figure.
#[test]
fn a_zero_dbtp_target_leaves_an_integer_format_its_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero.wav");
    for (depth, bits) in [(IntDepth::Int16, 16u32), (IntDepth::Int24, 24)] {
        for (dither, lsbs) in [
            (None, 1.0f64),
            (Some(DitherShape::Rectangular), 1.5),
            (Some(DitherShape::Triangular), 2.0),
        ] {
            let pcm = int(depth, dither, false);
            let written = render_normalized_to_file(
                quarter_rate_sine(),
                &config(pcm),
                &FrozenClock,
                Normalize::peak(Db::new(0.0)),
                &path,
            )
            .unwrap();
            assert_eq!(written.clipped_samples, 0, "{pcm:?} {dither:?}");
            let full = (1i64 << (bits - 1)) - 1;
            assert!(
                max_code(&path) <= full,
                "{pcm:?} {dither:?}: past the top code"
            );
            let want = tutti_units::Db::from_amplitude(tutti_units::Amplitude::new(
                1.0 - lsbs / f64::from(1u32 << (bits - 1)),
            ))
            .get();
            // The effective target is where the measured peak lands under the
            // reported gain: `gain` is `target - measured.true_peak` exactly.
            let gain = written.gain.expect("a normalizing export reports");
            let measured = written.measured.expect("and what it measured");
            let landed = measured.true_peak.get() + gain.get();
            assert!(
                (landed - want).abs() < 1e-5,
                "{pcm:?} {dither:?}: target {landed} vs {want}"
            );
        }
    }
}

/// An infinity clips to the rail and is counted; a NaN is written as 0 and is
/// not a clip (it is not a value past full scale).
#[test]
fn an_infinity_is_a_clip_and_a_nan_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nan.wav");
    for (value, counted) in [
        (f32::NAN, 0),
        (f32::INFINITY, 2 * 4_800),
        (f32::NEG_INFINITY, 2 * 4_800),
    ] {
        let written = render_to_file(
            dc(value),
            &config(int(IntDepth::Int16, None, false)),
            &FrozenClock,
            &path,
        )
        .unwrap();
        assert_eq!(written.clipped_samples, counted, "{value}");
    }
}

/// A noise-shaped export normalized to 0 dBTP clips nothing, and its target sits below the
/// unshaped one by the feedback: the shaped error reaches `(1 + Σ|c_k|)` times the largest
/// recorded error (dither plus half an LSB), so the ceiling drops by `Σ|c_k|` times that.
/// The sine's crests sit on the ceiling, so any shortfall in the headroom is a clip.
#[test]
fn a_shaped_zero_dbtp_export_leaves_the_feedback_its_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shaped.wav");
    let gain = tutti_dsp::NoiseShaping::new(RATE).feedback_gain().get();
    for (depth, bits) in [(IntDepth::Int16, 16u32), (IntDepth::Int24, 24)] {
        for (dither, peak) in [
            (None, 0.0),
            (Some(DitherShape::Rectangular), 0.5),
            (Some(DitherShape::Triangular), 1.0),
        ] {
            let pcm = int(depth, dither, true);
            let cfg = config(pcm);
            let written = render_normalized_to_file(
                quarter_rate_sine(),
                &cfg,
                &FrozenClock,
                Normalize::peak(Db::new(0.0)),
                &path,
            )
            .unwrap();
            assert_eq!(written.clipped_samples, 0, "{pcm:?} {dither:?}");
            let lsbs = 1.0 + peak + gain * (peak + 0.5);
            let want = Db::from_amplitude(tutti_units::Amplitude::new(
                1.0 - lsbs / f64::from(1u32 << (bits - 1)),
            ))
            .get();
            let landed = written.measured.unwrap().true_peak.get() + written.gain.unwrap().get();
            assert!(
                (landed - want).abs() < 1e-5,
                "{pcm:?} {dither:?}: target {landed} vs {want}"
            );
            let unshaped_lsbs = 1.0 + peak;
            assert!(lsbs > unshaped_lsbs);
        }
    }
}
