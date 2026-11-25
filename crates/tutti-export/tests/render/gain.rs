//! `Written::gain` and `Written::measured`: what a normalizing export reports,
//! pinned against figures worked out from the signal, not against another
//! render.

use crate::support::{EncodeConfig, ExportConfig, RenderConfig};

use crate::support::{render_normalized_to_file, render_to_file};
use tutti_buffers::Interleaved;
use tutti_buffers::{Bed, ChannelLayout};
use tutti_codec::FloatWidth;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{Format, Pcm};
use tutti_dsp::DitherShape;
use tutti_export::{Normalize, RenderGraph, Setup};
use tutti_graph::GraphBuilder;
use tutti_measure::{LoudnessConfig, measure_loudness};
use tutti_nodes::testing::Osc;
use tutti_timeline::FrozenClock;
use tutti_units::{Db, Hz, SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;

fn config(pcm: Pcm, dither: Option<DitherShape>, seconds: f64) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds::new(seconds),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(pcm),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        dither,
        ..Default::default()
    }
}

/// A full-scale sine in both channels (a mono node feeds every output).
fn sine(frequency: Hz) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Osc::sine(frequency));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("paired")
}

/// A peak normalization reports the gain it applied and the reading it came
/// from. The quarter-rate sine's samples are 0, 1, 0, -1, so its true peak is
/// 0 dBTP, and the gain to -6 dBTP is -6 dB; a plain export reports neither.
#[test]
fn a_peak_normalized_export_reports_its_gain_and_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("six.wav");
    for target in [-6.0f64, -3.0] {
        let written = render_normalized_to_file(
            sine(Hz::new(RATE.get() / 4.0)),
            &config(
                Pcm::Int(IntPcm::new(IntDepth::Int16)),
                Some(DitherShape::Triangular),
                0.1,
            ),
            &FrozenClock,
            Normalize::peak(Db::new(target)),
            &path,
        )
        .unwrap();
        let gain = written.gain.expect("a normalizing export reports its gain");
        let measured = written.measured.expect("and the reading it came from");
        assert!((gain.get() - target).abs() < 0.05, "{target}: {gain:?}");
        assert!(measured.true_peak.get().abs() < 0.05, "{measured:?}");
    }

    let plain = render_to_file(
        sine(Hz::new(1_000.0)),
        &config(Pcm::Int(IntPcm::new(IntDepth::Int16)), None, 0.1),
        &FrozenClock,
        &path,
    )
    .unwrap();
    assert_eq!(plain.gain, None);
    assert_eq!(plain.measured, None);
}

/// A loudness normalization of a known level lands on its target. A 1 kHz
/// sine at full scale in both channels reads 0 LUFS (EBU Tech 3341's
/// calibration: -0.691 + 10 log10 of the channel sum, K-weighting being unity
/// at 1 kHz, to within 0.1 LU), so the gain to -23 LUFS is -23 dB within the
/// same tolerance, and the file written with it reads -23 LUFS.
#[test]
fn a_loudness_normalized_export_reports_the_gain_to_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lufs.wav");
    let written = render_normalized_to_file(
        sine(Hz::new(1_000.0)),
        &config(Pcm::Float(FloatWidth::F32), None, 3.0),
        &FrozenClock,
        Normalize::lufs(Db::new(-23.0)),
        &path,
    )
    .unwrap();
    let measured = written.measured.expect("reported");
    let gain = written.gain.expect("reported");
    assert!(measured.lufs.get().abs() < 0.1, "{measured:?}");
    assert!((gain.get() + 23.0).abs() < 0.1, "{gain:?}");

    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(Result::unwrap)
        .collect();
    let meter = LoudnessConfig::new(Bed::Stereo.topology());
    let written_loudness = measure_loudness(
        &meter,
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        RATE,
    )
    .unwrap();
    assert!(
        (written_loudness.lufs.get() + 23.0).abs() < 0.1,
        "{written_loudness:?}"
    );
}
