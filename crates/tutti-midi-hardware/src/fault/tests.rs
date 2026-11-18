use super::*;
#[cfg(feature = "serde")]
use tutti_fixtures::check_range_serde;
use tutti_fixtures::{RangeContract, check_range_contract};

fn errno(raw: i32) -> Option<Errno> {
    Errno::clamped(raw)
}

fn every_variant(raw: i32) -> [InputFault; 7] {
    [
        InputFault::Panicked,
        InputFault::WaitFailed(errno(raw)),
        InputFault::ReadFailed(errno(raw)),
        InputFault::WaitFailed(None),
        InputFault::PortBroken,
        InputFault::DriverClosed,
        InputFault::BufferLost,
    ]
}

/// **The `Cause` law holds for every variant at the payload edges**: no
/// number, `1`, a usual one, the largest 24-bit number, one past it, and the
/// largest `i32`.
#[test]
fn every_fault_reads_back_from_its_code() {
    for raw in [0, 1, 5, 0x00FF_FFFF, 0x0100_0000, i32::MAX] {
        for fault in every_variant(raw) {
            assert_eq!(
                InputFault::from_code(fault.code()),
                Some(fault),
                "{fault:?}"
            );
        }
    }
}

/// **A number past 24 bits saturates to the largest 24-bit number**, never
/// wraps into a small one; one that is not positive is no number.
#[test]
fn an_errno_past_24_bits_saturates() {
    assert_eq!(errno(0x00FF_FFFF).map(Errno::get), Some(0x00FF_FFFF));
    assert_eq!(errno(0x0100_0000).map(Errno::get), Some(0x00FF_FFFF));
    assert_eq!(errno(i32::MAX).map(Errno::get), Some(0x00FF_FFFF));
    assert_eq!(errno(0), None);
    assert_eq!(errno(-5), None);
    let over = InputFault::WaitFailed(errno(0x0100_0001));
    assert_eq!(over.code().get() & 0x00FF_FFFF, 0x00FF_FFFF);
}

/// **The payload and the kind are both in the code**: two errnos are two
/// codes, and kinds with the same payload are distinct codes.
#[test]
fn the_payload_and_the_kind_are_both_in_the_code() {
    let wait = |raw| InputFault::WaitFailed(errno(raw)).code();
    assert_ne!(wait(5), wait(6));
    assert_ne!(wait(5), InputFault::ReadFailed(errno(5)).code());
    assert_ne!(wait(5), InputFault::WaitFailed(None).code());
    assert_eq!(wait(5).get(), (2 << 24) | 5);
    assert_eq!(InputFault::WaitFailed(None).code().get(), 2 << 24);
}

/// **A code no fault packs to reads as none**: kind `0`, an unknown kind, and
/// each unit kind carrying a payload.
#[test]
fn a_code_no_fault_packs_to_is_none() {
    for code in [
        5u32,
        0x0700_0000,
        0xFF00_0000,
        0x0100_0001,
        0x0100_0000 | 0x00FF_FFFF,
        0x0400_0001,
        0x0500_0001,
        0x0600_0001,
    ] {
        assert_eq!(
            InputFault::from_code(NonZeroU32::new(code).unwrap()),
            None,
            "{code:#x}"
        );
    }
}

/// **`Errno` keeps the range contract** over `1..=0xFF_FFFF`.
#[test]
fn errno_keeps_the_range_contract() {
    let contract = RangeContract {
        new: Errno::new,
        get: Errno::get,
        of: Some(Errno::of),
        valid: &[1, 5, 0x00FF_FFFF],
        invalid: &[0, -1, 0x0100_0000, i32::MIN, i32::MAX],
    };
    check_range_contract(&contract);
    #[cfg(feature = "serde")]
    check_range_serde(&contract);
}
