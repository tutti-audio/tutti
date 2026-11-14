//! `I24` is three bytes and keeps the range-checked contract.

use tutti_buffers::{I24, I24OutOfRange};
use tutti_fixtures::{RangeContract, check_range_contract};

#[test]
fn i24_occupies_three_bytes_and_a_slice_of_them_is_packed() {
    assert_eq!(size_of::<I24>(), 3);
    assert_eq!(size_of::<[I24; 4]>(), 12);
}

#[test]
fn i24_keeps_the_range_checked_contract() {
    check_range_contract(&RangeContract {
        new: I24::new,
        get: I24::get,
        of: Some(I24::of),
        valid: &[
            -8_388_608, -8_388_607, -256, -1, 0, 1, 255, 65_536, 8_388_607,
        ],
        invalid: &[-8_388_609, 8_388_608, i32::MIN, i32::MAX],
    });
}

#[test]
fn i24_range_is_minus_two_to_the_23_through_two_to_the_23_minus_one() {
    assert_eq!(I24::MIN.get(), -(1 << 23));
    assert_eq!(I24::MAX.get(), (1 << 23) - 1);
    assert_eq!(I24::ZERO.get(), 0);
}

#[test]
fn i24_refusal_carries_the_refused_raw() {
    assert_eq!(I24::try_from(1 << 23), Err(I24OutOfRange(1 << 23)));
    assert!(I24OutOfRange(9_000_000).to_string().contains("9000000"));
}

#[test]
fn i24_bytes_are_little_endian_two_complement() {
    assert_eq!(I24::of(0x01_02_03).to_le_bytes(), [0x03, 0x02, 0x01]);
    assert_eq!(I24::of(-1).to_le_bytes(), [0xff, 0xff, 0xff]);
    assert_eq!(I24::MIN.to_le_bytes(), [0x00, 0x00, 0x80]);
    assert_eq!(I24::from_le_bytes([0x00, 0x00, 0x80]), I24::MIN);
}

#[test]
fn i24_orders_by_value_not_by_bytes() {
    assert!(I24::of(-1) < I24::of(0));
    assert!(I24::MIN < I24::of(-1));
    assert!(I24::of(256) > I24::of(255));
}

/// 24-bit AIFF is big-endian: the same three bytes, reversed.
#[test]
fn i24_big_endian_bytes_are_the_little_endian_bytes_reversed() {
    assert_eq!(I24::of(0x01_02_03).to_be_bytes(), [0x01, 0x02, 0x03]);
    assert_eq!(I24::of(-2).to_be_bytes(), [0xff, 0xff, 0xfe]);
    assert_eq!(I24::MIN.to_be_bytes(), [0x80, 0x00, 0x00]);
    assert_eq!(I24::MAX.to_be_bytes(), [0x7f, 0xff, 0xff]);
    assert_eq!(I24::from_be_bytes([0x80, 0x00, 0x00]), I24::MIN);
    for raw in [-8_388_608, -300, -1, 0, 1, 70_000, 8_388_607] {
        let v = I24::of(raw);
        assert_eq!(I24::from_be_bytes(v.to_be_bytes()), v, "{raw}");
        let mut le = v.to_le_bytes();
        le.reverse();
        assert_eq!(v.to_be_bytes(), le, "{raw}");
    }
}

#[test]
fn i24_displays_as_its_signed_value() {
    assert_eq!(I24::of(-5).to_string(), "-5");
    assert_eq!(I24::MIN.to_string(), "-8388608");
    assert_eq!(I24::MAX.to_string(), "8388607");
    assert_eq!(
        format!("{:>6}", I24::of(42)),
        "    42",
        "padding is honoured"
    );
    assert_eq!(format!("{:?}", I24::of(-5)), "I24(-5)");
}

/// The narrower signed integers widen without loss, sign included.
#[test]
fn i24_converts_from_the_narrower_integers_exactly() {
    for x in [i8::MIN, -1, 0, 1, i8::MAX] {
        assert_eq!(I24::from(x).get(), i32::from(x));
    }
    for x in [i16::MIN, -256, -1, 0, 1, 255, i16::MAX] {
        assert_eq!(I24::from(x).get(), i32::from(x));
    }
}
