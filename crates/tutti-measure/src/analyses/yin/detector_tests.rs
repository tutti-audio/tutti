use super::*;
use tutti_fixtures::uniform_noise;
use tutti_units::SampleRate;

const SR: SampleRate = SampleRate::SR_48K;
const N: usize = 4096;

fn sine(freq: Hz, amp: f64) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        freq,
        tutti_units::Amplitude::new(amp),
        SR,
        tutti_units::Samples::new(N),
    )
}

fn saw(freq: Hz, amp: f64) -> Vec<f64> {
    let f = freq.get();
    (0..N)
        .map(|i| {
            let phase = (f * i as f64 / SR.get()).fract();
            (2.0 * phase - 1.0) * amp
        })
        .collect()
}

fn detector() -> PitchDetector {
    PitchDetector::new(YinConfig::new().plan(SR).unwrap())
}

fn period_samples(freq: Hz) -> f64 {
    SR.get() / freq.get()
}

fn fill_cmndf(det: &mut PitchDetector, samples: &[f64]) -> (usize, usize) {
    let (min_period, max_period) = det.periods(samples.len());
    det.compute_difference(samples, max_period);
    det.compute_cumulative_mean(max_period);
    (min_period, max_period)
}

/// d'(0) is 1 by definition, for any signal the difference function can see.
#[test]
fn cmndf_equals_one_at_lag_zero() {
    let mut det = detector();
    fill_cmndf(&mut det, &sine(Hz::new(440.0), 1.0));
    assert!(
        (det.cumulative_mean[0] - 1.0).abs() < f64::EPSILON,
        "d'(0) must be 1 by definition, got {}",
        det.cumulative_mean[0]
    );
}

/// A pure sine's CMNDF dips below the YIN threshold at the true period.
#[test]
fn cmndf_dips_below_threshold_at_the_true_period() {
    let freq = Hz::new(440.0);
    let tau = period_samples(freq).round() as usize;
    let mut det = detector();
    fill_cmndf(&mut det, &sine(freq, 1.0));
    assert!(
        det.cumulative_mean[tau] < det.cfg.threshold.get(),
        "d'({tau}) = {} was not below threshold {}",
        det.cumulative_mean[tau],
        det.cfg.threshold.get()
    );
}

/// The first dip below threshold wins, even when a later (octave-down) dip
/// is deeper. A 110 Hz saw is periodic at T *and* 2T; 2T is the global
/// minimum — the second-harmonic dip of the period — but the first
/// below-threshold dip is the fundamental.
#[test]
fn first_local_minimum_beats_a_deeper_later_dip() {
    let freq = Hz::new(110.0);
    let true_period = period_samples(freq);
    let tau = true_period.round() as usize;
    let tau_2t = (true_period * 2.0).round() as usize;

    let mut det = detector();
    let (min_period, max_period) = fill_cmndf(&mut det, &saw(freq, 0.5));

    let dip_t = det.cumulative_mean[tau];
    let dip_2t = det.cumulative_mean[tau_2t];
    assert!(
        dip_2t < dip_t,
        "precondition: the 2T dip ({dip_2t}) must be deeper than the T dip ({dip_t})"
    );
    assert!(
        dip_t < det.cfg.threshold.get(),
        "precondition: the fundamental dip {dip_t} must be below threshold"
    );

    let (picked, _) = det.find_best_period_full(min_period, max_period);
    assert!(
        (picked as f64 - true_period).abs() < 2.0,
        "first-local-minimum should return ~{true_period:.1} (fundamental), got {picked}"
    );
    assert!(
        (picked as f64 - tau_2t as f64).abs() > 10.0,
        "picked the deeper 2T dip at {picked}, an octave down"
    );
}

/// Parabolic interpolation recovers a half-sample period; the integer lag
/// alone is off by half a sample.
#[test]
fn parabolic_interpolation_recovers_a_half_sample_period() {
    let true_period = 100.5;
    let freq = Hz::new(SR.get() / true_period);
    let mut det = detector();
    let (min_period, max_period) = fill_cmndf(&mut det, &sine(freq, 1.0));
    let (tau, _) = det.find_best_period_full(min_period, max_period);
    let interpolated = det.parabolic_interpolation(tau, max_period);

    assert!(
        (tau as f64 - true_period).abs() > 0.4,
        "un-interpolated integer lag {tau} should be ~0.5 samples off {true_period}"
    );
    assert!(
        (interpolated - true_period).abs() < 0.05,
        "interpolated period {interpolated} should be within 0.05 of {true_period}"
    );
}

/// All-zero, DC, and white-noise buffers are unvoiced: no lag falls below
/// threshold, so the detector must not invent a pitch.
#[test]
fn aperiodic_buffers_are_unvoiced() {
    for (label, samples) in [
        ("zeros", vec![0.0f64; N]),
        ("dc", vec![0.5f64; N]),
        (
            "noise",
            uniform_noise::<f64>(1, N).iter().map(|v| v * 0.5).collect(),
        ),
    ] {
        let mut det = detector();
        let (min_period, max_period) = fill_cmndf(&mut det, &samples);
        let (period, aperiodicity) = det.find_best_period_full(min_period, max_period);
        assert_eq!(
            period, 0,
            "{label}: a lag of {period} (aperiodicity {aperiodicity}) fell out of an aperiodic buffer"
        );

        let mut det = detector();
        let result = det.detect(&samples);
        assert!(
            !result.is_voiced(),
            "{label} reported as {} Hz at confidence {}",
            result.frequency.get(),
            result.confidence.get()
        );
        assert_eq!(
            result.frequency,
            Hz::new(0.0),
            "{label} invented a frequency"
        );
    }
}

/// YIN's CMNDF is a ratio, so a 40 dB amplitude change must not move the
/// frequency.
#[test]
fn the_estimate_is_invariant_to_amplitude_scaling() {
    let freq = Hz::new(440.0);
    let quiet = {
        let mut det = detector();
        det.detect(&sine(freq, 0.01))
    };
    let loud = {
        let mut det = detector();
        det.detect(&sine(freq, 1.0))
    };

    assert!(quiet.is_voiced(), "0.01-amp 440 Hz was unvoiced");
    assert!(loud.is_voiced(), "1.0-amp 440 Hz was unvoiced");
    assert!(
        (quiet.frequency.get() - loud.frequency.get()).abs() < 0.05,
        "440 Hz read as {} at amp 0.01 but {} at amp 1.0",
        quiet.frequency.get(),
        loud.frequency.get()
    );
    assert!(
        (loud.frequency.get() - freq.get()).abs() < 0.1,
        "loud reading {} drifted from {}",
        loud.frequency.get(),
        freq.get()
    );
}

/// The FFT-derived difference function equals its definition
/// `d(tau) = sum_{j<W} (x[j] - x[j+tau])^2`, summed directly, and is zero
/// at a lag that is a whole number of periods.
#[test]
fn the_difference_function_matches_its_definition() {
    // 480 Hz at 48 kHz: exactly 100 samples per period.
    let x = sine(Hz::new(480.0), 0.8);
    let mut det = detector();
    let (_, max_period) = det.periods(x.len());
    det.compute_difference(&x, max_period);
    let window = max_period;
    for tau in [1usize, 17, 50, 100, 200, max_period] {
        let direct: f64 = (0..window)
            .map(|j| {
                let d = x[j] - x[j + tau];
                d * d
            })
            .sum();
        let got = det.difference[tau];
        assert!(
            (got - direct).abs() < 1e-9 * direct.max(1.0),
            "tau {tau}: {got} vs {direct}"
        );
    }
    assert!(det.difference[100] < 1e-9, "{}", det.difference[100]);
    assert!(det.difference[50] > 100.0, "{}", det.difference[50]);
}
