//! `Normalized` keeps the range-checked contract: `new` refuses, `TryFrom` and
//! serde agree with it, and `clamped` pins.
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_units::{Normalized, NormalizedOutOfRange};

/// A normalized value is finite and on `0..=1`, both ends included.
#[test]
fn a_normalized_value_is_finite_and_on_zero_to_one() {
    let c = RangeContract {
        new: Normalized::new,
        get: Normalized::get,
        of: Some(Normalized::of),
        valid: &[0.0, f64::MIN_POSITIVE, 0.25, 0.5, 1.0],
        invalid: &[
            -f64::MIN_POSITIVE,
            -1.0,
            1.0 + f64::EPSILON,
            40.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(Normalized::try_from(2.0), Err(NormalizedOutOfRange(2.0)));
}

/// `clamped` holds the unit interval whatever it is handed: an automation
/// curve overshooting its endpoints, a wire payload from another process, a
/// division that produced NaN. NaN maps to zero rather than clamping, since
/// `f64::clamp` returns NaN for NaN.
#[test]
fn clamped_always_lands_on_the_unit_interval() {
    for (input, want) in [
        (0.5, 0.5),
        (0.0, 0.0),
        (1.0, 1.0),
        (1.5, 1.0),
        (-0.5, 0.0),
        (f64::INFINITY, 1.0),
        (f64::NEG_INFINITY, 0.0),
        (f64::NAN, 0.0),
    ] {
        let n = Normalized::clamped(input);
        assert_eq!(n.get(), want, "Normalized::clamped({input})");
    }
}

/// A plain value is not a normalized one: `new` refuses it and `clamped`
/// saturates it, so `set_parameter(id, 20_000.0)` against a `[10, 22050]` Hz
/// cutoff cannot be mistaken for a plain write.
#[test]
fn a_plain_value_cannot_masquerade_as_a_normalized_one() {
    assert_eq!(Normalized::clamped(20_000.0).get(), 1.0);
    assert_eq!(Normalized::new(20_000.0), None);
}
