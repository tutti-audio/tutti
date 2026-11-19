//! `PlaybackRate` keeps the range-checked contract: `new` refuses, `TryFrom` and
//! serde agree with it.
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_units::{PlaybackRate, PlaybackRateOutOfRange};

/// A rate is within `0.25..=4.0`, both ends included; NaN, the infinities, zero
/// and negatives are refused.
#[test]
fn a_rate_is_within_a_quarter_and_four() {
    let c = RangeContract {
        new: PlaybackRate::new,
        get: PlaybackRate::get,
        of: Some(PlaybackRate::of),
        valid: &[0.25, 0.5, 1.0, 2.0, 4.0],
        invalid: &[
            0.2499,
            4.0001,
            0.0,
            -0.0,
            -1.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(
        PlaybackRate::try_from(8.0),
        Err(PlaybackRateOutOfRange(8.0))
    );
    assert!(PlaybackRateOutOfRange(8.0).to_string().contains('8'));
}

/// `clamped` saturates at the bounds, and a NaN becomes unity: a NaN rate cannot
/// exist.
#[test]
fn clamped_saturates_and_maps_nan_to_unity() {
    assert_eq!(PlaybackRate::clamped(f64::NAN), PlaybackRate::UNITY);
    assert_eq!(PlaybackRate::clamped(10.0), PlaybackRate::MAX);
    assert_eq!(PlaybackRate::clamped(0.1), PlaybackRate::MIN);
    assert_eq!(PlaybackRate::clamped(f64::INFINITY), PlaybackRate::MAX);
    assert_eq!(PlaybackRate::clamped(f64::NEG_INFINITY), PlaybackRate::MIN);
    assert_eq!(PlaybackRate::clamped(2.0), PlaybackRate::of(2.0));
}

/// The bounds and the default are in range, and a cell's reload of any bits is
/// too.
#[test]
fn the_constants_and_a_cell_reload_stay_in_range() {
    use tutti_units::Unit;
    assert_eq!(PlaybackRate::MIN.get(), 0.25);
    assert_eq!(PlaybackRate::MAX.get(), 4.0);
    assert_eq!(PlaybackRate::default(), PlaybackRate::UNITY);
    for bits in [f64::NAN, 1e300, -3.0, 0.0] {
        let rate = PlaybackRate::from_raw(bits);
        assert!((0.25..=4.0).contains(&rate.get()));
    }
}
