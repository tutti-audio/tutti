use super::*;

fn every_variant(raw: i32) -> [InputFault; 5] {
    [
        InputFault::Panicked,
        InputFault::wait_failed(raw),
        InputFault::read_failed(Some(raw)),
        InputFault::PortBroken,
        InputFault::DriverClosed,
    ]
}

/// **The `Cause` law holds for every variant at the payload edges**: `0`, the
/// largest 24-bit number, one past it, and the largest `i32`.
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
    let unknown = InputFault::read_failed(None);
    assert_eq!(InputFault::from_code(unknown.code()), Some(unknown));
}

/// **An errno past 24 bits saturates to the largest 24-bit number**, never
/// wraps into a small one; one that is not positive is "unknown", `0`.
#[test]
fn an_errno_past_24_bits_saturates() {
    assert_eq!(Errno::saturating(0x00FF_FFFF).get(), 0x00FF_FFFF);
    assert_eq!(Errno::saturating(0x0100_0000).get(), 0x00FF_FFFF);
    assert_eq!(Errno::saturating(i32::MAX).get(), 0x00FF_FFFF);
    assert_eq!(Errno::saturating(-5).get(), 0);
    let over = InputFault::wait_failed(0x0100_0001);
    assert_eq!(over, InputFault::WaitFailed(Errno::saturating(0x00FF_FFFF)));
    assert_eq!(over.code().get() & 0x00FF_FFFF, 0x00FF_FFFF);
}

/// **A fault's payload survives the code**: two errnos are two codes, and the
/// kinds with the same payload are distinct codes.
#[test]
fn the_payload_and_the_kind_are_both_in_the_code() {
    assert_ne!(
        InputFault::wait_failed(5).code(),
        InputFault::wait_failed(6).code()
    );
    assert_ne!(
        InputFault::wait_failed(5).code(),
        InputFault::read_failed(Some(5)).code()
    );
    assert_eq!(InputFault::wait_failed(5).code().get(), (2 << 24) | 5);
}

/// A code no fault packs to reads as none: kind `0`, an unknown kind, a unit
/// kind with a payload.
#[test]
fn a_code_no_fault_packs_to_is_none() {
    for code in [5u32, 0x0600_0000, 0x0100_0001, 0xFF00_0000] {
        assert_eq!(
            InputFault::from_code(NonZeroU32::new(code).unwrap()),
            None,
            "{code:#x}"
        );
    }
}
