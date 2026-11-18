use tutti_units::Amplitude;

use super::fold_peak;

/// The peak fold ignores a NaN (`max` returns the other operand), so a NaN sample never
/// reaches the detector, and takes the magnitude of a negative one.
#[test]
fn a_nan_fed_to_the_peak_fold_is_ignored() {
    assert_eq!(fold_peak(Amplitude(0.5), f64::NAN), Amplitude(0.5));
    assert_eq!(fold_peak(Amplitude::SILENT, f64::NAN), Amplitude::SILENT);
    assert_eq!(fold_peak(Amplitude(0.5), -0.75), Amplitude(0.75));
}
