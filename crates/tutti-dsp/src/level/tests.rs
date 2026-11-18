use super::*;
use core::f64::consts::TAU;

/// A whole number of periods of a full-scale sine has mean square 1/2, so
/// RMS `1/sqrt(2)`.
#[test]
fn a_full_scale_sine_reads_one_over_root_two() {
    let n = 4800;
    let sine: Vec<f64> = (0..n)
        .map(|i| libm::sin(TAU * 10.0 * i as f64 / n as f64))
        .collect();
    let got = rms(Mono::new(&sine)).get();
    assert!(
        (f64::from(got) - core::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6,
        "{got}"
    );
}

/// A constant block reads its magnitude, whatever its sign.
#[test]
fn a_constant_reads_its_magnitude() {
    assert_eq!(rms(Mono::new(&[0.5f32; 100])), Amplitude(0.5));
    assert_eq!(rms(Mono::new(&[-0.25f32; 7])), Amplitude(0.25));
}

/// There is nothing to average, and no level.
#[test]
fn an_empty_block_is_silent() {
    assert_eq!(rms(Mono::new(&[] as &[f32])), Amplitude::SILENT);
}
