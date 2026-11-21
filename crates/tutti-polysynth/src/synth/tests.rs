use super::*;
use tutti_fixtures::{RangeContract, check_range_contract};

/// **A starting master volume is a finite gain of at least zero**: silence is valid, a
/// negative gain, NaN and the infinities are refused rather than read as 0.
#[test]
fn master_volume_takes_every_finite_gain_from_zero() {
    check_range_contract(&RangeContract {
        new: MasterVolume::new,
        get: MasterVolume::get,
        of: Some(MasterVolume::of),
        valid: &[
            Amplitude::SILENT,
            Amplitude::new(0.5),
            Amplitude::UNITY,
            Amplitude::new(4.0),
        ],
        invalid: &[
            Amplitude::new(-0.001),
            Amplitude::new(f64::NAN),
            Amplitude::new(f64::INFINITY),
            Amplitude::new(f64::NEG_INFINITY),
        ],
    });
}

/// **The SVF's Q runs from 0.01 to 1000**, the range its coefficients are solved and stable
/// in: both ends are valid, and a Q at or below zero, past the ceiling, or NaN is refused.
#[test]
fn svf_q_takes_exactly_the_stable_range() {
    check_range_contract(&RangeContract {
        new: SvfQ::new,
        get: SvfQ::get,
        of: Some(SvfQ::of),
        valid: &[
            Q::new(SvfControls::MIN_Q),
            Q::BUTTERWORTH,
            Q::new(SvfControls::MAX_Q),
        ],
        invalid: &[
            Q::new(0.0),
            Q::new(-1.0),
            Q::new(0.009_999),
            Q::new(1_000.001),
            Q::new(f64::NAN),
            Q::new(f64::INFINITY),
        ],
    });
}
