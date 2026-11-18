//! A `ranged!` type declared `bits $width` keeps the range-checked contract
//! over `0..=(1 << width) - 1`, and adds `MAX` and `masked`.

use tutti_fixtures::{RangeContract, check_range_contract};

tutti_units::ranged! {
    /// A 7-bit field.
    Seven(u8), SevenOutOfRange, bits 7
}

tutti_units::ranged! {
    /// A 28-bit field.
    TwentyEight(u32), TwentyEightOutOfRange, bits 28
}

/// The range is the width's bits all clear to all set.
#[test]
fn a_bits_type_holds_zero_to_all_bits_set() {
    check_range_contract(&RangeContract {
        new: Seven::new,
        get: Seven::get,
        of: Some(Seven::of),
        valid: &[0, 1, 64, 127],
        invalid: &[128, 200, u8::MAX],
    });
    check_range_contract(&RangeContract {
        new: TwentyEight::new,
        get: TwentyEight::get,
        of: Some(TwentyEight::of),
        valid: &[0, 1, 0x0800_0000, 0x0FFF_FFFF],
        invalid: &[0x1000_0000, u32::MAX],
    });
    assert_eq!(Seven::MAX.get(), 127);
    assert_eq!(TwentyEight::MAX.get(), 0x0FFF_FFFF);
}

/// `masked` keeps the low bits and agrees with `new` inside the range.
#[test]
fn masked_keeps_the_low_bits() {
    for raw in 0..=u8::MAX {
        assert_eq!(Seven::masked(raw).get(), raw & 0x7F);
        assert_eq!(Seven::new(raw), (raw < 128).then(|| Seven::masked(raw)));
    }
    assert_eq!(TwentyEight::masked(u32::MAX), TwentyEight::MAX);
}

/// The error carries the refused raw and is `Eq`.
#[test]
fn the_error_carries_the_raw_and_is_eq() {
    fn is_eq<T: Eq>(_: &T) {}
    let err = Seven::try_from(130).unwrap_err();
    is_eq(&err);
    assert_eq!(err, SevenOutOfRange(130));
    assert!(err.to_string().contains("130"));
}
