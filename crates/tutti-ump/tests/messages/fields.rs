//! The bit-width fields (`U4`, `U7`, `U14`, `U28`, `NoteNumber`, `DeltaTicks`) and the
//! bounded ones keep the range-checked contract: `new` and `TryFrom` refuse
//! outside the range and agree, `of` panics there, `get` and `From` give the
//! raw back, `masked` keeps the low bits of a bit-width field, and serde (feature
//! `serde`) refuses what `new` refuses.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::{
    CCNumber, ChordType, DeltaTicks, FirstGroup, FunctionBlockCount, FunctionBlockId, GroupSpan,
    MidiChannel, MidiGroup, MonoChannels, MpeChannels, NoteNumber, SharpsFlats,
    TicksPerQuarterNote, U4, U4OutOfRange, U7, U14, U28,
};

/// Every bit-width field: valid up to its all-ones value, refused above it.
macro_rules! bit_field {
    ($ty:ident, $repr:ty, $width:expr) => {{
        let max: $repr = (1 << $width) - 1;
        check_range_contract(&RangeContract {
            new: $ty::new,
            get: $ty::get,
            of: Some($ty::of),
            valid: &[0, 1, max / 2, max],
            invalid: &[max + 1, <$repr>::MAX],
        });
        assert_eq!($ty::MAX.get(), max);
        for raw in [0, 1, max, max + 1, <$repr>::MAX] {
            assert_eq!(
                $ty::masked(raw).get(),
                raw & max,
                "{} masks {raw}",
                stringify!($ty)
            );
        }
    }};
}

#[test]
fn each_bit_width_field_holds_zero_to_all_bits_set() {
    bit_field!(U4, u8, 4);
    bit_field!(U7, u8, 7);
    bit_field!(U14, u16, 14);
    bit_field!(U28, u32, 28);
    bit_field!(NoteNumber, u8, 7);
    bit_field!(DeltaTicks, u32, 20);
    bit_field!(MidiChannel, u8, 4);
    bit_field!(MidiGroup, u8, 4);
    bit_field!(CCNumber, u8, 7);
}

#[test]
fn a_bit_width_error_carries_the_refused_raw() {
    assert_eq!(U4::try_from(16), Err(U4OutOfRange(16)));
}

/// Every bounded field over its whole raw range.
macro_rules! bounded {
    ($ty:ident, $repr:ty, $range:expr) => {{
        let range: core::ops::RangeInclusive<$repr> = $range;
        let (lo, hi) = (*range.start(), *range.end());
        let valid: Vec<$repr> = range.collect();
        let invalid: Vec<$repr> = (<$repr>::MIN..=<$repr>::MAX)
            .filter(|raw| !(lo..=hi).contains(raw))
            .collect();
        check_range_contract(&RangeContract {
            new: $ty::new,
            get: $ty::get,
            of: Some($ty::of),
            valid: &valid,
            invalid: &invalid,
        });
    }};
}

#[test]
fn each_bounded_field_holds_its_stated_range() {
    bounded!(FunctionBlockId, u8, 0..=31);
    bounded!(FirstGroup, u8, 0..=15);
    bounded!(GroupSpan, u8, 1..=16);
    bounded!(MpeChannels, u8, 2..=16);
    bounded!(ChordType, u8, 0..=0x1B);
    bounded!(FunctionBlockCount, u8, 0..=32);
    bounded!(MonoChannels, u8, 0..=16);
    bounded!(SharpsFlats, i8, -7..=7);
    check_range_contract(&RangeContract {
        new: TicksPerQuarterNote::new,
        get: TicksPerQuarterNote::get,
        of: Some(TicksPerQuarterNote::of),
        valid: &[1, 2, 480, u16::MAX],
        invalid: &[0],
    });
}

#[test]
fn of_is_usable_in_a_const() {
    const MIDDLE_C: NoteNumber = NoteNumber::of(60);
    const CENTRE: U14 = U14::of(8192);
    assert_eq!((MIDDLE_C.get(), CENTRE.get()), (60, 8192));
}

#[cfg(feature = "serde")]
#[test]
fn serde_refuses_what_new_refuses() {
    assert_eq!(serde_json::to_string(&U7::of(127)).unwrap(), "127");
    assert_eq!(serde_json::from_str::<U7>("127").unwrap(), U7::of(127));
    assert!(serde_json::from_str::<U7>("128").is_err());
    assert_eq!(serde_json::from_str::<U14>("16383").unwrap(), U14::MAX);
    assert!(serde_json::from_str::<U14>("16384").is_err());
    assert!(serde_json::from_str::<NoteNumber>("200").is_err());
    assert!(serde_json::from_str::<DeltaTicks>("1048576").is_err());
    assert!(serde_json::from_str::<U4>("16").is_err());
}
