use super::*;

/// Pair two equal-length planes, panicking if they disagree.
///
/// Test-only: a test that hands over mismatched planes has a bug in the
/// test, and saying so loudly beats threading an `Option` through every
/// assertion. Production callers use `StereoPlanes::new` and handle `None`.
fn pair<'a>(l: &'a [f32], r: &'a [f32]) -> StereoPlanes<'a> {
    StereoPlanes::new(l, r).expect("test planes must be the same length")
}

fn smoother(attack: Seconds, release: Seconds) -> StereoSmoother {
    StereoSmoother::new(Ballistics::new(attack, release).expect("valid times"))
}

fn tone(n: usize, scale: f32) -> Vec<f32> {
    (0..n).map(|i| (i as f32 / 20.0).sin() * scale).collect()
}

#[test]
fn identical_channels_are_mono() {
    let signal = tone(1000, 1.0);
    let reading = correlate(pair(&signal, &signal));

    assert!((reading.correlation.get() - 1.0).abs() < 1e-12);
    assert!(reading.is_mono());
    assert!(!reading.has_phase_issues());
    assert_eq!(reading.width(), StereoWidth::MONO);
    assert_eq!(reading.levels.side.get(), 0.0, "no side content");
}

#[test]
fn inverted_channels_cancel() {
    let signal = tone(1000, 1.0);
    let inverted: Vec<f32> = signal.iter().map(|s| -s).collect();
    let reading = correlate(pair(&signal, &inverted));

    assert!((reading.correlation.get() + 1.0).abs() < 1e-12);
    assert!(reading.has_phase_issues());
    assert_eq!(reading.levels.mid.get(), 0.0, "mid cancels to silence");
}

#[test]
fn balance_follows_the_louder_channel() {
    let signal = tone(1000, 1.0);
    let silence = vec![0.0f32; 1000];

    assert!(correlate(pair(&signal, &silence)).balance < StereoBalance::new(-0.9));
    assert!(correlate(pair(&silence, &signal)).balance > StereoBalance::new(0.9));
    assert_eq!(
        correlate(pair(&silence, &silence)).balance,
        StereoBalance::CENTER
    );
}

/// The standard ballistics are a 10 ms attack and a 100 ms release: over a
/// step of one time constant a one-pole moves `1 - 1/e` of the way.
#[test]
fn standard_ballistics_are_ten_milliseconds_up_and_a_hundred_down() {
    let cfg = StereoSmoother::standard().cfg;
    let want = 1.0 - (-1.0f64).exp();
    assert!((cfg.coeff_over(true, Seconds::new(0.01)) - want).abs() < 1e-6);
    assert!((cfg.coeff_over(false, Seconds::new(0.1)) - want).abs() < 1e-6);
}

/// Width is derived, so it cannot
/// drift from the correlation it is defined from — even after smoothing.
#[test]
fn width_cannot_contradict_correlation_even_when_smoothed() {
    let mut state = StereoSmoother::standard();
    let mono = tone(512, 1.0);
    let inverted: Vec<f32> = mono.iter().map(|s| -s).collect();

    // Flip from correlated to anti-correlated: the case that desynced the
    // old stored pair.
    let smoothed = state.push(correlate(pair(&mono, &mono)), Seconds::new(0.01));
    assert_eq!(smoothed.width(), smoothed.correlation.to_stereo_width());

    let smoothed = state.push(correlate(pair(&mono, &inverted)), Seconds::new(0.01));
    assert_eq!(smoothed.width(), smoothed.correlation.to_stereo_width());
}

/// The smoothed value is what comes back, not the instantaneous one.
#[test]
fn the_return_value_is_the_smoothed_reading() {
    let mut state = smoother(Seconds::new(1.0), Seconds::new(1.0));
    let mono = tone(512, 1.0);

    let instant = correlate(pair(&mono, &mono));
    let smoothed = state.push(instant, Seconds::new(0.001));

    assert_eq!(smoothed, state.current());
    assert!(
        smoothed.correlation < instant.correlation,
        "a 1 s attack should lag a step input"
    );
}

/// Ballistics that actually differ, unlike the dead `set_smoothing` field.
#[test]
fn attack_and_release_have_distinct_effects() {
    let mono = tone(512, 1.0);
    let inverted: Vec<f32> = mono.iter().map(|s| -s).collect();

    let mut fast = smoother(Seconds::new(0.001), Seconds::new(0.001));
    let mut slow = smoother(Seconds::new(1.0), Seconds::new(1.0));

    let target = correlate(pair(&mono, &mono));
    let fast_out = fast.push(target, Seconds::new(0.01));
    let slow_out = slow.push(target, Seconds::new(0.01));

    assert!(
        fast_out.correlation > slow_out.correlation,
        "fast ballistics must approach the target sooner"
    );

    // And the release path differs from the attack path.
    let falling = correlate(pair(&mono, &inverted));
    let mut state = smoother(Seconds::new(0.001), Seconds::new(1.0));
    state.push(target, Seconds::new(0.01));
    let before = state.current().correlation;
    state.push(falling, Seconds::new(0.01));
    let after = state.current().correlation;
    assert!(
        (before.get() - after.get()).abs() < 0.05,
        "a 1 s release should barely move in 10 ms"
    );
}

/// **A zero attack or release is instant**: one step of any length
/// reaches the new value, rising and falling.
#[test]
fn a_zero_time_is_instant() {
    let mut smoother = smoother(Seconds::new(0.0), Seconds::new(0.0));
    let reading = |correlation: f64| StereoReading {
        correlation: Correlation::new_clamped(correlation),
        ..StereoReading::default()
    };
    let dt = Seconds::new(0.001);
    assert_eq!(smoother.push(reading(0.8), dt).correlation.get(), 0.8);
    assert_eq!(smoother.push(reading(0.25), dt).correlation.get(), 0.25);
}

#[test]
fn ms_ratio_is_finite_at_both_extremes() {
    let mono = tone(1000, 1.0);
    let silence = vec![0.0f32; 1000];

    // All mid, no side.
    let all_mid = correlate(pair(&mono, &mono)).levels.ms_ratio();
    assert!(all_mid.get().is_finite() && all_mid.get() > 0.0);

    // All side, no mid.
    let inverted: Vec<f32> = mono.iter().map(|s| -s).collect();
    let all_side = correlate(pair(&mono, &inverted)).levels.ms_ratio();
    assert!(all_side.get().is_finite() && all_side.get() < 0.0);

    // Silence is neither.
    assert_eq!(
        correlate(pair(&silence, &silence)).levels.ms_ratio(),
        Db::new(0.0)
    );
}

#[test]
fn an_empty_block_reads_as_the_default() {
    assert_eq!(correlate(pair(&[], &[])), StereoReading::default());
}

/// Ragged planes are **unrepresentable** rather than silently truncated.
///
/// Reconciling a mismatch with `left.len().min(right.len())` makes a
/// three-frame left against a one-frame right report a correlation of 1.0
/// from a single frame, with the caller never learning that two thirds of
/// its left channel went unmeasured. `StereoPlanes` refuses the pairing
/// instead, which is what this test pins.
#[test]
fn ragged_planes_cannot_be_paired() {
    assert!(
        StereoPlanes::new(&[1.0, 1.0, 1.0], &[1.0]).is_err(),
        "a mismatched pair must be rejected, not silently truncated to the shorter"
    );
}

#[test]
fn independent_noise_is_near_zero() {
    use rand::{RngExt, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE);
    let left: Vec<f32> = (0..20000).map(|_| rng.random_range(-1.0..1.0)).collect();
    let right: Vec<f32> = (0..20000).map(|_| rng.random_range(-1.0..1.0)).collect();

    let reading = correlate(pair(&left, &right));
    assert!(reading.correlation.get().abs() < 0.05);
    assert!((reading.width().get() - 1.0).abs() < 0.05);
}
