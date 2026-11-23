//! The dynamic EQ band and its de-esser configuration as the graph runs them. Every figure is
//! from the closed form of the compressor's static curve (Giannoulis, Massberg and Reiss,
//! 2012, eq. 4 and 5) and of the RBJ bell under the bilinear map, computed here.

use core::f64::consts::PI;
use tutti_fixtures::signal::sine;
use tutti_units::{Amplitude, Samples};

use tutti_buffers::ChannelLayout;
use tutti_buffers::Planes;
use tutti_dsp::DynamicMode;
use tutti_graph::Node;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{DynamicEqConfig, DynamicEqNode, KeySource};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds};

const SR: SampleRate = SampleRate::SR_48K;
const FS: f64 = 48_000.0;
/// Frames of a run: the de-esser's 40 ms release and the detector's mean have settled
/// many times over.
const RUN: usize = 24_000;
/// The frames at the end read over: whole periods of every tone below (200, 1000 and 6000 Hz).
const WINDOW: usize = 4_800;

fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// The amplitude of the component at `f` in the last [`WINDOW`] frames of `x`, by correlation
/// with a sine and a cosine over whole periods.
fn component(x: &[f32], f: f64) -> f64 {
    let start = x.len() - WINDOW;
    let (mut s, mut c) = (0.0, 0.0);
    for (i, &v) in x[start..].iter().enumerate() {
        let w = 2.0 * PI * f * (start + i) as f64 / FS;
        s += f64::from(v) * w.sin();
        c += f64::from(v) * w.cos();
    }
    2.0 * s.hypot(c) / WINDOW as f64
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.log10()
}

/// The compressor's reduction at `level` dB, positive.
fn reduction(level: f64, threshold: f64, knee: f64, ratio: f64) -> f64 {
    let slope = 1.0 - 1.0 / ratio;
    if knee <= 0.0 {
        slope * (level - threshold).max(0.0)
    } else if level <= threshold - knee / 2.0 {
        0.0
    } else if level >= threshold + knee / 2.0 {
        slope * (level - threshold)
    } else {
        slope * (level - threshold + knee / 2.0).powi(2) / (2.0 * knee)
    }
}

/// `|H|` in dB at `f` of an RBJ bell of centre `f0`, `q` and `gain` dB.
fn bell_db(f: f64, f0: f64, q: f64, gain: f64) -> f64 {
    let a = 10f64.powf(gain / 40.0);
    let k = 1.0 / (q * a);
    let t = (PI * f / FS).tan() / (PI * f0 / FS).tan();
    let (dr, di) = (1.0 - t * t, k * t);
    let ni = k * (a * a - 1.0) * t;
    let den = dr * dr + di * di;
    let (re, im) = (1.0 + ni * di / den, ni * dr / den);
    10.0 * (re * re + im * im).log10()
}

fn de_esser() -> DynamicEqNode<f32> {
    DynamicEqNode::<f32>::from(DynamicEqConfig::de_esser(
        ChannelLayout::MONO,
        Hz::new(6_000.0),
    ))
}

fn render(node: DynamicEqNode<f32>, inputs: &[&[f32]]) -> Vec<f32> {
    let mut node = prepared(node, SR, inputs[0].len());
    drive(&mut node, SR, Planes::new(inputs).unwrap(), &[]).remove(0)
}

/// A sibilance-band sine is cut by the de-esser's static curve (threshold -24 dB, 6 dB knee,
/// ratio 4, range 10 dB), at levels under, in and over the knee and past the range's clamp.
/// 0.15 dB: the detector's ripple at 8 periods (0.05 dB) and the attack's tracking of its
/// crests.
#[test]
fn the_de_esser_cuts_a_sibilance_tone_by_its_static_curve() {
    for level in [-40.0, -30.0, -26.0, -24.0, -20.0, -14.0, -8.0, -2.0] {
        let x = sine(
            Hz::new(6_000.0),
            Amplitude::new(amplitude(level)),
            SampleRate::of(FS),
            Samples::new(RUN),
        );
        let y = render(de_esser(), &[&x]);
        let got = db(component(&y, 6_000.0) / component(&x, 6_000.0));
        let want = -reduction(level, -24.0, 6.0, 4.0).min(10.0);
        assert!(
            (got - want).abs() < 0.15,
            "{level} dB: cut {got} dB against the curve's {want}"
        );
    }
}

/// A low tone, loud, is not the de-esser's business: its level through the band-pass at
/// 6 kHz is far under the threshold, so the band stays flat and the tone passes at unity.
#[test]
fn the_de_esser_passes_a_low_tone() {
    let x = sine(
        Hz::new(200.0),
        Amplitude::new(amplitude(-6.0)),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let y = render(de_esser(), &[&x]);
    let got = db(component(&y, 200.0) / component(&x, 200.0));
    assert!(got.abs() < 0.01, "{got} dB");
}

/// With sibilance and a low tone together, the sibilance is cut by the curve at its own level
/// and the low tone passes by the bell's skirt at the cut's gain, `|H(200 Hz)|`, which is
/// within 0.1 dB of unity at Q 2 and a 10 dB cut.
#[test]
fn the_de_esser_cuts_the_sibilance_and_leaves_the_voice() {
    let hiss = sine(
        Hz::new(6_000.0),
        Amplitude::new(amplitude(-12.0)),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let voice = sine(
        Hz::new(200.0),
        Amplitude::new(amplitude(-8.0)),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let x: Vec<f32> = hiss.iter().zip(&voice).map(|(a, b)| a + b).collect();
    let y = render(de_esser(), &[&x]);
    // The voice's own contribution to the key is the band-pass's skirt at 200 Hz, 1/30 of
    // its amplitude and under the threshold, so the sibilance alone sets the cut.
    let cut = -reduction(-12.0, -24.0, 6.0, 4.0).min(10.0);
    let hiss_got = db(component(&y, 6_000.0) / component(&x, 6_000.0));
    let voice_got = db(component(&y, 200.0) / component(&x, 200.0));
    assert!((hiss_got - cut).abs() < 0.2, "{hiss_got} against {cut}");
    let skirt = bell_db(200.0, 6_000.0, 2.0, cut);
    assert!(
        (voice_got - skirt).abs() < 0.1,
        "{voice_got} against {skirt}"
    );
}

/// An upward band boosts a quiet band: the mirrored curve at the sine's level, within the
/// range, 0.15 dB as the de-esser's.
#[test]
fn an_upward_band_boosts_a_quiet_band_by_the_mirrored_curve() {
    let config = DynamicEqConfig::new(ChannelLayout::MONO, Hz::new(1_000.0), Q::new(1.5))
        .mode(DynamicMode::Upward)
        .threshold(Db::new(-20.0))
        .ratio(CompressionRatio::new(3.0))
        .range(Db::new(9.0))
        .attack(Seconds::new(0.001))
        .release(Seconds::new(0.02));
    for level in [-50.0, -36.0, -26.0, -20.0, -10.0] {
        let x = sine(
            Hz::new(1_000.0),
            Amplitude::new(amplitude(level)),
            SampleRate::of(FS),
            Samples::new(RUN),
        );
        let y = render(DynamicEqNode::<f32>::from(config), &[&x]);
        let got = db(component(&y, 1_000.0) / component(&x, 1_000.0));
        let want = reduction(2.0 * -20.0 - level, -20.0, 0.0, 3.0).min(9.0);
        assert!(
            (got - want).abs() < 0.15,
            "{level} dB: boost {got} dB against the curve's {want}"
        );
    }
}

/// A side-chain key moves the band the signal is shaped by: a quiet tone at the centre is cut
/// by the curve of the key's level, not its own, and the node has the key bus after its input.
#[test]
fn a_side_chain_key_sets_the_cut_of_the_signal() {
    let config = DynamicEqConfig::new(ChannelLayout::MONO, Hz::new(1_000.0), Q::new(2.0))
        .threshold(Db::new(-20.0))
        .ratio(CompressionRatio::new(4.0))
        .range(Db::new(20.0))
        .key(KeySource::Sidechain);
    let node = DynamicEqNode::<f32>::from(config);
    assert_eq!(node.shape().audio_in().count(), 2, "a signal and a key");
    let x = sine(
        Hz::new(1_000.0),
        Amplitude::new(0.05),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let key = sine(
        Hz::new(1_000.0),
        Amplitude::new(amplitude(-6.0)),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let y = render(node, &[&x, &key]);
    let got = db(component(&y, 1_000.0) / component(&x, 1_000.0));
    let want = -reduction(-6.0, -20.0, 0.0, 4.0);
    assert!((got - want).abs() < 0.1, "{got} against {want}");
}

/// A node whose key nobody wired is silence: a downward band stays flat.
#[test]
fn an_unwired_side_chain_leaves_a_downward_band_flat() {
    let config = DynamicEqConfig::new(ChannelLayout::MONO, Hz::new(1_000.0), Q::new(2.0))
        .threshold(Db::new(-20.0))
        .ratio(CompressionRatio::new(4.0))
        .key(KeySource::Sidechain);
    let x = sine(
        Hz::new(1_000.0),
        Amplitude::new(0.5),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let silence = vec![0.0f32; RUN];
    let y = render(DynamicEqNode::<f32>::from(config), &[&x, &silence]);
    assert!(db(component(&y, 1_000.0) / component(&x, 1_000.0)).abs() < 0.01);
}

/// With the key nobody wired, silence, an upward band sits at its full boost: the range.
#[test]
fn an_unwired_side_chain_gives_an_upward_band_its_full_range() {
    let config = DynamicEqConfig::new(ChannelLayout::MONO, Hz::new(1_000.0), Q::new(2.0))
        .mode(DynamicMode::Upward)
        .threshold(Db::new(-40.0))
        .ratio(CompressionRatio::new(4.0))
        .range(Db::new(9.0))
        .key(KeySource::Sidechain);
    let x = sine(
        Hz::new(1_000.0),
        Amplitude::new(0.1),
        SampleRate::of(FS),
        Samples::new(RUN),
    );
    let silence = vec![0.0f32; RUN];
    let y = render(DynamicEqNode::<f32>::from(config), &[&x, &silence]);
    let got = db(component(&y, 1_000.0) / component(&x, 1_000.0));
    assert!((got - 9.0).abs() < 0.1, "{got} dB against the range's 9");
}
