//! The bit-width fields (`U4`, `U7`, `U14`, `U28`, `NoteNumber`, `DeltaTicks`) and the
//! bounded ones keep the range-checked contract: `new` and `TryFrom` refuse
//! outside the range and agree, `of` panics there, `get` and `From` give the
//! raw back, `masked` keeps the low bits of a bit-width field, and serde (feature
//! `serde`) refuses what `new` refuses.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::{
    CCNumber, ChordType, CiVersion, DeltaTicks, FirstGroup, FunctionBlockCount, FunctionBlockId,
    GroupChannelIndex, GroupSpan, ManufacturerId, MdsChunkSize, MdsChunkUmps, MidiChannel,
    MidiGroup, MonoChannels, MpeChannels, NoteNumber, SharpsFlats, TicksPerQuarterNote, U4,
    U4OutOfRange, U7, U14, U28,
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
        new: CiVersion::new,
        get: CiVersion::get,
        of: Some(CiVersion::of),
        valid: &[0x01, 0x02, 0x1F],
        invalid: &[0x00, 0x20, 0x80, 0xFF],
    });
    check_range_contract(&RangeContract {
        new: TicksPerQuarterNote::new,
        get: TicksPerQuarterNote::get,
        of: Some(TicksPerQuarterNote::of),
        valid: &[1, 2, 480, u16::MAX],
        invalid: &[0],
    });
    // M2-104 section 7.9: a chunk is a 16-byte header and whole 16-byte
    // payload UMPs, the last padded, and a size one byte into a payload UMP's
    // prefix has no data. The 4094 payload UMPs the 16 bits reach.
    check_range_contract(&RangeContract {
        new: MdsChunkSize::new,
        get: MdsChunkSize::get,
        of: Some(MdsChunkSize::of),
        valid: &[16, 18, 32, 34, 48, u16::MAX],
        invalid: &[0, 15, 17, 33, 16 + 16 * 5 + 1, 16 + 16 * 4094 + 1],
    });
    check_range_contract(&RangeContract {
        new: MdsChunkUmps::new,
        get: MdsChunkUmps::get,
        of: Some(MdsChunkUmps::of),
        valid: &[1, 2, 2047, 4093, 4094],
        invalid: &[0, 4095, 4096, u16::MAX],
    });
    check_range_contract(&RangeContract {
        new: GroupChannelIndex::new,
        get: GroupChannelIndex::get,
        of: Some(GroupChannelIndex::of),
        valid: &[0, 1, 255, 256, 511],
        invalid: &[512, 513, 1023, u16::MAX],
    });
    // M2-104 section 7.10: `0x0001..=0x007F`, or the top bit set over two
    // 7-bit bytes; neither zero id.
    check_range_contract(&RangeContract {
        new: ManufacturerId::new,
        get: ManufacturerId::get,
        of: Some(ManufacturerId::of),
        valid: &[0x0001, 0x007F, 0x8001, 0x8100, 0xA109, 0xFF7F],
        invalid: &[
            0x0000, 0x0080, 0x0100, 0x7F00, 0x7FFF, 0x8000, 0x8080, 0x80FF, 0xFFFF,
        ],
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
    assert!(serde_json::from_str::<MdsChunkSize>("33").is_err());
    assert_eq!(
        serde_json::from_str::<MdsChunkSize>("34").unwrap().get(),
        34
    );
    assert!(serde_json::from_str::<ManufacturerId>("128").is_err());
    assert_eq!(
        serde_json::from_str::<ManufacturerId>("41225")
            .unwrap()
            .get(),
        0xA109
    );
}

/// A count of blocks past what an Endpoint holds is the most it holds, never
/// wrapped to a smaller count.
#[test]
fn a_block_count_saturates_at_the_most_an_endpoint_holds() {
    assert_eq!(FunctionBlockCount::saturating(0).get(), 0);
    assert_eq!(FunctionBlockCount::saturating(32).get(), 32);
    assert_eq!(FunctionBlockCount::saturating(33), FunctionBlockCount::MAX);
    assert_eq!(FunctionBlockCount::saturating(256), FunctionBlockCount::MAX);
    assert_eq!(
        FunctionBlockCount::saturating(usize::MAX),
        FunctionBlockCount::MAX
    );
}
