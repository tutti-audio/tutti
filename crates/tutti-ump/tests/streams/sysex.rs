//! `SysexMessage`: a borrowed, checked, complete MIDI 1.0 SysEx message.

use tutti_ump::{
    IDENTITY_REQUEST, SysexError, SysexMessage, UNIVERSAL_NON_REAL_TIME, UNIVERSAL_REAL_TIME,
};

/// The least message is the two delimiters.
#[test]
fn the_minimum_message_is_f0_f7() {
    let message = SysexMessage::from_framed(&[0xF0, 0xF7]).unwrap();
    assert_eq!(message.bytes(), [0xF0, 0xF7]);
    assert_eq!(message.payload(), [] as [u8; 0]);
}

/// A message with a payload hands back both views.
#[test]
fn bytes_and_payload() {
    let raw = [0xF0, 0x7E, 0x7F, 0x00, 0xF7];
    let message = SysexMessage::from_framed(&raw).unwrap();
    assert_eq!(message.bytes(), raw);
    assert_eq!(message.payload(), [0x7E, 0x7F, 0x00]);
    assert_eq!(<&[u8]>::from(message), raw);
}

/// A message that does not start with `F0` (or is empty) is refused as
/// `MissingStart`, whatever else is wrong with it.
#[test]
fn no_f0_is_missing_start() {
    for raw in [
        &[][..],
        &[0x7E, 0x7F, 0xF7],
        &[0xF7, 0xF7],
        &[0xF7],
        &[0x80, 0xF7],
    ] {
        assert_eq!(
            SysexMessage::from_framed(raw),
            Err(SysexError::MissingStart),
            "{raw:x?}"
        );
    }
}

/// A message that starts with `F0` but does not end with `F7` is refused as
/// `MissingEnd`; the lone `F0` is one such byte, since one byte cannot be both
/// delimiters.
#[test]
fn no_f7_is_missing_end() {
    for raw in [
        &[0xF0][..],
        &[0xF0, 0x7E, 0x7F],
        &[0xF0, 0xF0],
        &[0xF0, 0x01, 0xFF],
    ] {
        assert_eq!(
            SysexMessage::from_framed(raw),
            Err(SysexError::MissingEnd),
            "{raw:x?}"
        );
    }
}

/// An interior byte of 0x80 or more (a status byte and `F7` included) is
/// refused with its position counted from the `F0`.
#[test]
fn a_wide_interior_byte_names_its_index() {
    for bad in [0x80, 0x90, 0xF0, 0xF7, 0xFF] {
        assert_eq!(
            SysexMessage::from_framed(&[0xF0, 0x01, bad, 0x02, 0xF7]),
            Err(SysexError::NotSevenBit { index: 2 }),
            "{bad:#x}"
        );
    }
    assert!(SysexMessage::from_framed(&[0xF0, 0x7F, 0xF7]).is_ok());
}

/// `TryFrom` is `from_framed`, and the error says why in words.
#[test]
fn try_from_agrees_with_from_framed() {
    let cases: [&[u8]; 5] = [
        &[0xF0, 0xF7],
        &[0xF0, 1, 2, 0xF7],
        &[1, 2],
        &[0xF0, 0x80, 0xF7],
        &[],
    ];
    for raw in cases {
        assert_eq!(SysexMessage::try_from(raw), SysexMessage::from_framed(raw));
    }
    let why = SysexMessage::try_from(&[1u8, 2][..])
        .unwrap_err()
        .to_string();
    assert!(why.contains("0xF0"), "{why}");
}

/// M1 p.34: the Universal System Exclusive ids are `7E` (Non-Real Time) and `7F`
/// (Real Time), and M1 p.40's Identity Request to every device is
/// `F0 7E 7F 06 01 F7`.
#[test]
fn the_universal_ids_and_the_identity_request_are_m1s() {
    assert_eq!(UNIVERSAL_NON_REAL_TIME, 0x7E);
    assert_eq!(UNIVERSAL_REAL_TIME, 0x7F);
    let framed = [0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7];
    let request = SysexMessage::from_framed(&framed).unwrap();
    assert_eq!(request.payload(), IDENTITY_REQUEST);
}
