//! `FrequencyRatio` and `ModulationIndex` keep the range-checked contract: `new` refuses,
//! `TryFrom` and serde agree with it.
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_units::{
    FrequencyRatio, FrequencyRatioOutOfRange, ModulationIndex, ModulationIndexOutOfRange,
};

/// A frequency ratio is finite and above zero: zero is refused, an inharmonic value is not.
#[test]
fn a_frequency_ratio_is_finite_and_above_zero() {
    let c = RangeContract {
        new: FrequencyRatio::new,
        get: FrequencyRatio::get,
        of: Some(FrequencyRatio::of),
        valid: &[f64::MIN_POSITIVE, 0.5, 1.0, 1.5, 2.0, f64::MAX],
        invalid: &[0.0, -0.0, -1.0, f64::NEG_INFINITY, f64::INFINITY, f64::NAN],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(
        FrequencyRatio::try_from(0.0),
        Err(FrequencyRatioOutOfRange(0.0))
    );
    assert!(FrequencyRatioOutOfRange(-5.0).to_string().contains("-5"));
}

/// A modulation index is finite and at least zero: zero is an index, a negative is not.
#[test]
fn a_modulation_index_is_finite_and_at_least_zero() {
    let c = RangeContract {
        new: ModulationIndex::new,
        get: ModulationIndex::get,
        of: Some(ModulationIndex::of),
        valid: &[0.0, f64::MIN_POSITIVE, 0.5, 8.0, f64::MAX],
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
    assert_eq!(
        ModulationIndex::try_from(-1.0),
        Err(ModulationIndexOutOfRange(-1.0))
    );
    assert!(ModulationIndexOutOfRange(-5.0).to_string().contains("-5"));
}
