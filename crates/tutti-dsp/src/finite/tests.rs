use super::*;

#[test]
fn finite_or_passes_a_finite_value_and_defaults_nan_and_infinities() {
    assert_eq!(finite_or(2.5_f32, 7.0), 2.5);
    assert_eq!(finite_or(f32::NAN, 7.0), 7.0);
    assert_eq!(finite_or(f32::INFINITY, 7.0), 7.0);
    assert_eq!(finite_or(f32::NEG_INFINITY, 7.0), 7.0);
    assert_eq!(finite_or(-1e300_f64, 7.0), -1e300);
    assert_eq!(finite_or(f64::NAN, 7.0), 7.0);
}
