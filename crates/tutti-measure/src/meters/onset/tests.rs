use super::*;
use crate::fold_uneven;

const SAMPLE_RATE: f64 = 44100.0;

fn rate() -> SampleRate {
    SampleRate::of(SAMPLE_RATE)
}

fn plan(cfg: OnsetConfig) -> OnsetPlan {
    cfg.plan(rate()).unwrap()
}

fn flux() -> OnsetConfig {
    OnsetConfig::new(DetectionFunction::SpectralFlux)
}

fn detect_at(cfg: &OnsetConfig, samples: &[f64]) -> Vec<Onset> {
    detect_onsets(cfg, Mono::new(samples), rate()).unwrap()
}

fn amplitude(a: f64) -> OnsetThreshold {
    OnsetThreshold::of(Amplitude::new(a))
}

fn gain(a: f64) -> OnsetSensitivity {
    OnsetSensitivity::of(Amplitude::new(a))
}

fn gap(seconds: f64) -> OnsetGap {
    OnsetGap::of(Seconds::new(seconds))
}

/// The curve has one entry per frame, at the frame's start sample, scaled by
/// the config's sensitivity, and a click raises the frame that holds it.
#[test]
fn the_novelty_curve_is_readable_per_frame_and_scaled_by_sensitivity() {
    let samples = signal(0.5, &[0.2]);
    let run = |factor: f64| {
        let mut meter = OnsetConfig::new(DetectionFunction::Energy)
            .sensitivity(gain(factor))
            .measure(rate())
            .unwrap();
        meter.push(Mono::new(&samples), &mut Vec::new()).unwrap();
        meter
    };
    let (one, three) = (run(1.0), run(3.0));
    for (i, n) in one.novelty().iter().enumerate() {
        assert_eq!(n.frame, Samples::new(i * 512));
    }
    for (a, b) in one.novelty().iter().zip(three.novelty()) {
        assert!((b.value - 3.0 * a.value).abs() <= 1e-12 * a.value.max(1.0));
    }
    // Frames holding the click (samples 8820..8870) carry its energy; a
    // frame wholly before it carries none.
    let at = |s: usize| one.novelty()[s / 512].value;
    assert!(at(8820) > 1.0 && at(2048) == 0.0);

    let mut cleared = one;
    cleared.reset();
    assert!(cleared.novelty().is_empty());
}

fn signal(seconds: f64, onsets: &[f64]) -> Vec<f64> {
    let n = (SAMPLE_RATE * seconds) as usize;
    let mut samples = vec![0.0f64; n];
    for &t in onsets {
        let pos = (t * SAMPLE_RATE) as usize;
        for i in 0..50.min(n.saturating_sub(pos)) {
            samples[pos + i] += (-0.1 * i as f64).exp() * 0.8;
        }
    }
    samples
}

/// The property the explicit carry buys: a meter fed chunks of any size
/// and the one-shot agree bit for bit, because they run the same step.
#[test]
fn a_meter_fed_in_chunks_equals_the_one_shot() {
    let cfg = flux().threshold(amplitude(0.2)).sensitivity(gain(2.0));
    let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);

    let batch = detect_at(&cfg, &samples);
    assert_eq!(batch.len(), 4);

    for chunk in [1usize, 7, 511, 2048, 3001, samples.len()] {
        let mut meter = cfg.measure(rate()).unwrap();
        for part in samples.chunks(chunk) {
            meter.push(Mono::new(part), &mut Vec::new()).unwrap();
        }
        let curve = meter.novelty().to_vec();
        assert_eq!(meter.finish(), batch, "chunk {chunk}");

        let reference = cfg.measure(rate()).unwrap();
        let mut whole = reference;
        whole.push(Mono::new(&samples), &mut Vec::new()).unwrap();
        assert_eq!(curve, whole.novelty(), "chunk {chunk}");
    }
}

/// Every sample format reads at its nominal level: a 16-bit copy of the
/// signal gives the novelty of the `f64` signal to within quantisation.
#[test]
fn an_integer_buffer_is_read_at_its_nominal_level() {
    let cfg = flux();
    let samples = signal(0.5, &[0.1, 0.3]);
    let ints: Vec<i16> = samples
        .iter()
        .map(|&s| (s * f64::from(i16::MAX)) as i16)
        .collect();
    let mut float = cfg.measure(rate()).unwrap();
    float.push(Mono::new(&samples), &mut Vec::new()).unwrap();
    let mut int = cfg.measure(rate()).unwrap();
    int.push(Mono::new(&ints), &mut Vec::new()).unwrap();
    assert_eq!(float.novelty().len(), int.novelty().len());
    for (a, b) in float.novelty().iter().zip(int.novelty()) {
        assert!(
            (a.value - b.value).abs() <= 1e-2 * a.value.max(1.0),
            "{a:?} {b:?}"
        );
    }
}

/// A reused meter must be reset; a fresh one needs nothing.
#[test]
fn a_reset_meter_matches_a_fresh_one() {
    let a = signal(0.5, &[0.1, 0.3]);
    let b = signal(0.5, &[0.2]);

    let mut reused = flux().measure(rate()).unwrap();
    reused.push(Mono::new(&a), &mut Vec::new()).unwrap();
    reused.reset();
    reused.push(Mono::new(&b), &mut Vec::new()).unwrap();

    let mut fresh = flux().measure(rate()).unwrap();
    fresh.push(Mono::new(&b), &mut Vec::new()).unwrap();

    assert_eq!(reused.finish(), fresh.finish());
}

/// The per-frame spectrum buffer is allocated once, not per frame.
///
/// A fresh `vec![Complex64; bins]` per frame cost one allocation per
/// frame, tens of thousands for a few minutes of audio.
///
/// Asserted by **reserving past the buffer and seeing the reserve
/// survive**: `capacity()` is identical for a refilled buffer and a fresh
/// `vec![_; bins]`, but a buffer that is *replaced* loses the spare
/// capacity and one refilled in place keeps it.
#[test]
fn the_spectrum_buffer_is_reused_across_frames() {
    let mut meter = flux().measure(rate()).unwrap();
    let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
    let bins = meter.spectrum.len();
    assert!(bins > 0);

    meter.spectrum.reserve_exact(bins * 4);
    let reserved = meter.spectrum.capacity();
    assert!(reserved > bins, "the reserve must actually over-allocate");

    meter.push(Mono::new(&samples), &mut Vec::new()).unwrap();

    assert!(meter.novelty().len() > 8, "the run must cover many frames");
    assert_eq!(meter.spectrum.len(), bins, "one frame of bins");
    assert_eq!(
        meter.spectrum.capacity(),
        reserved,
        "every frame must refill the same allocation"
    );
}

#[test]
fn onsets_land_near_the_known_spikes() {
    let truth = [0.1f64, 0.3, 0.5, 0.7];
    let samples = signal(1.0, &truth);

    for function in [
        DetectionFunction::SpectralFlux,
        DetectionFunction::HighFrequencyContent,
        DetectionFunction::SpectralDistance,
    ] {
        let cfg = OnsetConfig::new(function)
            .threshold(amplitude(0.2))
            .sensitivity(gain(2.0));
        let onsets = detect_at(&cfg, &samples);

        assert_eq!(onsets.len(), truth.len(), "{function:?}: wrong count");

        // Reported early by up to one window — a frame is attributed to
        // its start offset, so the error grows with how deep into the
        // frame the spike falls.
        let tolerance = plan(cfg).geometry().window().get() as f64 / SAMPLE_RATE;
        for (found, &expected) in onsets.iter().zip(&truth) {
            let t = found.time(SampleRate::of(SAMPLE_RATE)).get();
            assert!(
                (t - expected).abs() <= tolerance,
                "{function:?}: onset at {t} is not within one window of {expected}"
            );
        }
    }
}

/// `Energy` misses these. Pinned so the
/// weakness stays a known property rather than becoming a silent change.
#[test]
fn energy_misses_short_decaying_spikes() {
    let cfg = OnsetConfig::new(DetectionFunction::Energy)
        .threshold(amplitude(0.2))
        .sensitivity(gain(2.0));
    let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);

    assert!(detect_at(&cfg, &samples).is_empty());
}

/// The defaults a config documents: a 0.3 threshold multiplier, unity
/// sensitivity, a 50 ms gap, and frames of 2048 samples every 512.
#[test]
fn the_defaults_are_the_documented_ones() {
    let p = plan(flux());
    assert_eq!(p.threshold, 0.3);
    assert_eq!(p.sensitivity, 1.0);
    assert_eq!(p.min_gap, Samples::new(2205));
    assert_eq!(p.geometry().window(), Samples::new(2048));
    assert_eq!(p.geometry().hop(), Samples::new(512));
}

#[test]
fn the_gap_is_expressed_in_seconds() {
    let cfg = flux().min_gap(gap(0.05));
    assert_eq!(plan(cfg).min_gap, Samples::new(2205));
    assert_eq!(plan(cfg.min_gap(gap(0.5))).min_gap, Samples::new(22050));
    // The same seconds are twice the samples at twice the rate.
    assert_eq!(
        cfg.plan(SampleRate::of(88200.0)).unwrap().min_gap,
        Samples::new(4410)
    );
}

#[test]
fn a_wide_gap_suppresses_close_onsets() {
    let cfg = flux().threshold(amplitude(0.2)).sensitivity(gain(2.0));
    let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);

    let baseline = detect_at(&cfg, &samples).len();
    let widened = detect_at(&cfg.min_gap(gap(0.5)), &samples);

    assert!(widened.len() < baseline);
}

#[test]
fn suppression_keeps_the_stronger_of_a_close_pair() {
    let onset = |pos: usize, strength: f64| Onset {
        position: Samples::new(pos),
        strength: Amplitude::new(strength),
    };

    let mut onsets = vec![onset(100, 0.4), onset(150, 0.9), onset(9000, 0.5)];
    suppress_close_onsets(Samples::new(1000), &mut onsets);
    assert_eq!(onsets.len(), 2);
    assert_eq!(onsets[0].strength, Amplitude::new(0.9));

    let mut onsets = vec![onset(100, 0.9), onset(150, 0.4), onset(9000, 0.5)];
    suppress_close_onsets(Samples::new(1000), &mut onsets);
    assert_eq!(onsets[0].strength, Amplitude::new(0.9));

    // Degenerate inputs are no-ops.
    let mut single = vec![onset(100, 0.5)];
    suppress_close_onsets(Samples::new(1000), &mut single);
    assert_eq!(single.len(), 1);
}

/// Each onset is compared with the last one *kept*, not with its
/// neighbour in the input: a falling run 600 apart under a 1000 gap keeps
/// 0 (600 is too close to it), then 1200 (1200 from 0), then drops 1800.
#[test]
fn suppression_measures_from_the_last_kept_onset() {
    let at = |p: usize, s: f64| Onset {
        position: Samples::new(p),
        strength: Amplitude::new(s),
    };
    let mut onsets = vec![at(0, 1.0), at(600, 0.9), at(1200, 0.8), at(1800, 0.7)];
    suppress_close_onsets(Samples::new(1000), &mut onsets);
    let kept: Vec<usize> = onsets.iter().map(|o| o.position.get()).collect();
    assert_eq!(kept, [0, 1200]);
}

/// Unsorted input must not underflow.
///
/// `pick_peaks` emits ascending positions, but the function is public and
/// takes a plain `Vec`. Subtracting `usize` positions would panic in debug
/// and wrap to a huge value in release, where the pair would silently never
/// be suppressed.
#[test]
fn suppression_handles_unsorted_input() {
    let onset = |pos: usize, strength: f64| Onset {
        position: Samples::new(pos),
        strength: Amplitude::new(strength),
    };

    let mut descending = vec![onset(9000, 0.4), onset(100, 0.9)];
    suppress_close_onsets(Samples::new(1000), &mut descending);
    assert_eq!(descending.len(), 2, "far apart in either direction");

    let mut close = vec![onset(150, 0.4), onset(100, 0.9)];
    suppress_close_onsets(Samples::new(1000), &mut close);
    assert_eq!(close.len(), 1);
    assert_eq!(close[0].strength, Amplitude::new(0.9));
}

#[test]
fn input_shorter_than_a_window_yields_nothing() {
    assert!(detect_at(&flux(), &[0.0; 100]).is_empty());
    let mut meter = flux().measure(rate()).unwrap();
    meter
        .push(Mono::new(&[0.0f64; 100]), &mut Vec::new())
        .unwrap();
    assert!(meter.novelty().is_empty());
    assert!(meter.finish().is_empty());
}

/// A zero window or hop is refused by the call that takes the rate.
#[test]
fn a_zero_framing_is_refused_where_the_rate_binds() {
    for framing in [
        Framing::new(Samples::new(0), Samples::new(512)),
        Framing::new(Samples::new(2048), Samples::new(0)),
    ] {
        let cfg = flux().framing(framing);
        assert!(matches!(
            Measure::<f64>::measure(&cfg, rate()),
            Err(crate::Error::ZeroWindow | crate::Error::ZeroHop)
        ));
        assert!(matches!(
            detect_onsets(&cfg, Mono::new(&[0.0f64; 4096]), rate()),
            Err(crate::Error::ZeroWindow | crate::Error::ZeroHop)
        ));
    }
}

/// The trait fold over uneven chunks (1, 7, 64 samples and the rest)
/// yields the novelty curve as its items and `detect_onsets` as its output.
#[test]
fn the_trait_fold_over_uneven_chunks_equals_detect_onsets_with_the_curve_as_items() {
    let cfg = flux().threshold(amplitude(0.2)).sensitivity(gain(2.0));
    let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
    let batch = detect_at(&cfg, &samples);
    assert_eq!(batch.len(), 4);
    let (items, onsets) = fold_uneven(&cfg, rate(), samples.len(), |meter, at, out| {
        meter.push(Mono::new(&samples[at]), out)
    })
    .unwrap();
    assert_eq!(onsets, batch);
    let mut whole = cfg.measure(rate()).unwrap();
    whole.push(Mono::new(&samples), &mut Vec::new()).unwrap();
    assert_eq!(items, whole.novelty());
}

/// A novelty curve of 10 minutes at 192 kHz (225 000 frames at a hop of 512)
/// alternating `LEVEL ± 1` has mean `LEVEL` and standard deviation exactly 1,
/// so its threshold is `LEVEL + 3 * k` for a multiplier `k`: the up-frames
/// (value `LEVEL + 1`) are peaks for `k = 0.3` (threshold `LEVEL + 0.9`) and
/// none is for `k = 0.4` (threshold `LEVEL + 1.2`). The one-pass form
/// `sum_sq / n - mean^2` cannot resolve that spread at the larger levels: at
/// 1e8 the squares near 1e16 sit 2 apart in `f64`, and in `f32` at 1e4 it read
/// a variance of 264536 for the true 1.
#[test]
fn a_long_novelty_curve_keeps_its_spread_at_a_large_level() {
    const FRAMES: usize = 225_000;
    for level in [1.0e4, 1.0e8, 1.0e10] {
        let novelty: Vec<Novelty> = (0..FRAMES)
            .map(|i| Novelty {
                frame: Samples::new(i * 512),
                value: if i % 2 == 0 { level + 1.0 } else { level - 1.0 },
            })
            .collect();
        let at = |k: f64| pick_peaks(&plan(flux().threshold(amplitude(k))), &novelty).len();
        assert_eq!(
            at(0.3),
            FRAMES / 2 - 1,
            "level {level}: every interior up-frame"
        );
        assert_eq!(at(0.4), 0, "level {level}");
    }
}
