use super::*;
use tutti_types::SampleRate;

/// A hostile control reads as `Seconds::one_pole_coeff` defines its edges: a NaN, negative or
/// zero time is instant (coefficient 1) and an infinite one holds (coefficient 0), at every
/// rate, a sub-hertz one included.
#[test]
fn a_nan_or_negative_time_is_instant_and_an_infinite_one_holds() {
    for rate in [44_100.0, 48_000.0, 96_000.0, 0.5] {
        let rate = SampleRate::of(rate);
        for instant in [f64::NAN, -1.0, f64::NEG_INFINITY, -0.0, 0.0] {
            for hold in [f64::INFINITY, f64::MAX] {
                let b = floored(Seconds(instant), Seconds(hold));
                assert_eq!(b.coeffs(rate), (1.0, 0.0), "{instant} {hold} at {rate:?}");
            }
        }
    }
}
