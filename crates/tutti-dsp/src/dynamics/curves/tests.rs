use super::*;

#[test]
fn test_amplitude_db_conversion() {
    assert_eq!(amplitude_to_db(Amplitude::UNITY), Db::UNITY);
    assert!((amplitude_to_db(Amplitude::new(0.5)).get() + 6.020_599_913_279_624).abs() < 1e-12);
    assert_eq!(Db::UNITY.to_amplitude(), Amplitude::UNITY);
    assert!((Db::new(-6.0).to_amplitude().get() - 0.501_187_233_627_272_3).abs() < 1e-15);
}

/// The floor lives here, not in `Db::from_amplitude`. The shared converter
/// hands back `-inf` for silence; this is the boundary that pins it, and
/// every dynamics detector inherits that choice by going through here.
#[test]
fn the_dynamics_floor_is_applied_at_this_boundary() {
    // The conversion this delegates to does NOT floor — proving the guard
    // above is doing the work rather than duplicating something upstream.
    assert!(Db::from_amplitude(Amplitude::SILENT).get().is_infinite());

    assert_eq!(amplitude_to_db(Amplitude::new(0.0)), Db::FLOOR);
    assert!(amplitude_to_db(Amplitude::new(0.0)).get().is_finite());
    // Negative amplitude is not physical, but a detector fed one must not
    // produce NaN either.
    assert_eq!(amplitude_to_db(Amplitude::new(-1.0)), Db::FLOOR);
}

/// The floor's payoff: a detected *level* stays finite and comparable.
///
/// Assert on the level, never on a downstream gain. The gain curves send
/// `-inf` down their below-threshold branch and return a finite reduction,
/// so `is_finite()` on the gain holds with the guard removed and would
/// cover nothing.
#[test]
fn the_floor_keeps_a_detected_level_usable() {
    let level = amplitude_to_db(Amplitude::new(0.0));

    // Arithmetic a detector does on its own level. `-inf - -inf` is NaN,
    // which is what an unfloored level turns a difference into.
    assert!((level - Db::FLOOR).get().is_finite());
    assert!(level.get().is_finite());

    // And it stays ordered against a real threshold, so envelope
    // comparisons behave.
    assert!(level < Db(-60.0));
    assert_eq!(level.clamp(Db(-90.0), Db::UNITY), Db(-90.0));
}

#[test]
fn test_compressor_gain_reduction_hard_knee() {
    // Below threshold: no reduction
    assert_eq!(
        compute_compressor_gain_reduction(
            Db::new(-30.0),
            Db::new(-20.0),
            CompressionRatio::new(4.0),
            Db::new(0.0)
        )
        .get(),
        0.0
    );
    // Above threshold: ratio compression
    let gr = compute_compressor_gain_reduction(
        Db::new(-10.0),
        Db::new(-20.0),
        CompressionRatio::new(4.0),
        Db::new(0.0),
    )
    .get();
    assert_eq!(gr, 7.5); // 10dB over * (1 - 1/4) = 7.5
}

#[test]
fn test_compressor_gain_reduction_soft_knee() {
    // Well below: no reduction
    assert_eq!(
        compute_compressor_gain_reduction(
            Db::new(-40.0),
            Db::new(-20.0),
            CompressionRatio::new(4.0),
            Db::new(6.0)
        )
        .get(),
        0.0
    );
    // Well above: same as hard knee
    let gr_hard = compute_compressor_gain_reduction(
        Db::new(0.0),
        Db::new(-20.0),
        CompressionRatio::new(4.0),
        Db::new(0.0),
    )
    .get();
    let gr_soft = compute_compressor_gain_reduction(
        Db::new(0.0),
        Db::new(-20.0),
        CompressionRatio::new(4.0),
        Db::new(6.0),
    )
    .get();
    assert_eq!(gr_hard, gr_soft);
    // In knee region: quadratic transition (non-zero, less than hard)
    let gr_knee = compute_compressor_gain_reduction(
        Db::new(-20.0),
        Db::new(-20.0),
        CompressionRatio::new(4.0),
        Db::new(6.0),
    )
    .get();
    assert!(gr_knee > 0.0);
    assert!(gr_knee < gr_hard);
}

#[test]
fn test_compute_limiter_gain() {
    // Below threshold: gain = ceiling - threshold offset
    let gain = compute_limiter_gain(Db::new(-20.0), Db::new(-6.0), Db::new(-0.3)).get();
    assert_eq!(gain, Db(-0.3 - (-6.0)).to_amplitude().get());
    // Above threshold: reduces gain
    let gain = compute_limiter_gain(Db::new(0.0), Db::new(-6.0), Db::new(-0.3)).get();
    assert!(gain < 1.0);
}

/// At (-6, -0.3) dB the output is monotonic: `min(in + 5.7, -0.3)`, with no drop at
/// the knee (-60 -> -54.3, -7 -> -1.3, -6.1 -> -0.4, 0 -> -0.3), on both the dB form
/// and the block's linear form.
#[test]
fn the_curve_is_the_input_plus_the_drive_up_to_the_ceiling() {
    let (threshold, ceiling) = (Db(-6.0), Db(-0.3));
    let curve = LimiterCurve::new(threshold, ceiling);
    for (input, want) in [
        (-60.0f64, -54.3f64),
        (-7.0, -1.3),
        (-6.1, -0.4),
        (-5.9, -0.3),
        (0.0, -0.3),
        (12.0, -0.3),
    ] {
        let peak = Db(input).to_amplitude();
        let block = amplitude_to_db(peak * curve.gain(peak).get()).get();
        let fed = amplitude_to_db(
            peak * compute_limiter_gain(amplitude_to_db(peak), threshold, ceiling).get(),
        )
        .get();
        assert!((block - want).abs() < 1e-12, "block: {input} -> {block}");
        assert!((fed - want).abs() < 1e-12, "fed: {input} -> {fed}");
    }
}

/// The dB form (the fed path) and the linear form (the block path) are
/// one curve: the same gain for every peak, threshold and ceiling,
/// boost or cut.
#[test]
fn the_block_and_fed_forms_are_one_curve() {
    for (t, c) in [
        (-6.0f64, -0.3f64),
        (-12.0, -1.0),
        (-3.0, -9.0),
        (-6.0, -6.0),
    ] {
        let curve = LimiterCurve::new(Db(t), Db(c));
        let mut peak_db = -80.0f64;
        while peak_db <= 12.0 {
            let peak = Db(peak_db).to_amplitude();
            let block = curve.gain(peak).get();
            let fed = compute_limiter_gain(amplitude_to_db(peak), Db(t), Db(c)).get();
            assert!(
                (block - fed).abs() <= 1e-12 * fed,
                "({t}, {c}) at {peak_db}: {block} against {fed}"
            );
            peak_db += 0.5;
        }
    }
}

/// A NaN peak takes `below`, not `over / NaN`.
#[test]
fn a_nan_peak_takes_the_below_gain() {
    let curve = LimiterCurve::new(Db(-6.0), Db(-3.0));
    assert_eq!(curve.gain(Amplitude(f64::NAN)), curve.below);
    assert!(curve.below > Amplitude(1.0));
}

/// A threshold past the clamp (under -6165 dB, or `-inf`) and a peak down to the smallest
/// subnormal never make a gain that is not finite, and the output `peak * gain` never passes
/// the ceiling: the boost under the threshold is held to the ceiling over the clamped threshold.
#[test]
fn a_threshold_past_the_clamp_never_overshoots_the_ceiling() {
    for threshold in [f64::NEG_INFINITY, -7_000.0, -6_165.0, -2_000.0] {
        for ceiling in [-6.0, 0.0, 12.0, 20.0] {
            let curve = LimiterCurve::new(Db(threshold), Db(ceiling));
            let bound = Db(ceiling).to_amplitude().get() * (1.0 + 1e-12);
            for peak in [0.0, 5e-324, 1e-320, 1e-308, 1e-300, 1e-100, 1e-3, 1.0, 1e30] {
                let gain = curve.gain(Amplitude(peak)).get();
                assert!(
                    gain.is_finite(),
                    "({threshold}, {ceiling}) at {peak}: {gain}"
                );
                assert!(
                    peak * gain <= bound,
                    "({threshold}, {ceiling}) at {peak}: {}",
                    peak * gain
                );
            }
        }
    }
}
