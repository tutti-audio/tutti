use std::time::Duration;

use tutti_ump::Ump;

use super::{Stamps, pack, unpack};

/// MIDI 1.0 Channel Voice in type 2 (M2-104 section 7.3): `0x90 0x3C 0x64` is
/// word `0x2090_3C64`, and winmm packs it low byte first.
#[test]
fn a_note_on_packs_to_status_low_and_unpacks_to_its_type_2_word() {
    let ump = Ump([0x2090_3C64, 0, 0, 0]);
    assert_eq!(pack(&ump), Some(0x0064_3C90));
    assert_eq!(unpack(0x0064_3C90), Some(ump));
}

/// A Program Change has one data byte: the UMP's last byte is zero and so is
/// the packed `DWORD`'s, and a nonzero padding byte in the `DWORD` is ignored.
#[test]
fn a_program_change_has_one_data_byte() {
    let ump = Ump([0x20C3_0A00, 0, 0, 0]);
    assert_eq!(pack(&ump), Some(0x0000_0AC3));
    assert_eq!(unpack(0x7F00_0AC3), Some(ump));
}

/// System Real Time (M2-104 section 7.6) is type 1 with the status byte alone.
#[test]
fn a_timing_clock_is_a_one_byte_system_message() {
    let ump = Ump([0x10F8_0000, 0, 0, 0]);
    assert_eq!(pack(&ump), Some(0xF8));
    assert_eq!(unpack(0xF8), Some(ump));
}

#[test]
fn a_data_byte_with_no_status_is_not_a_message() {
    assert_eq!(unpack(0x0000_0040), None);
}

/// MIDI 2.0 Channel Voice (type 4, M2-104 section 7.4) has no MIDI 1.0 byte
/// form, nor do Utility (type 0), Data 64 (type 3) or Flex Data (type D).
#[test]
fn a_message_type_with_no_midi1_byte_form_is_not_packed() {
    for first in [0x4090_3C00, 0x0000_0000, 0x3004_7E7F, 0xD010_0000] {
        assert_eq!(pack(&Ump([first, 0, 0, 0])), None, "{first:#010x}");
    }
}

/// Stamps `u32::MAX - 5` ms and then `3` ms are 9 ms apart: the second is
/// after the wrap.
#[test]
fn a_stamp_after_the_wrap_is_later_by_the_wrapped_difference() {
    let mut stamps = Stamps::default();
    let before = stamps.since_start(u32::MAX - 5);
    let after = stamps.since_start(3);
    assert_eq!(before, Duration::from_millis(u64::from(u32::MAX) - 5));
    assert_eq!(after - before, Duration::from_millis(9));
}

/// Three stamps across two wraps: each step is the difference modulo 2^32.
#[test]
fn stamps_across_two_wraps_accumulate() {
    let mut stamps = Stamps::default();
    let step = 0x7FFF_FFFF_u32;
    let times: Vec<u64> = [step, step.wrapping_mul(2), step.wrapping_mul(3)]
        .map(|m| stamps.since_start(m).as_millis() as u64)
        .into();
    assert_eq!(
        times,
        [u64::from(step), 2 * u64::from(step), 3 * u64::from(step)]
    );
    // The third stamp is past 2^32 ms in total: the wraps are counted.
    assert!(times[2] > u64::from(u32::MAX));
}

/// A stamp 3 ms before the last holds the time, and the next in-order stamp
/// continues from it.
#[test]
fn an_out_of_order_stamp_holds_the_time() {
    let mut stamps = Stamps::default();
    assert_eq!(stamps.since_start(100), Duration::from_millis(100));
    assert_eq!(stamps.since_start(97), Duration::from_millis(100));
    assert_eq!(stamps.since_start(105), Duration::from_millis(105));
}

/// 0xF4 is undefined (M2-104 section 7.6 lists none); 0xF8 is not a channel
/// voice status and 0x90 is not a system one, so each word is refused.
#[test]
fn a_status_that_does_not_belong_to_its_type_is_not_packed() {
    for first in [0x10F4_0000, 0x20F8_0000, 0x1090_3C64, 0x2070_0000] {
        assert_eq!(pack(&Ump([first, 0, 0, 0])), None, "{first:#010x}");
    }
}

/// The packed word holds a message's own bytes only: the unused bytes are zero.
#[test]
fn bytes_a_message_does_not_use_pack_as_zero() {
    assert_eq!(pack(&Ump([0x20C3_0A7F, 0, 0, 0])), Some(0x0000_0AC3));
    assert_eq!(pack(&Ump([0x10F8_FFFF, 0, 0, 0])), Some(0xF8));
}
