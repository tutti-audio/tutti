use proptest::prelude::*;
use tutti_ump::{ManufacturerId, MidiChannel, U7, U28};

use super::super::testing::{DST, SRC, channel, decoded, header_on, preamble, to_dst, unread};
use super::super::*;

const BROADCAST: [u8; 4] = [0x7F; 4];

fn piano() -> ProfileId {
    ProfileId::standard(0x01, 0x02, 0x03, 0x04)
}

fn vendor() -> ProfileId {
    ProfileId::from_bytes([0x00, 0x21, 0x09, 0x05, 0x06]).unwrap()
}

fn one_byte_vendor() -> ProfileId {
    ProfileId::from_bytes([0x41, 0x00, 0x00, 0x07, 0x08]).unwrap()
}

impl ProfileId {
    fn standard(bank: u8, number: u8, version: u8, level: u8) -> Self {
        Self::Standard(StandardProfile::new(
            U7::of(bank),
            U7::of(number),
            U7::of(version),
            U7::of(level),
        ))
    }
}

#[test]
fn a_standard_profile_id_is_7e_bank_number_version_level() {
    // §7.3.1, Table 19.
    assert_eq!(piano().to_bytes(), [0x7E, 0x01, 0x02, 0x03, 0x04]);
    let ProfileId::Standard(p) = piano() else {
        unreachable!()
    };
    assert_eq!(
        (p.bank(), p.number(), p.version(), p.level()),
        (U7::of(1), U7::of(2), U7::of(3), U7::of(4))
    );
}

/// M2-120 Table 4: the MPE Profile's five bytes.
#[test]
fn the_mpe_profile_id_is_table_4_of_m2_120() {
    assert_eq!(ProfileId::MPE.to_bytes(), [0x7E, 0x31, 0x00, 0x01, 0x01]);
    assert_eq!(
        ProfileId::from_bytes([0x7E, 0x31, 0x00, 0x01, 0x01]),
        Some(ProfileId::MPE)
    );
}

#[test]
fn a_manufacturer_profile_id_is_its_sysex_id_then_two_info_bytes() {
    // §7.3.1: a three-byte id fills bytes 1 to 3; a one-byte id is in the first byte
    // and the remaining two bytes are zeroes.
    let ProfileId::Manufacturer(p) = vendor() else {
        panic!("a manufacturer's")
    };
    assert_eq!(p.manufacturer(), ManufacturerId::of(0xA109));
    assert_eq!(p.info(), [U7::of(5), U7::of(6)]);
    assert_eq!(vendor().to_bytes(), [0x00, 0x21, 0x09, 0x05, 0x06]);
    let ProfileId::Manufacturer(p) = one_byte_vendor() else {
        panic!("a manufacturer's")
    };
    assert_eq!(p.manufacturer(), ManufacturerId::of(0x41));
    assert_eq!(one_byte_vendor().to_bytes(), [0x41, 0, 0, 0x07, 0x08]);
}

#[test]
fn a_profile_id_that_is_not_one_is_refused() {
    // A one-byte id followed by a non-zero byte, and a byte with its top bit set,
    // in each position.
    assert_eq!(ProfileId::from_bytes([0x41, 0x01, 0x00, 0, 0]), None);
    assert_eq!(ProfileId::from_bytes([0x41, 0x00, 0x01, 0, 0]), None);
    for i in 0..5 {
        let mut bytes = [0x7E, 1, 2, 3, 4];
        bytes[i] = 0x80;
        assert_eq!(ProfileId::from_bytes(bytes), None, "byte {i}");
    }
    // A manufacturer with the id `7E` would read back as a Standard Profile.
    assert_eq!(
        ManufacturerProfile::new(ManufacturerId::UNIVERSAL_NON_REAL_TIME, [U7::of(0); 2]),
        None
    );
}

#[test]
fn the_inquiry_is_table_17_byte_for_byte() {
    // Device ID is the address asked; no body.
    let message = CiMessage::ProfileInquiry {
        header: header_on(channel(3)),
        to: Muid::of(DST),
    };
    let want = preamble(0x03, 0x20, to_dst());
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    assert_eq!(SUB_ID2_PROFILE_INQUIRY, 0x20);
    assert_eq!(message.destination(), Destination::Muid(Muid::of(DST)));
}

#[test]
fn the_reply_is_table_18_enabled_list_then_disabled_list() {
    let message = CiMessage::ProfileReply {
        header: header_on(CiDeviceId::Group),
        to: Muid::of(DST),
        profiles: ProfileLists::new(
            ProfileList::try_from(vec![piano(), vendor()]).unwrap(),
            ProfileList::try_from(vec![one_byte_vendor()]).unwrap(),
        ),
    };
    let mut want = preamble(0x7E, 0x21, to_dst());
    want.extend([2, 0]); // cep, LSB first
    want.extend([0x7E, 1, 2, 3, 4]);
    want.extend([0x00, 0x21, 0x09, 5, 6]);
    want.extend([1, 0]); // cdp
    want.extend([0x41, 0, 0, 7, 8]);
    assert_eq!(message.encode(), want);
    let CiMessage::ProfileReply { profiles, .. } = decoded(&want) else {
        panic!("a reply")
    };
    assert_eq!(profiles.enabled(), [piano(), vendor()]);
    assert_eq!(profiles.disabled(), [one_byte_vendor()]);
}

#[test]
fn a_reply_declaring_no_profiles_is_two_zero_counts() {
    // §7.13.
    let message = CiMessage::ProfileReply {
        header: header_on(CiDeviceId::FunctionBlock),
        to: Muid::of(DST),
        profiles: ProfileLists::none(),
    };
    let mut want = preamble(0x7F, 0x21, to_dst());
    want.extend([0, 0, 0, 0]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
}

#[test]
fn a_reply_whose_count_runs_past_the_message_is_malformed() {
    let mut bytes = preamble(0x7F, 0x21, to_dst());
    bytes.extend([2, 0, 0x7E, 1, 2, 3, 4]); // two enabled declared, one there
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
    let mut bytes = preamble(0x7F, 0x21, to_dst());
    bytes.extend([0, 0]); // no second count
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
}

#[test]
fn a_list_of_more_than_14_bits_is_refused_and_the_most_is_written() {
    let id = piano();
    assert_eq!(
        ProfileList::try_from(vec![id; 0x4000]),
        Err(ListTooLong { len: 0x4000 })
    );
    assert_eq!(ProfileList::new(vec![id; 0x4000]), None);
    let full = ProfileLists::new(
        ProfileList::try_from(vec![id; 0x3FFF]).unwrap(),
        ProfileList::default(),
    );
    let message = CiMessage::ProfileReply {
        header: header_on(CiDeviceId::FunctionBlock),
        to: Muid::of(DST),
        profiles: full.clone(),
    };
    let bytes = message.encode();
    assert_eq!(&bytes[13..15], [0x7F, 0x7F]);
    let CiMessage::ProfileReply { profiles, .. } = decoded(&bytes) else {
        panic!("a reply")
    };
    assert_eq!(profiles, full);
}

#[test]
fn set_on_is_table_24_with_the_channels_requested_lsb_first() {
    let message = CiMessage::SetProfileOn {
        source: Muid::of(SRC),
        ci_version: CI_VERSION,
        to: Muid::of(DST),
        target: ProfileTarget::Channel {
            channel: MidiChannel::of(0),
            channels: ProfileChannels::of(130),
        },
        profile: piano(),
    };
    let mut want = preamble(0x00, 0x22, to_dst());
    want.extend([0x7E, 1, 2, 3, 4]);
    want.extend([0x02, 0x01]); // 130 = 1 << 7 | 2
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
}

/// Spec: m2-101/7.8#1
#[test]
fn a_set_on_to_the_group_or_function_block_is_table_24_with_zero_channels() {
    for (device, target) in [
        (0x7Eu8, ProfileTarget::Group),
        (0x7F, ProfileTarget::FunctionBlock),
    ] {
        let message = CiMessage::SetProfileOn {
            source: Muid::of(SRC),
            ci_version: CI_VERSION,
            to: Muid::of(DST),
            target,
            profile: piano(),
        };
        let mut want = preamble(device, 0x22, to_dst());
        want.extend([0x7E, 1, 2, 3, 4, 0, 0]);
        assert_eq!(message.encode(), want, "{device:#x}");
        assert_eq!(decoded(&want), message);
    }
}

/// Tables 26 and 27: a report at the Group or Function Block has `00 00` for
/// its count and the Broadcast MUID as its destination.
#[test]
fn enabled_and_disabled_reports_at_the_group_or_function_block_are_zero_count_bytes() {
    for (sub, enabled) in [(0x24u8, true), (0x25, false)] {
        for (device, target) in [
            (0x7Eu8, ProfileTarget::Group),
            (0x7F, ProfileTarget::FunctionBlock),
        ] {
            let (source, ci_version, profile) = (Muid::of(SRC), CI_VERSION, piano());
            let message = if enabled {
                CiMessage::ProfileEnabled {
                    source,
                    ci_version,
                    target,
                    profile,
                }
            } else {
                CiMessage::ProfileDisabled {
                    source,
                    ci_version,
                    target,
                    profile,
                }
            };
            let mut want = vec![0x7E, device, 0x0D, sub, 0x02];
            want.extend(Muid::of(SRC).to_bytes());
            want.extend([0x7F, 0x7F, 0x7F, 0x7F]);
            want.extend([0x7E, 1, 2, 3, 4, 0, 0]);
            assert_eq!(message.encode(), want, "{sub:#x} at {device:#x}");
            assert_eq!(decoded(&want), message);
        }
    }
}

/// §5.3: a Version 1 Set Profile On has no count field, so a Group address with
/// none is a Group target.
#[test]
fn a_version_1_set_on_to_the_group_has_no_count_bytes() {
    let mut bytes = vec![0x7E, 0x7E, 0x0D, 0x22, 0x01];
    bytes.extend(Muid::of(SRC).to_bytes());
    bytes.extend(to_dst());
    bytes.extend([0x7E, 1, 2, 3, 4]);
    assert_eq!(
        decoded(&bytes),
        CiMessage::SetProfileOn {
            source: Muid::of(SRC),
            ci_version: CiVersion::of(1),
            to: Muid::of(DST),
            target: ProfileTarget::Group,
            profile: piano(),
        }
    );
}

/// §7.8, §7.10 and §7.11 notes: the count is 0x0000 at address 0x7E or 0x7F.
#[test]
fn a_group_or_function_block_address_with_channels_is_not_read() {
    for sub in [0x22u8, 0x24, 0x25] {
        for device in [0x7Eu8, 0x7F] {
            let destination = if sub == 0x22 { to_dst() } else { BROADCAST };
            let mut bytes = preamble(device, sub, destination);
            bytes.extend([0x7E, 1, 2, 3, 4, 3, 0]);
            assert_eq!(
                unread(&bytes).reason,
                UnreadReason::Malformed,
                "{sub:#x} at {device:#x}"
            );
            // The same count at a Channel is read.
            bytes[1] = 0x05;
            assert!(matches!(
                decoded(&bytes),
                CiMessage::SetProfileOn { .. }
                    | CiMessage::ProfileEnabled { .. }
                    | CiMessage::ProfileDisabled { .. }
            ));
        }
    }
}

#[test]
fn a_version_1_set_on_has_no_channels_field_and_reads_as_zero() {
    // §5.3: `Number Channels Requested` was added in Version 2.
    let mut bytes = preamble(0x7F, 0x22, to_dst());
    bytes[4] = 0x01;
    bytes.extend([0x7E, 1, 2, 3, 4]);
    let CiMessage::SetProfileOn { target, .. } = decoded(&bytes) else {
        panic!("a Set On")
    };
    assert_eq!(target, ProfileTarget::FunctionBlock);
    assert_eq!(target.channels(), ProfileChannels::BY_ADDRESS);
    // One byte of the two is malformed, not absent.
    bytes.push(0x05);
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
}

#[test]
fn set_off_is_table_25_with_two_reserved_bytes_that_are_ignored() {
    let message = CiMessage::SetProfileOff {
        header: header_on(CiDeviceId::Group),
        to: Muid::of(DST),
        profile: piano(),
    };
    let mut want = preamble(0x7E, 0x23, to_dst());
    want.extend([0x7E, 1, 2, 3, 4, 0, 0]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    // Reserved bytes: whatever a sender puts there, the message is the same.
    let n = want.len();
    want[n - 2..].copy_from_slice(&[0x55, 0x66]);
    assert_eq!(decoded(&want), message);
}

/// Spec: m2-101/7.10#5, m2-101/7.11#5
#[test]
fn enabled_and_disabled_reports_are_tables_26_and_27_to_the_broadcast_muid() {
    for (sub, enabled) in [(0x24u8, true), (0x25, false)] {
        let (source, profile) = (Muid::of(SRC), piano());
        let target = ProfileTarget::Channel {
            channel: MidiChannel::of(9),
            channels: ProfileChannels::of(3),
        };
        let message = if enabled {
            CiMessage::ProfileEnabled {
                source,
                ci_version: CI_VERSION,
                target,
                profile,
            }
        } else {
            CiMessage::ProfileDisabled {
                source,
                ci_version: CI_VERSION,
                target,
                profile,
            }
        };
        let mut want = preamble(0x09, sub, BROADCAST);
        want.extend([0x7E, 1, 2, 3, 4, 3, 0]);
        assert_eq!(message.encode(), want, "{sub:#x}");
        assert_eq!(decoded(&want), message);
        assert_eq!(message.destination(), Destination::Broadcast);
        // A report always carries the Broadcast MUID as its Destination MUID.
        let mut unicast = want.clone();
        unicast[9..13].copy_from_slice(&to_dst());
        assert_eq!(unread(&unicast).reason, UnreadReason::WrongDestination);
    }
}

#[test]
fn a_version_1_report_has_no_channel_count_and_reads_as_zero() {
    let mut bytes = preamble(0x7F, 0x24, BROADCAST);
    bytes[4] = 0x01;
    bytes.extend([0x7E, 1, 2, 3, 4]);
    let CiMessage::ProfileEnabled {
        ci_version, target, ..
    } = decoded(&bytes)
    else {
        panic!("a report")
    };
    assert_eq!(ci_version.get(), 1);
    assert_eq!(target.channels(), ProfileChannels::BY_ADDRESS);
}

/// Spec: m2-101/7.4#2, m2-101/7.5#2, m2-102/2.4#15, m2-102/2.4#18
#[test]
fn added_and_removed_reports_are_tables_20_and_21_to_the_broadcast_muid() {
    let added = CiMessage::ProfileAdded {
        header: header_on(CiDeviceId::FunctionBlock),
        profile: vendor(),
    };
    let removed = CiMessage::ProfileRemoved {
        header: header_on(CiDeviceId::FunctionBlock),
        profile: vendor(),
    };
    for (message, sub) in [(&added, 0x26u8), (&removed, 0x27)] {
        let mut want = preamble(0x7F, sub, BROADCAST);
        want.extend([0x00, 0x21, 0x09, 5, 6]);
        assert_eq!(message.encode(), want);
        assert_eq!(&decoded(&want), message);
        let mut unicast = want.clone();
        unicast[9..13].copy_from_slice(&to_dst());
        assert_eq!(unread(&unicast).reason, UnreadReason::WrongDestination);
    }
}

#[test]
fn the_details_inquiry_is_table_22_and_the_reply_table_23() {
    let inquiry = CiMessage::ProfileDetailsInquiry {
        header: header_on(channel(2)),
        to: Muid::of(DST),
        profile: piano(),
        target: InquiryTarget::NUMBER_OF_CHANNELS,
    };
    let mut want = preamble(0x02, 0x28, to_dst());
    want.extend([0x7E, 1, 2, 3, 4, 0x00]);
    assert_eq!(inquiry.encode(), want);
    assert_eq!(decoded(&want), inquiry);

    let data = ChannelsDetail {
        in_use: ProfileChannels::of(0),
        max: ProfileChannels::of(200),
    }
    .to_data();
    let reply = CiMessage::ProfileDetailsReply {
        header: header_on(channel(2)),
        to: Muid::of(DST),
        profile: piano(),
        target: InquiryTarget::NUMBER_OF_CHANNELS,
        data: data.clone(),
    };
    let mut want = preamble(0x02, 0x29, to_dst());
    want.extend([0x7E, 1, 2, 3, 4, 0x00]);
    // M2-102 Table 8: length 4, in use 0, maximum 200 = 1 << 7 | 72.
    want.extend([0x04, 0x00, 0x00, 0x00, 72, 1]);
    assert_eq!(reply.encode(), want);
    let CiMessage::ProfileDetailsReply { data: got, .. } = decoded(&want) else {
        panic!("a reply")
    };
    assert_eq!(got.as_channels().unwrap().max, ProfileChannels::of(200));
    assert_eq!(got.as_channels().unwrap().in_use, ProfileChannels::of(0));
    assert_eq!(got, data);
}

/// Spec: m2-101/7.12#1
#[test]
fn profile_specific_data_is_table_28_with_a_four_byte_length() {
    let data = SpecificData::new(vec![1, 2, 3]).unwrap();
    for (to, wire) in [
        (Destination::Muid(Muid::of(DST)), to_dst()),
        (Destination::Broadcast, BROADCAST),
    ] {
        let message = CiMessage::ProfileSpecificData(
            ProfileSpecific::new(header_on(channel(15)), to, vendor(), data.clone()).unwrap(),
        );
        let mut want = preamble(0x0F, 0x2F, wire);
        want.extend([0x00, 0x21, 0x09, 5, 6]);
        want.extend([3, 0, 0, 0]);
        want.extend([1, 2, 3]);
        assert_eq!(message.encode(), want);
        assert_eq!(decoded(&want), message);
        assert_eq!(message.destination(), to);
    }
    // A length of 128 is `00 01 00 00`.
    let long = CiMessage::ProfileSpecificData(
        ProfileSpecific::new(
            header_on(channel(0)),
            Destination::Muid(Muid::of(DST)),
            piano(),
            SpecificData::new(vec![9; 128]).unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(&long.encode()[18..22], [0x00, 0x01, 0x00, 0x00]);
    assert_eq!(decoded(&long.encode()), long);
}

/// Spec: m2-101/7.12#2
#[test]
fn profile_specific_data_to_the_broadcast_muid_is_at_most_512_bytes_with_its_delimiters() {
    let specific = |to, len| {
        ProfileSpecific::new(
            header_on(channel(0)),
            to,
            vendor(),
            SpecificData::new(vec![5; len]).unwrap(),
        )
    };
    let most = ProfileSpecific::BROADCAST_MAX_DATA;
    let at_limit = CiMessage::ProfileSpecificData(specific(Destination::Broadcast, most).unwrap());
    // F0 and F7 around the payload `encode` gives.
    assert_eq!(at_limit.encode().len() + 2, 512);
    assert_eq!(specific(Destination::Broadcast, most + 1), None);
    // One device's buffer is the receiver's to have declared (§5.5.3): no cap here.
    let to_one = specific(Destination::Muid(Muid::of(DST)), most + 1).unwrap();
    assert_eq!(to_one.data().as_bytes().len(), most + 1);
    // A message that arrives is read whatever its size.
    let mut oversized = preamble(0x00, 0x2F, BROADCAST);
    oversized.extend([0x00, 0x21, 0x09, 5, 6]);
    oversized.extend(U28::of((most + 1) as u32).to_le7());
    oversized.extend(vec![5; most + 1]);
    let Decoded::Message(CiMessage::ProfileSpecificData(read)) = CiMessage::decode(&oversized)
    else {
        panic!("a Profile Specific Data message");
    };
    assert_eq!(read.data().as_bytes().len(), most + 1);
}

#[test]
fn a_length_that_runs_past_the_message_or_holds_a_non_data_byte_is_malformed() {
    // (case, sub-ID#2, the body after the preamble)
    let cases: [(&str, u8, u8, Vec<u8>); 4] = [
        (
            "details: 3 declared, 2 there",
            0x02,
            0x29,
            vec![0x7E, 1, 2, 3, 4, 0x40, 0x03, 0x00, 1, 2],
        ),
        (
            "details: a non-data byte",
            0x02,
            0x29,
            vec![0x7E, 1, 2, 3, 4, 0x40, 0x01, 0x00, 0x80],
        ),
        (
            "specific data: 5 declared, 2 there",
            0x0F,
            0x2F,
            vec![0x7E, 1, 2, 3, 4, 5, 0, 0, 0, 1, 2],
        ),
        (
            "specific data: 2^28 - 1 declared",
            0x0F,
            0x2F,
            vec![0x7E, 1, 2, 3, 4, 0x7F, 0x7F, 0x7F, 0x7F],
        ),
    ];
    for (name, channel, sub, body) in cases {
        let mut bytes = preamble(channel, sub, to_dst());
        bytes.extend(body);
        assert_eq!(unread(&bytes).reason, UnreadReason::Malformed, "{name}");
    }
}

#[test]
fn data_that_cannot_be_carried_is_refused_at_construction() {
    assert_eq!(
        SpecificData::new(vec![0x80]),
        Err(ProfileDataError::NotSevenBit)
    );
    assert_eq!(
        DetailsData::new(vec![0; 0x4000]),
        Err(ProfileDataError::TooLong)
    );
    assert!(DetailsData::new(vec![0x7F; 0x3FFF]).is_ok());
}

#[test]
fn an_inquiry_or_a_set_to_the_broadcast_muid_is_for_nobody() {
    // Tables 17, 18, 22 to 25: one device. The broadcast is for the reports.
    for (sub, body) in [
        (0x20u8, vec![]),
        (0x21, vec![0, 0, 0, 0]),
        (0x22, vec![0x7E, 1, 2, 3, 4, 0, 0]),
        (0x23, vec![0x7E, 1, 2, 3, 4, 0, 0]),
        (0x28, vec![0x7E, 1, 2, 3, 4, 0]),
        (0x29, vec![0x7E, 1, 2, 3, 4, 0, 0, 0]),
    ] {
        let mut bytes = preamble(0x7F, sub, BROADCAST);
        bytes.extend(body);
        assert_eq!(
            unread(&bytes).reason,
            UnreadReason::WrongDestination,
            "{sub:#x}"
        );
    }
}

#[test]
fn bytes_after_the_last_field_are_ignored() {
    // §5.3, as for every body.
    let mut bytes = preamble(0x7F, 0x26, BROADCAST);
    bytes.extend([0x7E, 1, 2, 3, 4, 0x55, 0x66]);
    let CiMessage::ProfileAdded { profile, .. } = decoded(&bytes) else {
        panic!("an Added Report")
    };
    assert_eq!(profile, piano());
}

#[test]
fn a_malformed_profile_message_is_unread_and_nakked_0x41_on_its_address() {
    // A Set On with a Profile Id cut short.
    let mut bytes = preamble(0x05, 0x22, to_dst());
    bytes.extend([0x7E, 1, 2]);
    let unread = unread(&bytes);
    assert_eq!(unread.reason, UnreadReason::Malformed);
    let CiMessage::Nak { header, nak, .. } = unread.nak(Muid::of(DST)).unwrap() else {
        panic!("a NAK")
    };
    assert_eq!(header.device_id, channel(5));
    assert_eq!(nak.nak_sub_id2, U7::of(0x22));
    assert_eq!(nak.status, NakStatus::Malformed);
}

#[test]
fn a_nak_for_an_unsupported_profile_carries_status_04_and_the_profile_id() {
    // §5.11.3: for Profile Configuration the 5 detail bytes are the Profile Id.
    let nak = Nak::new(U7::of(0x22), NakStatus::ProfileNotSupported)
        .with_details(piano().to_bytes().map(U7::of));
    let message = CiMessage::Nak {
        header: header_on(channel(1)),
        to: Muid::of(DST),
        nak,
    };
    let bytes = message.encode();
    assert_eq!(&bytes[13..21], [0x22, 0x04, 0x00, 0x7E, 1, 2, 3, 4]);
    assert_eq!(decoded(&bytes), message);
}

#[test]
fn profile_channels_hold_14_bits_and_zero_is_by_address() {
    assert_eq!(
        ProfileChannels::new(0x3FFF),
        Some(ProfileChannels::of(0x3FFF))
    );
    assert_eq!(ProfileChannels::new(0x4000), None);
    assert!(ProfileChannels::BY_ADDRESS.is_by_address());
    assert!(!ProfileChannels::of(1).is_by_address());
}

#[test]
fn registered_targets_are_below_0x40() {
    assert!(InquiryTarget::new(U7::of(0x3F)).is_registered());
    assert!(!InquiryTarget::new(U7::of(0x40)).is_registered());
}

#[test]
fn the_reserved_profile_sub_ids_are_not_read() {
    // Appendix D: 0x2A to 0x2E are reserved.
    for sub in 0x2A..=0x2E {
        let bytes = preamble(0x7F, sub, to_dst());
        assert_eq!(unread(&bytes).reason, UnreadReason::UnsupportedMessage);
    }
}

fn any_profile() -> impl Strategy<Value = ProfileId> {
    prop_oneof![
        (0u8..0x80, 0u8..0x80, 0u8..0x80, 0u8..0x80)
            .prop_map(|(a, b, c, d)| ProfileId::standard(a, b, c, d)),
        (0u8..0x80, 0u8..0x80, 0u8..0x80, 0u8..0x80, 0u8..0x80).prop_filter_map(
            "a manufacturer id",
            |(a, b, c, d, e)| {
                let bytes = if a == 0 {
                    [0, b, c, d, e]
                } else {
                    [a, 0, 0, d, e]
                };
                ProfileId::from_bytes(bytes).filter(|id| matches!(id, ProfileId::Manufacturer(_)))
            }
        ),
    ]
}

fn any_address() -> impl Strategy<Value = CiDeviceId> {
    prop_oneof![
        (0u8..16).prop_map(|c| CiDeviceId::Channel(MidiChannel::of(c))),
        Just(CiDeviceId::Group),
        Just(CiDeviceId::FunctionBlock),
    ]
}

fn any_target() -> impl Strategy<Value = ProfileTarget> {
    prop_oneof![
        ((0u8..16), (0u16..0x4000)).prop_map(|(c, n)| ProfileTarget::Channel {
            channel: MidiChannel::of(c),
            channels: ProfileChannels::of(n),
        }),
        Just(ProfileTarget::Group),
        Just(ProfileTarget::FunctionBlock),
    ]
}

fn any_data() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..0x80, 0..40)
}

fn head() -> impl Strategy<Value = CiHeader> {
    any_address().prop_map(header_on)
}

fn to() -> impl Strategy<Value = Muid> {
    Just(Muid::of(DST))
}

fn any_message() -> impl Strategy<Value = CiMessage> {
    let lists = (
        proptest::collection::vec(any_profile(), 0..4),
        proptest::collection::vec(any_profile(), 0..4),
    )
        .prop_map(|(e, d)| {
            ProfileLists::new(
                ProfileList::try_from(e).unwrap(),
                ProfileList::try_from(d).unwrap(),
            )
        });
    prop_oneof![
        (head(), to()).prop_map(|(header, to)| CiMessage::ProfileInquiry { header, to }),
        (head(), to(), lists).prop_map(|(header, to, profiles)| {
            CiMessage::ProfileReply {
                header,
                to,
                profiles,
            }
        }),
        (any_target(), to(), any_profile()).prop_map(|(target, to, profile)| {
            CiMessage::SetProfileOn {
                source: Muid::of(SRC),
                ci_version: CI_VERSION,
                to,
                target,
                profile,
            }
        }),
        (head(), to(), any_profile()).prop_map(|(header, to, profile)| {
            CiMessage::SetProfileOff {
                header,
                to,
                profile,
            }
        }),
        (any_target(), any_profile()).prop_map(|(target, profile)| CiMessage::ProfileEnabled {
            source: Muid::of(SRC),
            ci_version: CI_VERSION,
            target,
            profile,
        }),
        (any_target(), any_profile()).prop_map(|(target, profile)| CiMessage::ProfileDisabled {
            source: Muid::of(SRC),
            ci_version: CI_VERSION,
            target,
            profile,
        }),
        (head(), any_profile())
            .prop_map(|(header, profile)| CiMessage::ProfileAdded { header, profile }),
        (head(), any_profile())
            .prop_map(|(header, profile)| CiMessage::ProfileRemoved { header, profile }),
        (head(), to(), any_profile(), 0u8..0x80).prop_map(|(header, to, profile, target)| {
            CiMessage::ProfileDetailsInquiry {
                header,
                to,
                profile,
                target: InquiryTarget::new(U7::of(target)),
            }
        }),
        (head(), to(), any_profile(), 0u8..0x80, any_data()).prop_map(
            |(header, to, profile, target, data)| {
                CiMessage::ProfileDetailsReply {
                    header,
                    to,
                    profile,
                    target: InquiryTarget::new(U7::of(target)),
                    data: DetailsData::new(data).unwrap(),
                }
            }
        ),
        (head(), any::<bool>(), any_profile(), any_data()).prop_map(
            |(header, broadcast, profile, data)| {
                CiMessage::ProfileSpecificData(
                    ProfileSpecific::new(
                        header,
                        if broadcast {
                            Destination::Broadcast
                        } else {
                            Destination::Muid(Muid::of(DST))
                        },
                        profile,
                        SpecificData::new(data).unwrap(),
                    )
                    .unwrap(),
                )
            }
        ),
    ]
}

proptest! {
    #[test]
    fn every_profile_message_decodes_to_what_was_encoded(message in any_message()) {
        let bytes = message.encode();
        prop_assert_eq!(CiMessage::decode(&bytes), Decoded::Message(message.clone()));
        // The destination the message reports is the one on the wire.
        let wire = &bytes[9..13];
        match message.destination() {
            Destination::Broadcast => prop_assert_eq!(wire, &BROADCAST),
            Destination::Muid(m) => prop_assert_eq!(wire, &m.to_bytes()),
        }
        prop_assert_eq!(bytes[3], message.sub_id2());
    }

    #[test]
    fn a_profile_id_round_trips_its_bytes(id in any_profile()) {
        prop_assert_eq!(ProfileId::from_bytes(id.to_bytes()), Some(id));
    }
}

fn set_profile_on_payload(device: u8, version: u8, source: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x7E, device, 0x0D, 0x22, version];
    bytes.extend(source);
    bytes.extend(BROADCAST);
    bytes.extend(body);
    bytes
}

/// M2-116 section 6.2 sets both MUIDs to Broadcast, and M2-101 section 7.8
/// (Table 24) gives the rest: `7E <device> 0D 22 <version>`, the two MUIDs, the
/// Profile Id and the two Number of Channels bytes, least significant first.
/// Spec: m2-116/6.2#3
#[test]
fn a_profile_on_request_is_table_24_with_broadcast_muids() {
    let on = ProfileOnRequest {
        target: ProfileTarget::Channel {
            channel: MidiChannel::of(2),
            channels: ProfileChannels::of(0x81),
        },
        profile: ProfileId::MPE,
    };
    assert_eq!(
        on.encode(),
        [
            0x7E, 0x02, 0x0D, 0x22, 0x02, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7E,
            0x31, 0x00, 0x01, 0x01, 0x01, 0x01,
        ]
    );
    let group = ProfileOnRequest {
        target: ProfileTarget::Group,
        profile: ProfileId::MPE,
    };
    assert_eq!(group.encode()[1], 0x7E);
    assert_eq!(group.encode()[18..], [0, 0]);
    assert_eq!(
        ProfileOnRequest {
            target: ProfileTarget::FunctionBlock,
            profile: piano()
        }
        .encode()[1],
        0x7F
    );
}

/// The request is the body `CiMessage::SetProfileOn` has, so the two are
/// one layout: the same message with real MUIDs differs only in them.
#[test]
fn a_profile_on_request_has_the_body_of_the_message_on_a_link() {
    let on = ProfileOnRequest {
        target: ProfileTarget::Channel {
            channel: MidiChannel::of(2),
            channels: ProfileChannels::of(3),
        },
        profile: piano(),
    };
    let message = CiMessage::SetProfileOn {
        source: Muid::of(0x0102_0304),
        ci_version: CI_VERSION,
        to: Muid::of(0x0506_0708),
        target: on.target,
        profile: on.profile,
    };
    assert_eq!(on.encode()[13..], message.encode()[13..]);
    assert_eq!(on.encode()[..5], message.encode()[..5]);
}

/// The packets are the SysEx7 cut of the payload (M2-104 section 7.7): 20 bytes
/// are a Start, two Continues and an End on the group asked for.
#[test]
fn a_profile_on_request_is_cut_into_sysex7_packets() {
    let on = ProfileOnRequest {
        target: ProfileTarget::Group,
        profile: ProfileId::MPE,
    };
    let packets: Vec<_> = on.packets(tutti_ump::MidiGroup::of(3)).collect();
    assert_eq!(packets.len(), 4);
    assert!(
        packets
            .iter()
            .all(|p| p.words()[0] >> 24 == 0x33 && p.words().len() == 2)
    );
}

/// Whatever the MUIDs, the message is a request for the Profile; a Version 1
/// message has no Number of Channels and asks for 0.
/// Spec: m2-116/6.2#2
#[test]
fn a_profile_on_request_is_read_with_any_muids_and_either_version() {
    let id = ProfileId::MPE.to_bytes();
    let v2 = [id.as_slice(), &[5, 0]].concat();
    let real = ProfileOnRequest::try_from(&set_profile_on_payload(0x01, 2, [1, 2, 3, 4], &v2)[..]);
    assert_eq!(
        real,
        Ok(ProfileOnRequest {
            target: ProfileTarget::Channel {
                channel: MidiChannel::of(1),
                channels: ProfileChannels::of(5)
            },
            profile: ProfileId::MPE
        })
    );
    // A Source of Broadcast too: a link would refuse it.
    let v1 = ProfileOnRequest::try_from(&set_profile_on_payload(0x7F, 1, BROADCAST, &id)[..]);
    assert_eq!(
        v1,
        Ok(ProfileOnRequest {
            target: ProfileTarget::FunctionBlock,
            profile: ProfileId::MPE
        })
    );
}

/// Each fault the bytes can have is its own error.
#[test]
fn a_profile_on_request_names_what_is_wrong_with_the_bytes() {
    let id = ProfileId::MPE.to_bytes();
    let body = [id.as_slice(), &[0, 0]].concat();
    let decode = |device, version, body: &[u8]| {
        ProfileOnRequest::try_from(&set_profile_on_payload(device, version, BROADCAST, body)[..])
    };
    assert_eq!(
        decode(0x7E, 2, &body),
        Ok(ProfileOnRequest {
            target: ProfileTarget::Group,
            profile: ProfileId::MPE
        })
    );
    assert_eq!(decode(0x7E, 0, &body), Err(NotSetProfileOn::Version(0)));
    assert_eq!(
        decode(0x7E, 0x22, &body),
        Err(NotSetProfileOn::Version(0x22))
    );
    assert_eq!(decode(0x20, 2, &body), Err(NotSetProfileOn::DeviceId(0x20)));
    assert_eq!(decode(0x7E, 2, &body[..4]), Err(NotSetProfileOn::Body));
    // A Number of Channels byte with its top bit set.
    let wide = [id.as_slice(), &[0x80, 0]].concat();
    assert_eq!(decode(0x00, 2, &wide), Err(NotSetProfileOn::Body));
    // §7.8's note: a Group or Function Block address asks for 0 Channels.
    let some = [id.as_slice(), &[3, 0]].concat();
    assert_eq!(
        decode(0x7E, 2, &some),
        Err(NotSetProfileOn::ChannelsForBlock(3))
    );
    assert_eq!(
        decode(0x7F, 2, &some),
        Err(NotSetProfileOn::ChannelsForBlock(3))
    );
    assert!(decode(0x05, 2, &some).is_ok());
    // Not a Set Profile On: another sub-ID#2, or no preamble.
    let mut other = set_profile_on_payload(0x7E, 2, BROADCAST, &body);
    other[3] = 0x23;
    assert_eq!(
        ProfileOnRequest::try_from(&other[..]),
        Err(NotSetProfileOn::NotThatMessage)
    );
    assert_eq!(
        ProfileOnRequest::try_from(&[0x7E, 0x7E][..]),
        Err(NotSetProfileOn::NotThatMessage)
    );
}

/// A link refuses what a request carries: a Source of Broadcast is not a
/// device that can own a MUID (M2-101 section 3.3.3).
#[test]
fn a_link_still_refuses_a_set_profile_on_from_the_broadcast_muid() {
    let bytes = ProfileOnRequest {
        target: ProfileTarget::Group,
        profile: ProfileId::MPE,
    }
    .encode();
    let Decoded::Unread(unread) = CiMessage::decode(&bytes) else {
        panic!("a Source of Broadcast is not a message on a link");
    };
    assert_eq!(unread.reason, UnreadReason::BadSource);
}
