use super::testing::{SRC, decoded, header, unread};
use super::*;
use tutti_ump::DeviceIdentity;
use tutti_ump::ManufacturerId;
use tutti_ump::SoftwareRevision;

/// Spec: m2-101/3.3#1
#[test]
fn muid_refuses_the_reserved_range_and_the_broadcast_value() {
    assert_eq!(Muid::new(0), Some(Muid(0)));
    assert_eq!(Muid::new(0x0FFF_FEFF).map(Muid::get), Some(0x0FFF_FEFF));
    // The reserved range, both ends, and the broadcast value (§3.3).
    for raw in [0x0FFF_FF00, 0x0FFF_FF01, 0x0FFF_FFFE, Muid::BROADCAST_RAW] {
        assert_eq!(Muid::new(raw), None, "{raw:#x}");
        assert_eq!(Muid::try_from(raw), Err(NotAMuid(raw)));
    }
    // Past 28 bits.
    assert_eq!(Muid::new(0x1000_0000), None);
    assert_eq!(Muid::new(u32::MAX), None);
    assert_eq!(u32::from(Muid::of(0x0123_4567)), 0x0123_4567);
    assert_eq!(Muid::try_from(7), Ok(Muid(7)));
}

#[test]
fn muid_round_trips_through_wire_bytes() {
    for raw in [0u32, 1, 0x0123_4567, 0x0FFF_FEFF] {
        let m = Muid::of(raw);
        assert_eq!(Muid::from_bytes(m.to_bytes()), Some(m));
    }
    // Least significant seven bits first.
    assert_eq!(Muid::of(0x0123_4567).to_bytes(), [0x67, 0x0A, 0x0D, 0x09]);
    // A byte with its top bit set, and the reserved range, are not a MUID.
    assert_eq!(Muid::from_bytes([0x80, 0, 0, 0]), None);
    assert_eq!(Muid::from_bytes([0x7F, 0x7F, 0x7F, 0x7F]), None);
    assert_eq!(Muid::from_bytes([0x00, 0x7E, 0x7F, 0x7F]), None);
}

fn sample_body() -> DeviceBody {
    DeviceBody::new(
        DeviceIdentity {
            manufacturer: ManufacturerId::of(0xA109),
            family: [0x34, 0x24].map(U7::of),
            model: [0x55, 0x00].map(U7::of),
            revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
        },
        CiCategories::PROFILE_CONFIGURATION,
        MaxSysexSize::of(512),
    )
}

fn sample() -> DiscoveryData {
    DiscoveryData::new(sample_body())
}

fn sample_reply() -> ReplyData {
    ReplyData::new(sample_body())
}

/// A broadcast Invalidate MUID with the target `5`, as bytes.
fn invalidate() -> Vec<u8> {
    CiMessage::InvalidateMuid {
        header: header(),
        target: Muid::of(5),
    }
    .encode()
}

#[test]
fn invalidate_muid_round_trips() {
    let msg = CiMessage::InvalidateMuid {
        header: header(),
        target: Muid::of(0x0ABC_DEF0 & 0x0FFF_FFFF),
    };
    let bytes = msg.encode();
    assert_eq!(
        &bytes[..5],
        [0x7E, 0x7F, 0x0D, 0x7E, CI_VERSION.get()],
        "§5.2.1 Table 5"
    );
    assert_eq!(&bytes[9..13], [0x7F; 4], "broadcast destination");
    assert_eq!(decoded(&bytes), msg);
}

#[test]
fn a_reply_keeps_the_device_it_goes_to() {
    let msg = CiMessage::Nak {
        header: header(),
        to: Muid::of(0x0042_4242),
        nak: Nak::new(U7::of(0x20), NakStatus::MessageNotSupported),
    };
    let bytes = msg.encode();
    assert_eq!(&bytes[9..13], Muid::of(0x0042_4242).to_bytes());
    assert_eq!(decoded(&bytes), msg);
    assert_eq!(msg.destination(), Destination::Muid(Muid::of(0x0042_4242)));
}

#[test]
fn non_ci_sysex_is_not_ci() {
    // Not `7E <id> 0D ...`: a manufacturer message, a different Universal
    // SysEx sub-ID#1, and a message too short for the preamble.
    assert_eq!(CiMessage::decode(&[0x01, 0x02, 0x03]), Decoded::NotCi);
    let mut other = invalidate();
    other[2] = 0x06;
    assert_eq!(CiMessage::decode(&other), Decoded::NotCi);
    assert_eq!(CiMessage::decode(&invalidate()[..12]), Decoded::NotCi);
}

// §5.3 and §5.11: a device owes a NAK to a message it does not understand, so a
// message that is not read keeps the header it was addressed by. This is the
// Unread of a sub-ID#2 no message uses (0x3E), a kind this codec does not model.
#[test]
fn a_message_that_is_not_read_keeps_its_header() {
    let mut bytes = invalidate();
    bytes[1] = 0x03; // Device ID: channel 4
    bytes[3] = 0x3E; // no message
    bytes[4] = 0x01;
    let got = unread(&bytes);
    assert_eq!(
        got,
        Unread {
            device_id: 0x03,
            sub_id2: 0x3E,
            version: 0x01,
            source: Some(Muid::of(SRC)),
            destination: Some(Destination::Broadcast),
            reason: UnreadReason::UnsupportedMessage,
        }
    );
    // The NAK is addressed to the sender, from `own`, on the same Device ID,
    // naming the message, with the message-not-supported status (Table 16 0x01).
    let reply = got.nak(Muid::of(0x77)).expect("the sender is known");
    assert_eq!(
        reply,
        CiMessage::Nak {
            header: CiHeader {
                device_id: CiDeviceId::Channel(MidiChannel::of(3)),
                ci_version: CI_VERSION,
                source: Muid::of(0x77),
            },
            to: Muid::of(SRC),
            nak: Nak::new(U7::of(0x3E), NakStatus::MessageNotSupported),
        }
    );
    assert_eq!(decoded(&reply.encode()), reply);
}

// §5.3: a message whose Message Format Version has reserved bits set is
// answered with a NAK (error code 0x02); §5.4: the NAK uses the receiver's own
// Message Format Version and Status Code 0x02.
/// Spec: m2-101/5.3#7
#[test]
fn reserved_version_bits_are_nakked_with_0x02_in_the_receivers_version() {
    for version in [0x20, 0x40, 0x80, 0xE2, 0x7F] {
        let mut bytes = invalidate();
        bytes[4] = version;
        let got = unread(&bytes);
        assert_eq!(
            got.reason,
            UnreadReason::ReservedVersionBits,
            "{version:#x}"
        );
        assert_eq!(got.version, version, "the version as sent is kept");
        let CiMessage::Nak { header, nak, .. } = got.nak(Muid::of(9)).unwrap() else {
            panic!("a NAK")
        };
        assert_eq!(nak.status, NakStatus::VersionNotSupported);
        assert_eq!(header.ci_version, CI_VERSION);
    }
}

// §5.4: Message Format Version 0x00 is deprecated.
/// Spec: m2-101/5.4#3, m2-101/5.4#2
#[test]
fn version_zero_is_refused() {
    let mut bytes = invalidate();
    bytes[4] = 0x00;
    assert_eq!(unread(&bytes).reason, UnreadReason::VersionZero);
}

// §5.3: a device that receives a version higher than its own processes only the
// fields of its own version and ignores any appended ones: a version the reserved bits do not
// forbid is read, with what it appended.
/// Spec: m2-101/5.3#5, m2-101/5.4#6, m2-101/5.4.1#1, m2-101/5.4.1#2
#[test]
fn a_higher_version_is_read_and_its_appended_fields_ignored() {
    let mut bytes = invalidate();
    bytes[4] = 0x05;
    bytes.extend_from_slice(&[0x11, 0x22]);
    let CiMessage::InvalidateMuid { header, target } = decoded(&bytes) else {
        panic!("Invalidate MUID")
    };
    assert_eq!(header.ci_version, CiVersion::of(5));
    assert_eq!(target, Muid::of(5));
}

// §3.2.1 Table 5: Device IDs 0x10 to 0x7D are reserved.
#[test]
fn a_reserved_device_id_is_refused() {
    for id in [0x10, 0x11, 0x40, 0x7D] {
        let mut bytes = invalidate();
        bytes[1] = id;
        let got = unread(&bytes);
        assert_eq!(got.reason, UnreadReason::ReservedDeviceId, "{id:#x}");
        // The NAK still goes out, on the Function Block.
        let CiMessage::Nak { header, nak, .. } = got.nak(Muid::of(9)).unwrap() else {
            panic!("a NAK")
        };
        assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
        assert_eq!(nak.status, NakStatus::Malformed);
    }
    for id in (0x00..=0x0F).chain([0x7E, 0x7F]) {
        let mut bytes = invalidate();
        bytes[1] = id;
        let CiMessage::InvalidateMuid { header, .. } = decoded(&bytes) else {
            panic!("Invalidate MUID")
        };
        assert_eq!(header.device_id.get(), id);
    }
}

#[test]
fn a_source_no_device_can_own_is_unread_with_nobody_to_answer() {
    // The broadcast value as the source (bytes 5 to 8) is malformed; as the
    // destination it is fine.
    let mut bytes = invalidate();
    bytes[5..9].copy_from_slice(&[0x7F; 4]);
    let got = unread(&bytes);
    assert_eq!(got.reason, UnreadReason::BadSource);
    assert_eq!(got.source, None);
    assert_eq!(got.nak(Muid::of(9)), None);
    // A reserved value as the invalidated target is malformed too, but the
    // header is fine, so it can be answered.
    let mut bytes = invalidate();
    bytes[13..17].copy_from_slice(&[0x00, 0x7E, 0x7F, 0x7F]);
    let got = unread(&bytes);
    assert_eq!(got.reason, UnreadReason::Malformed);
    assert!(got.nak(Muid::of(9)).is_some());
    // A reserved destination.
    let mut bytes = invalidate();
    bytes[9..13].copy_from_slice(&[0x00, 0x7E, 0x7F, 0x7F]);
    assert_eq!(unread(&bytes).reason, UnreadReason::BadDestination);
}

// §3.3.3: the Broadcast MUID is used only for a short list of messages,
// Discovery and Invalidate MUID among them; Tables 6 and 12 give those two the
// Broadcast MUID as destination. So the other three kinds refuse it, and those
// two refuse a unicast one.
/// Spec: m2-101/3.3.3#2, m2-101/3.3.3#3
#[test]
fn the_broadcast_rule_is_held_in_both_directions() {
    let mut discovery_to_one = CiMessage::Discovery {
        header: header(),
        data: sample(),
    }
    .encode();
    discovery_to_one[9..13].copy_from_slice(&Muid::of(7).to_bytes());
    assert_eq!(
        unread(&discovery_to_one).reason,
        UnreadReason::WrongDestination
    );

    let mut invalidate_to_one = invalidate();
    invalidate_to_one[9..13].copy_from_slice(&Muid::of(7).to_bytes());
    assert_eq!(
        unread(&invalidate_to_one).reason,
        UnreadReason::WrongDestination
    );

    for msg in [
        CiMessage::DiscoveryReply {
            header: header(),
            to: Muid::of(7),
            data: sample_reply(),
        },
        CiMessage::Ack {
            header: header(),
            to: Muid::of(7),
            ack: Ack::new(U7::of(0x34), AckStatus::Ack),
        },
        CiMessage::Nak {
            header: header(),
            to: Muid::of(7),
            nak: Nak::new(U7::of(0x34), NakStatus::Nak),
        },
    ] {
        let mut bytes = msg.encode();
        assert_eq!(decoded(&bytes), msg);
        bytes[9..13].copy_from_slice(&[0x7F; 4]);
        assert_eq!(
            unread(&bytes).reason,
            UnreadReason::WrongDestination,
            "{msg:?}"
        );
    }
}

// An ACK or a NAK is never answered with a NAK.
#[test]
fn an_unreadable_ack_or_nak_is_not_nakked() {
    for kind in [SUB_ID2_ACK, SUB_ID2_NAK] {
        let mut bytes = invalidate();
        bytes[3] = kind;
        bytes.truncate(13); // a unicast body is missing: malformed
        bytes[9..13].copy_from_slice(&Muid::of(7).to_bytes());
        bytes.extend_from_slice(&[0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0x05, 0x00]); // ml 5, no text
        let got = unread(&bytes);
        assert_eq!(got.reason, UnreadReason::Malformed);
        assert_eq!(got.nak(Muid::of(9)), None);
    }
}

/// M2-101 Table 5: `7E` is the whole Group and `7F` the whole Function Block;
/// each whole is the Device ID and the Profile target of its own byte.
#[test]
fn a_whole_is_the_device_id_and_profile_target_of_its_byte() {
    for (whole, byte, target) in [
        (Whole::Group, 0x7E, ProfileTarget::Group),
        (Whole::FunctionBlock, 0x7F, ProfileTarget::FunctionBlock),
    ] {
        let id = CiDeviceId::from(whole);
        assert_eq!(id.get(), byte);
        assert_eq!(ProfileTarget::from(whole), target);
        assert_eq!(target.address(), id);
    }
}

#[test]
fn the_device_id_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    let c = RangeContract {
        new: CiDeviceId::new,
        get: CiDeviceId::get,
        of: None,
        valid: &[0x00, 0x05, 0x0F, 0x7E, 0x7F],
        invalid: &[0x10, 0x7D, 0x80, 0xFF],
    };
    check_range_contract(&c);
}

/// The ranged values of the CI messages keep the range-checked contract at
/// each edge of their spec range.
#[test]
fn the_ci_ranged_values_keep_the_range_contract() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    macro_rules! contract {
        ($ty:ty, $raw:ty, $valid:expr, $invalid:expr) => {{
            let c = RangeContract {
                new: <$ty>::new,
                get: <$ty>::get,
                of: Some(<$ty>::of),
                valid: &$valid,
                invalid: &$invalid,
            };
            check_range_contract(&c);
        }};
    }
    contract!(
        Muid,
        u32,
        [0, 1, 0x0FFF_FEFF],
        [0x0FFF_FF00, 0x0FFF_FFFF, 0x1000_0000, u32::MAX]
    );
    contract!(
        MaxSysexSize,
        u32,
        [128, 512, 0x0FFF_FFFF],
        [0, 127, 0x1000_0000]
    );
    contract!(TimeoutWait, u8, [0, 1, 127], [128, 255]);
    contract!(PeRequestId, u8, [0, 127], [128, 255]);
    contract!(ChunkNo, u16, [1, 0x3FFF], [0, 0x4000]);
    contract!(PeChannel, u16, [1, 256], [0, 257, u16::MAX]);
    contract!(Priority, u8, [1, 5], [0, 6]);
    contract!(SignificantBits, u8, [1, 32], [0, 33]);
    contract!(StepCount, u32, [1, u32::MAX], [0]);
    contract!(
        UnixSeconds,
        u64,
        [0, 0x7FFF_FFFF_FFFF_FFFF],
        [0x8000_0000_0000_0000, u64::MAX]
    );
    contract!(
        StateSize,
        u64,
        [0, 0x7FFF_FFFF_FFFF_FFFF],
        [0x8000_0000_0000_0000, u64::MAX]
    );
    check_range_contract(&RangeContract {
        new: ProfileChannels::new,
        get: ProfileChannels::get,
        of: Some(ProfileChannels::of),
        valid: &[0, 1, 0x3FFF],
        invalid: &[0x4000, u16::MAX],
    });
}

fn long_message() -> CiMessage {
    CiMessage::DiscoveryReply {
        header: header(),
        to: Muid::of(2),
        data: sample_reply(),
    }
}

/// M2-104 section 7.7: the payload as SysEx7 packets of six bytes, a Start,
/// Continues and an End; reassembled they are the payload.
#[test]
fn packets_carry_the_payload_in_sysex7_packets() {
    use tutti_ump::{Message, MidiGroup, SysexStatus};
    let message = long_message();
    let payload = message.encode();
    let packets: Vec<_> = message.packets(MidiGroup::of(3)).unwrap().collect();
    assert_eq!(packets.len(), payload.len().div_ceil(6));
    let mut bytes = Vec::new();
    for (i, ump) in packets.iter().enumerate() {
        let Ok(Message::Sysex7(group, packet)) = ump.decode() else {
            panic!("not a SysEx7 packet: {ump:?}")
        };
        assert_eq!(group, MidiGroup::of(3));
        let want = match (i, packets.len()) {
            (0, 1) => SysexStatus::Complete,
            (0, _) => SysexStatus::Start,
            (i, n) if i + 1 == n => SysexStatus::End,
            _ => SysexStatus::Continue,
        };
        assert_eq!(packet.status(), want);
        bytes.extend_from_slice(packet.data());
    }
    assert_eq!(bytes, payload);
}

#[test]
fn the_muid_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: Muid::new,
        get: Muid::get,
        of: Some(Muid::of),
        valid: &[0, 1, 0x0FFF_FEFF],
        invalid: &[
            0x0FFF_FF00,
            0x0FFF_FFFE,
            Muid::BROADCAST_RAW,
            0x1000_0000,
            u32::MAX,
        ],
    });
}

/// M2-101 Appendix E: Property Exchange messages are Sub-ID#2 `30` to `3F` under
/// Universal Non-Real Time and Sub-ID#1 `0D`, on any Device ID; Set Profile On
/// is `22`.
#[test]
fn the_preamble_tells_property_exchange_and_set_profile_on() {
    let message = |device: u8, sub_id1: u8, sub_id2: u8| [0x7E, device, sub_id1, sub_id2, 0x02];
    for device in [0x00, 0x0F, 0x7E, 0x7F] {
        for sub_id2 in 0x30..=0x3F {
            assert!(is_property_exchange(&message(device, 0x0D, sub_id2)));
            assert!(!is_set_profile_on(&message(device, 0x0D, sub_id2)));
        }
        assert!(is_set_profile_on(&message(device, 0x0D, 0x22)));
        assert!(!is_property_exchange(&message(device, 0x0D, 0x22)));
    }
    // The edges of the range, another Sub-ID#1, Real Time, and too short.
    assert!(!is_property_exchange(&message(0x7F, 0x0D, 0x2F)));
    assert!(!is_property_exchange(&message(0x7F, 0x0D, 0x40)));
    assert!(!is_property_exchange(&message(0x7F, 0x0C, 0x34)));
    assert!(!is_set_profile_on(&message(0x7F, 0x0C, 0x22)));
    let mut real_time = message(0x7F, 0x0D, 0x34);
    real_time[0] = 0x7F;
    assert!(!is_property_exchange(&real_time));
    assert!(!is_set_profile_on(&[0x7E, 0x7F, 0x0D]));
    assert!(!is_property_exchange(&[]));
    // Only the preamble is read: a first packet's data can stop at it.
    assert!(is_property_exchange(&[0x7E, 0x7F, 0x0D, 0x34]));
    assert!(is_set_profile_on(&[0x7E, 0x7F, 0x0D, 0x22]));
}

/// A payload byte of 128 or more is refused with its index, never dropped from
/// the packets: M2-104 §7.7 SysEx7 carries 7-bit bytes only.
#[test]
fn a_payload_that_is_not_seven_bit_is_refused_not_sent_short() {
    let group = MidiGroup::of(3);
    assert_eq!(
        sysex7_packets(group, &[0x01, 0x80, 0x02]).err(),
        Some(tutti_ump::SysexError::NotSevenBit { index: 1 })
    );
    assert_eq!(sysex7_packets(group, &[0x01, 0x7F]).unwrap().count(), 1);
}
