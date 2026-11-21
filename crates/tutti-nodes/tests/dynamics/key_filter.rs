//! The key filter of the compressor and the gate: the detector hears the key through an SVF,
//! and the signal the gain is applied to is never filtered. Every figure is from the closed
//! form of the filter's response (the RBJ cookbook's high-pass and constant-skirt-gain
//! band-pass under the bilinear map) and the compressor's static curve, computed here.
//!
//! The nodes run with instant ballistics, so the gain at a frame is the static curve at that
//! frame's filtered key level, and the deepest reduction (or the highest gain) over whole
//! cycles of a settled sine is the curve at the sine's peak, `amplitude * |H(f)|`.

use core::f64::consts::PI;
use tutti_buffers::Planes;

use tutti_buffers::BusTopology;
use tutti_dsp::{CompressorControls, GateControls, KeyFilter};
use tutti_graph::contract::{drive, prepared};
use tutti_graph::{Node, ParamNode};
use tutti_nodes::{CompressorConfig, CompressorNode, GateConfig, GateNode, KeySource};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;
const FS: f64 = 48_000.0;
const BLOCK: usize = 512;
/// The Q of a Butterworth section.
const BUTTERWORTH: f64 = core::f64::consts::FRAC_1_SQRT_2;
/// The audio the nodes gain: a constant, so the output is the gain times 0.5.
const AUDIO: f32 = 0.5;
/// The frames at the end of a settled run that the figure is read over: a whole number of
/// cycles of every test tone below (250, 1000 and 4000 Hz at 48 kHz), and long enough for the
/// peak to fall within a sample's phase of the true one.
const WINDOW: usize = 4800;

/// The closed-form magnitude of the 2nd-order section at `f` for a cutoff `fc` and `q`, in
/// dB: `numerator(w) / sqrt((1 - w^2)^2 + (w / q)^2)` at the prewarped `w = tan(pi f / fs) /
/// tan(pi fc / fs)`. The high-pass has `w^2` over it, the band-pass `w` (peak gain `q`).
fn response_db(filter: KeyFilter, f: f64, fc: f64, q: f64) -> f64 {
    let w = (PI * f / FS).tan() / (PI * fc / FS).tan();
    let denominator = (1.0 - w * w).hypot(w / q);
    let numerator = match filter {
        KeyFilter::HighPass => w * w,
        KeyFilter::BandPass => w,
        KeyFilter::Off => unreachable!("no response"),
    };
    20.0 * (numerator / denominator).log10()
}

/// The compressor's hard-knee static reduction at `level` dB: `(level - threshold)
/// (1 - 1/ratio)` over the threshold, none under it.
fn curve_db(level: f64, threshold: f64, ratio: f64) -> f64 {
    (level - threshold).max(0.0) * (1.0 - 1.0 / ratio)
}

/// How far under a tone's true peak, in dB, the largest of its samples can fall: the
/// samples of a tone at `f` are `2 pi f / fs` of phase apart, so one is within half of that
/// of the crest, `cos(pi f / fs)` of its height, plus the 0.02 dB of `f32` and of the filter's
/// own state.
fn sampled_peak_tolerance_db(f: f64) -> f64 {
    0.02 - 20.0 * (PI * f / FS).cos().log10()
}

fn amplitude_db(a: f64) -> f64 {
    20.0 * a.log10()
}

/// `frames` of an amplitude-`a` sine at `f`, from phase 0.
fn tone(f: f64, a: f64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| (a * (2.0 * PI * f * i as f64 / FS).sin()) as f32)
        .collect()
}

/// `node` over `key` in blocks, its audio the constant [`AUDIO`]; the output.
fn run<N: Node>(node: &mut N, key: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    for block in key.chunks(BLOCK) {
        let audio = vec![AUDIO; block.len()];
        out.extend(drive(node, SR, Planes::new(&[&audio, block]).unwrap(), &[]).remove(0));
    }
    out
}

/// The deepest reduction over the last [`WINDOW`] frames of `out`, in dB.
fn deepest_reduction_db(out: &[f32]) -> f64 {
    let lowest = out[out.len() - WINDOW..]
        .iter()
        .fold(f32::MAX, |m, &x| m.min(x));
    -amplitude_db(f64::from(lowest) / f64::from(AUDIO))
}

/// The highest gain over the last [`WINDOW`] frames of `out`.
fn highest_gain(out: &[f32]) -> f64 {
    let highest = out[out.len() - WINDOW..]
        .iter()
        .fold(f32::MIN, |m, &x| m.max(x));
    f64::from(highest) / f64::from(AUDIO)
}

const THRESHOLD: f64 = -40.0;
const RATIO: f64 = 4.0;

fn compressor_controls(cutoff: f64, q: f64) -> CompressorControls {
    CompressorControls {
        threshold: Db::new(THRESHOLD),
        ratio: CompressionRatio::new(RATIO),
        key_cutoff: Hz::new(cutoff),
        key_q: Q::new(q),
        ..CompressorControls::DEFAULT
    }
}

fn compressor(filter: KeyFilter, cutoff: f64, q: f64) -> CompressorNode<f32> {
    CompressorNode::<f32>::from(
        CompressorConfig::new(BusTopology::MONO)
            .key(KeySource::Sidechain)
            .key_filter(filter)
            .controls(compressor_controls(cutoff, q)),
    )
}

/// A key through a high-pass is reduced by the static curve at the key's level plus the
/// filter's analytic magnitude: a tone under the cutoff is attenuated by it, a tone well over
/// the cutoff is not.
#[test]
fn a_high_passed_key_reduces_by_the_curve_at_its_filtered_level() {
    let (fc, q, a) = (1_000.0, BUTTERWORTH, 0.5);
    for (f, attenuated) in [(250.0, true), (125.0, true), (4_000.0, false)] {
        let mut node = prepared(compressor(KeyFilter::HighPass, fc, q), SR, 1_024);
        let out = run(&mut node, &tone(f, a, 24_000));
        let got = deepest_reduction_db(&out);
        let h = response_db(KeyFilter::HighPass, f, fc, q);
        let want = curve_db(amplitude_db(a) + h, THRESHOLD, RATIO);
        let tolerance = sampled_peak_tolerance_db(f);
        assert!(
            (got - want).abs() < tolerance,
            "{f} Hz: {got} dB, curve says {want} dB (within {tolerance} dB)"
        );
        let unfiltered = curve_db(amplitude_db(a), THRESHOLD, RATIO);
        if attenuated {
            assert!(
                want < unfiltered - 5.0,
                "{f} Hz: the tone is filtered down by {h} dB, so this run sees the filter"
            );
        } else {
            assert!(
                (got - unfiltered).abs() < sampled_peak_tolerance_db(f) + 0.05,
                "{f} Hz is well over the cutoff: {got} dB against the unfiltered {unfiltered} dB"
            );
        }
    }
}

/// A band-pass key is reduced by the curve at the key's level plus the band-pass's analytic
/// magnitude: its peak gain `q` at the centre, and the skirt below it.
#[test]
fn a_band_passed_key_reduces_by_the_curve_at_its_filtered_level() {
    let (fc, q, a) = (1_000.0, 2.0, 0.05);
    for f in [1_000.0, 125.0, 4_000.0] {
        let mut node = prepared(compressor(KeyFilter::BandPass, fc, q), SR, 1_024);
        let out = run(&mut node, &tone(f, a, 24_000));
        let got = deepest_reduction_db(&out);
        let h = response_db(KeyFilter::BandPass, f, fc, q);
        let want = curve_db(amplitude_db(a) + h, THRESHOLD, RATIO);
        let tolerance = sampled_peak_tolerance_db(f);
        assert!(
            (got - want).abs() < tolerance,
            "{f} Hz: {got} dB, curve says {want} dB (within {tolerance} dB)"
        );
    }
    let centre = response_db(KeyFilter::BandPass, fc, fc, q);
    assert!(
        (centre - amplitude_db(q)).abs() < 1e-9,
        "the band-pass peaks at q: {centre} dB"
    );
}

/// A NaN or an infinite key sample does not latch the filter: once the key is finite again,
/// the reduction is back on the curve at the filtered level, a tone under the cutoff
/// attenuated by it.
#[test]
fn a_non_finite_key_sample_does_not_latch_the_key_filter() {
    let (f, fc, q, a) = (250.0, 1_000.0, BUTTERWORTH, 0.5);
    let want = curve_db(
        amplitude_db(a) + response_db(KeyFilter::HighPass, f, fc, q),
        THRESHOLD,
        RATIO,
    );
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut node = prepared(compressor(KeyFilter::HighPass, fc, q), SR, 1_024);
        let mut key = tone(f, a, 24_000);
        key[6_000] = bad;
        let out = run(&mut node, &key);
        let got = deepest_reduction_db(&out);
        let tolerance = sampled_peak_tolerance_db(f);
        assert!(
            (got - want).abs() < tolerance,
            "after a {bad} key sample: {got} dB, curve says {want} dB (within {tolerance} dB)"
        );
    }
}

/// Setting the cutoff through the node's `ParamSet` moves the filter: the reduction at 1 kHz
/// falls from the curve at the unfiltered level to the curve at the level a 4 kHz high-pass
/// leaves.
#[test]
fn the_cutoff_param_moves_the_key_filter() {
    let (f, q, a) = (1_000.0, BUTTERWORTH, 0.5);
    let node = compressor(KeyFilter::HighPass, 100.0, q);
    let set = node.param_set();
    let mut node = prepared(node, SR, 1_024);
    let key = tone(f, a, 48_000);
    let before = run(&mut node, &key[..24_000]);
    assert!(set.set(UnitParam::Cutoff, 4_000.0));
    let after = run(&mut node, &key[24_000..]);
    for (out, cutoff) in [(before, 100.0), (after, 4_000.0)] {
        let want = curve_db(
            amplitude_db(a) + response_db(KeyFilter::HighPass, f, cutoff, q),
            THRESHOLD,
            RATIO,
        );
        let got = deepest_reduction_db(&out);
        // A 1 kHz tone's sampled peak is within `sampled_peak_tolerance_db(1 kHz)`, 0.022 dB,
        // of its crest; 0.05 dB holds that with room for the ramp's last step.
        assert!(
            (got - want).abs() < 0.05,
            "cutoff {cutoff}: {got} dB, curve says {want} dB"
        );
    }
}

/// Setting the Q through the `ParamSet` moves the band-pass's peak: its gain at the centre is
/// the new `q`.
#[test]
fn the_q_param_moves_the_key_filter() {
    let (fc, a) = (1_000.0, 0.05);
    let node = compressor(KeyFilter::BandPass, fc, 1.0);
    let set = node.param_set();
    let mut node = prepared(node, SR, 1_024);
    let key = tone(fc, a, 48_000);
    let before = run(&mut node, &key[..24_000]);
    assert!(set.set(UnitParam::Q, 4.0));
    let after = run(&mut node, &key[24_000..]);
    for (out, q) in [(before, 1.0), (after, 4.0)] {
        let want = curve_db(
            amplitude_db(a) + response_db(KeyFilter::BandPass, fc, fc, q),
            THRESHOLD,
            RATIO,
        );
        let got = deepest_reduction_db(&out);
        // A 1 kHz tone's sampled peak is within `sampled_peak_tolerance_db(1 kHz)`, 0.022 dB,
        // of its crest; 0.05 dB holds that with room for the ramp's last step.
        assert!(
            (got - want).abs() < 0.05,
            "q {q}: {got} dB, curve says {want} dB"
        );
    }
}

/// The audio path is untouched: with a key of alternating samples (the Nyquist tone, which a
/// high-pass passes at unity, so the key level is constant) and a high-pass at 1 kHz, a 440 Hz
/// tone through the node is the tone times the one gain of the static curve, which a filter on
/// the audio would have coloured.
#[test]
fn the_audio_is_gained_by_the_curve_and_never_filtered() {
    let a = 0.5;
    let reduction = curve_db(amplitude_db(a), -20.0, RATIO);
    let gain = 10f64.powf(-reduction / 20.0);
    let controls = CompressorControls {
        threshold: Db::new(-20.0),
        ..compressor_controls(1_000.0, BUTTERWORTH)
    };
    let mut node = prepared(
        CompressorNode::<f32>::from(
            CompressorConfig::new(BusTopology::MONO)
                .key(KeySource::Sidechain)
                .key_filter(KeyFilter::HighPass)
                .controls(controls),
        ),
        SR,
        1_024,
    );
    let audio = tone(440.0, 0.3, 4_800);
    let key: Vec<f32> = (0..audio.len())
        .map(|i| if i % 2 == 0 { a as f32 } else { -a as f32 })
        .collect();
    let mut out = Vec::new();
    for (x, k) in audio.chunks(BLOCK).zip(key.chunks(BLOCK)) {
        out.extend(drive(&mut node, SR, Planes::new(&[x, k]).unwrap(), &[]).remove(0));
    }
    for (i, (&y, &x)) in out.iter().zip(&audio).enumerate().skip(1_000) {
        let want = f64::from(x) * gain;
        assert!(
            (f64::from(y) - want).abs() < 2e-5,
            "frame {i}: {y} against {want}"
        );
    }
}

fn gate_controls(threshold: f64, cutoff: f64, q: f64) -> GateControls {
    GateControls {
        threshold: Db::new(threshold),
        range: Db::new(-80.0),
        key_cutoff: Hz::new(cutoff),
        key_q: Q::new(q),
        ..GateControls::DEFAULT
    }
}

fn gate(filter: KeyFilter, threshold: f64, cutoff: f64, q: f64) -> GateNode<f32> {
    GateNode::<f32>::from(
        GateConfig::new(BusTopology::MONO)
            .key(KeySource::Sidechain)
            .key_filter(filter)
            .controls(gate_controls(threshold, cutoff, q)),
    )
}

/// A high-passed gate opens on a key whose filtered peak is over the threshold and stays closed
/// on one under it: the thresholds sit 0.3 dB either side of the analytic peak of a 250 Hz tone
/// through a 1 kHz high-pass, where an unfiltered gate opens on both.
#[test]
fn a_high_passed_gate_decides_on_the_filtered_key() {
    let (f, fc, q, a) = (250.0, 1_000.0, BUTTERWORTH, 0.5);
    let peak = amplitude_db(a) + response_db(KeyFilter::HighPass, f, fc, q);
    let key = tone(f, a, 24_000);
    for (threshold, opens) in [(peak - 0.3, true), (peak + 0.3, false)] {
        let mut filtered = prepared(gate(KeyFilter::HighPass, threshold, fc, q), SR, 1_024);
        let gain = highest_gain(&run(&mut filtered, &key));
        let mut plain = prepared(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(gate_controls(threshold, fc, q)),
            ),
            SR,
            1_024,
        );
        assert!(
            highest_gain(&run(&mut plain, &key)) > 0.999,
            "the unfiltered key peaks at {} dB, over the threshold {threshold} dB",
            amplitude_db(a)
        );
        if opens {
            assert!(
                gain > 0.999,
                "peak {peak} dB over threshold {threshold} dB: gain {gain}"
            );
        } else {
            assert!(
                gain < 1e-3,
                "peak {peak} dB under threshold {threshold} dB: gain {gain}"
            );
        }
    }
}

/// The gate's cutoff param moves its filter: a 1 kHz key opens a gate whose high-pass sits at
/// 100 Hz and, once the cutoff is 4 kHz (a filtered peak 23 dB down), leaves it closed.
#[test]
fn the_gates_cutoff_param_moves_the_key_filter() {
    let (f, q, a) = (1_000.0, BUTTERWORTH, 0.5);
    let threshold = amplitude_db(a) - 10.0;
    assert!(response_db(KeyFilter::HighPass, f, 4_000.0, q) < -20.0);
    let node = gate(KeyFilter::HighPass, threshold, 100.0, q);
    let set = node.param_set();
    let mut node = prepared(node, SR, 1_024);
    let key = tone(f, a, 48_000);
    let before = run(&mut node, &key[..24_000]);
    assert!(set.set(UnitParam::Cutoff, 4_000.0));
    let after = run(&mut node, &key[24_000..]);
    assert!(highest_gain(&before) > 0.999, "open at 100 Hz");
    assert!(highest_gain(&after) < 1e-3, "closed at 4 kHz");
}

/// The gate's audio is untouched: a Nyquist key under the threshold holds the gate at its
/// range and one over it opens the gate, and in both the 440 Hz tone is the tone times the
/// range's gain or unity, whatever the filter is.
#[test]
fn the_gate_applies_its_gain_to_the_unfiltered_audio() {
    let a = 0.5;
    let audio = tone(440.0, 0.3, 4_800);
    let key: Vec<f32> = (0..audio.len())
        .map(|i| if i % 2 == 0 { a as f32 } else { -a as f32 })
        .collect();
    for (threshold, gain) in [(-3.0, 1e-4), (-20.0, 1.0)] {
        let mut node = prepared(
            gate(KeyFilter::HighPass, threshold, 1_000.0, BUTTERWORTH),
            SR,
            1_024,
        );
        let mut out = Vec::new();
        for (x, k) in audio.chunks(BLOCK).zip(key.chunks(BLOCK)) {
            out.extend(drive(&mut node, SR, Planes::new(&[x, k]).unwrap(), &[]).remove(0));
        }
        for (i, (&y, &x)) in out.iter().zip(&audio).enumerate().skip(1_000) {
            let want = f64::from(x) * gain;
            assert!(
                (f64::from(y) - want).abs() < 2e-5 * gain.max(1e-3),
                "threshold {threshold} dB, frame {i}: {y} against {want}"
            );
        }
    }
}

/// `Off` declares no key-filter control: the compressor's six and the gate's five are all the
/// node has, and writing a cutoff or Q reaches nothing. A node with a filter declares the
/// cutoff and Q after them, at the values it was built with, and the controls it had keep their
/// values.
#[test]
fn off_declares_no_controls_and_a_filter_declares_two_more() {
    let controls = CompressorControls {
        makeup: Db::new(3.0),
        ..compressor_controls(2_000.0, 1.5)
    };
    let config = CompressorConfig::new(BusTopology::MONO)
        .key(KeySource::Sidechain)
        .controls(controls);
    let off = CompressorNode::<f32>::from(config);
    assert_eq!(off.shape().params.len(), 6);
    assert!(!off.param_set().set(UnitParam::Cutoff, 500.0));
    assert!(!off.param_set().set(UnitParam::Q, 2.0));
    let on = CompressorNode::<f32>::from(config.key_filter(KeyFilter::BandPass));
    assert_eq!(
        &on.shape().params.as_slice()[6..],
        &[UnitParam::Cutoff, UnitParam::Q][..]
    );
    let bases: Vec<_> = (0..8).map(|k| on.param_base(k)).collect();
    assert_eq!(
        bases,
        [-40.0, 0.0, 4.0, 0.0, 0.0, 3.0, 2_000.0, 1.5].map(Some)
    );

    let gate = gate(KeyFilter::Off, -30.0, 100.0, 1.0);
    assert_eq!(gate.shape().params.len(), 6);
    let gate = self::gate(KeyFilter::HighPass, -30.0, 100.0, 1.0);
    assert_eq!(
        &gate.shape().params.as_slice()[6..],
        &[UnitParam::Cutoff, UnitParam::Q][..]
    );
}
