//! Noise shaping, measured end to end through a real encoder: a quiet sine
//! written to a 16-bit file, the error against the sine analysed in a band
//! where the weighting asks for less noise.
//!
//! The unshaped TPDF error is white with variance 1/6 (the dither) plus 1/12
//! (the rounding) LSB^2 (Lipshitz, Wannamaker and Vanderkooy, "Quantization
//! and dither: a theory", JAES 1992). The shaped error is that spectrum times
//! `|NTF|^2`, and `|NTF|` follows the threshold of hearing, which is lowest
//! near 3.3 kHz: the band 3 to 3.6 kHz carries a small fraction of the white
//! level.

use crate::support::{EncodeConfig, ExportConfig, RenderConfig, render_to_file};
use core::f64::consts::PI;
use tutti_buffers::ChannelLayout;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{Format, Pcm};
use tutti_export::{RenderGraph, Setup};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Osc;
use tutti_timeline::FrozenClock;
use tutti_units::{Amplitude, Hz, SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
const TONE: f64 = 997.0;
const LEVEL: f64 = 0.25;
const LSB: f64 = 1.0 / 32_768.0;
/// The white level of the unshaped TPDF error, in LSB^2.
const WHITE: f64 = 1.0 / 6.0 + 1.0 / 12.0;

fn sine() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let id = g.add(Osc::sine(Hz::new(TONE)).with_amplitude(Amplitude::new(LEVEL)));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
}

fn config(noise_shaping: bool) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds::new(2.0),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))),
            channels: ChannelLayout::MONO,
            topology: None,
        },
        noise_shaping,
        ..Default::default()
    }
}

/// The error of the written file against the sine, in LSB.
fn error_lsb(noise_shaping: bool) -> Vec<f64> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shaped.wav");
    render_to_file(sine(), &config(noise_shaping), &FrozenClock, &path).unwrap();
    hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<i32>()
        .enumerate()
        .map(|(n, y)| {
            let x = LEVEL * (2.0 * PI * TONE * n as f64 / RATE.get()).sin();
            (f64::from(y.unwrap()) * LSB - x) / LSB
        })
        .collect()
}

/// The mean periodogram of `e` over 24 frequencies from 3 to 3.6 kHz, normalized so that
/// white noise of variance `v` gives `v`. Each periodogram value has a relative standard
/// deviation of one; the mean of 24 about 0.2, which the bounds below leave room for.
fn band_power(e: &[f64]) -> f64 {
    let n = e.len() as f64;
    let sum: f64 = (0..24)
        .map(|k| {
            let w = 2.0 * PI * (3_000.0 + 25.0 * k as f64) / RATE.get();
            let (re, im) = e.iter().enumerate().fold((0.0, 0.0), |(re, im), (i, v)| {
                let a = w * i as f64;
                (re + v * a.cos(), im - v * a.sin())
            });
            (re * re + im * im) / n
        })
        .sum();
    sum / 24.0
}

/// **The unshaped file's error is white at 1/4 LSB^2 and the shaped file's is under a tenth
/// of that in the band where the weighting dips.** The control pins the analytic level (to
/// 40 %, five standard deviations of the estimate), so the shaped bound is a fraction of
/// a measured quantity and a no-op `noise_shaping` fails it.
#[test]
fn a_shaped_file_has_its_error_moved_out_of_the_hearing_dip() {
    let plain = band_power(&error_lsb(false));
    assert!((plain / WHITE - 1.0).abs() < 0.4, "unshaped {plain}");
    let shaped = band_power(&error_lsb(true));
    assert!(shaped < 0.1 * WHITE, "shaped {shaped} vs white {WHITE}");
}
