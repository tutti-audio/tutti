use super::fold_peak;

/// The peak fold ignores a NaN (`max` returns the other operand), so a NaN sample never
/// reaches the detector, and takes the magnitude of a negative one.
#[test]
fn a_nan_fed_to_the_peak_fold_is_ignored() {
    assert_eq!(fold_peak(0.5, f32::NAN), 0.5);
    assert_eq!(fold_peak(0.0, f32::NAN), 0.0);
    assert_eq!(fold_peak(0.5, -0.75), 0.75);
}
