//! `Metres` and `Speed` keep the range-checked contract: `new` refuses, `TryFrom`
//! and serde agree with it.
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_units::{Metres, MetresOutOfRange, Speed, SpeedOutOfRange};

/// A distance is finite and at least zero: zero is a distance, a negative or
/// non-finite raw is not.
#[test]
fn a_distance_is_finite_and_at_least_zero() {
    let c = RangeContract {
        new: Metres::new,
        get: Metres::get,
        of: Some(Metres::of),
        valid: &[0.0, f64::MIN_POSITIVE, 0.5, 343.0, f64::MAX],
        invalid: &[
            -1.0,
            -f64::MIN_POSITIVE,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(Metres::try_from(-1.0), Err(MetresOutOfRange(-1.0)));
    assert!(MetresOutOfRange(-5.0).to_string().contains("-5"));
}

/// A speed is finite and above zero: zero is refused.
#[test]
fn a_speed_is_finite_and_above_zero() {
    let c = RangeContract {
        new: Speed::new,
        get: Speed::get,
        of: Some(Speed::of),
        valid: &[f64::MIN_POSITIVE, 1.0, 343.0, 1_500.0, f64::MAX],
        invalid: &[
            0.0,
            -0.0,
            -343.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(Speed::try_from(0.0), Err(SpeedOutOfRange(0.0)));
    assert!(SpeedOutOfRange(-5.0).to_string().contains("-5"));
}

/// The speed of sound in air at 20 degrees Celsius is 343 m/s.
#[test]
fn sound_in_air_is_343_metres_per_second() {
    assert_eq!(Speed::SOUND_IN_AIR_20C.get(), 343.0);
}
