use super::*;
use tutti_fixtures::{RangeContract, check_range_contract};

fn amplitudes(raws: &[f64]) -> Vec<Amplitude> {
    raws.iter().map(|&r| Amplitude::new(r)).collect()
}

fn seconds(raws: &[f64]) -> Vec<Seconds> {
    raws.iter().map(|&r| Seconds::new(r)).collect()
}

/// Above zero, finite.
#[test]
fn transient_threshold_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: TransientThreshold::new,
        get: TransientThreshold::get,
        of: Some(TransientThreshold::of),
        valid: &amplitudes(&[f64::MIN_POSITIVE, 0.05, 1.0, f64::MAX]),
        invalid: &amplitudes(&[0.0, -1.0, f64::NAN, f64::INFINITY]),
    });
}

/// `0.1 ms..=1 s`.
#[test]
fn transient_time_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: TransientTime::new,
        get: TransientTime::get,
        of: Some(TransientTime::of),
        valid: &seconds(&[1e-4, 0.001, 1.0]),
        invalid: &seconds(&[0.0, 9.9e-5, 1.001, -0.5, f64::NAN, f64::INFINITY]),
    });
}

/// A pair is built only with the fast time strictly below the slow one, and the error names both.
#[test]
fn a_pair_of_times_needs_the_fast_one_below_the_slow_one() {
    let t = |s: f64| TransientTime::of(Seconds::new(s));
    assert!(TransientTimes::new(t(0.001), t(0.03)).is_ok());
    for (fast, slow) in [(0.05, 0.01), (0.01, 0.01)] {
        let err = TransientTimes::new(t(fast), t(slow)).unwrap_err();
        assert_eq!(
            err,
            TransientTimesError {
                fast: Seconds::new(fast),
                slow: Seconds::new(slow)
            }
        );
    }
}
