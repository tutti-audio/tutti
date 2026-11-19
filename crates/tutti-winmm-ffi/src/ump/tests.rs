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
