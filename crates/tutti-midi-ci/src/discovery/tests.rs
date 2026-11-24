use super::super::testing::{DST, decoded, header};
use super::super::*;
use super::*;
use tutti_ump::ManufacturerId;
use tutti_ump::SoftwareRevision;

fn is_malformed(body: &[u8], is_reply: bool) -> bool {
    if is_reply {
        ReplyData::decode_body(body).is_none()
    } else {
        DiscoveryData::decode_body(body).is_none()
    }
}

fn sample_body() -> DeviceBody {
    DeviceBody {
        identity: DeviceIdentity {
            manufacturer: ManufacturerId::of(0xA109),
            family: [0x34, 0x24].map(U7::of),
            model: [0x55, 0x00].map(U7::of),
            revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
        },
        categories: CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
        max_sysex_size: MaxSysexSize::of(512),
    }
}

fn sample_discovery() -> DiscoveryData {
    DiscoveryData {
        body: sample_body(),
        output_path_id: Some(U7::of(0x05)),
    }
}

fn sample_reply() -> ReplyData {
    ReplyData {
        body: sample_body(),
        version_2: Some(ReplyVersion2 {
            output_path_id: U7::of(0x05),
            function_block: FunctionBlockId::new(3),
        }),
    }
}

#[test]
fn discovery_round_trips() {
    let msg = CiMessage::Discovery {
        header: header(),
        data: sample_discovery(),
    };
    let bytes = msg.encode();
    assert_eq!(bytes[3], SUB_ID2_DISCOVERY);
    assert_eq!(bytes.len(), 13 + 17);
    assert_eq!(decoded(&bytes), msg);
}

// M2-101 Appendix D: the Special ID 0x7C is a one-byte Manufacturer ID that
// Discovery carries as it carries any other.
/// Spec: m2-101/d#1
#[test]
fn discovery_carries_the_special_manufacturer_id_7c() {
    let msg = CiMessage::Discovery {
        header: header(),
        data: DiscoveryData {
            body: DeviceBody {
                identity: DeviceIdentity {
                    manufacturer: ManufacturerId::MIDI2_NON_COMMERCIAL,
                    ..sample_body().identity
                },
                ..sample_body()
            },
            ..sample_discovery()
        },
    };
    let bytes = msg.encode();
    // The Manufacturer's three id bytes follow the preamble (13 bytes).
    assert_eq!(bytes[13..16], [0x7C, 0x00, 0x00]);
    assert_eq!(decoded(&bytes), msg);
}

#[test]
fn discovery_and_reply_have_the_version_2_trailers() {
    // Table 6 appends 1 byte Output Path ID; Table 8 appends Output Path
    // Instance ID *and* Function Block. The two directions differ by exactly
    // one byte.
    let d = sample_discovery();
    let r = sample_reply();

    let mut discovery = Vec::new();
    d.encode_body(&mut discovery);
    assert_eq!(discovery.len(), 17);
    assert_eq!(
        discovery[16], 0x05,
        "Output Path ID is the last Discovery byte"
    );

    let mut reply = Vec::new();
    r.encode_body(&mut reply);
    assert_eq!(reply.len(), 18);
    assert_eq!(reply[16], 0x05, "path id echoed");
    assert_eq!(reply[16 + 1], 0x03, "Function Block follows it");

    // Both directions decode back to what they encoded.
    assert_eq!(ReplyData::decode_body(&reply), Some(r));
    assert_eq!(DiscoveryData::decode_body(&discovery), Some(d));
}

// M2-101 Tables 6 and 8 mark the Output Path ID and the Function Block as added
// in Message Format Version 2, and §5.3, §5.4 have a receiver read a lower
// version with the fields of that version: a 1.1 body ends after the Receivable
// Maximum SysEx Message Size (16 bytes), and the fields it lacks are absent, not
// path 0 and not block 0.
/// Spec: m2-101/5.3#6, m2-101/5.4#7
#[test]
fn a_version_1_body_has_no_output_path_and_no_function_block() {
    let mut body = Vec::new();
    ReplyData {
        version_2: None,
        ..sample_reply()
    }
    .encode_body(&mut body);
    assert_eq!(body.len(), 16);
    let reply = ReplyData::decode_body(&body).expect("v1 reply decodes");
    assert_eq!(reply.body, sample_body());
    assert_eq!(reply.version_2, None);
    assert_eq!(reply.function_block(), None);
    let discovery = DiscoveryData::decode_body(&body).expect("v1 discovery decodes");
    assert_eq!(discovery.body, sample_body());
    assert_eq!(discovery.output_path_id, None);
    let mut again = Vec::new();
    discovery.encode_body(&mut again);
    assert_eq!(again, body);
}

// A Version 2 Reply holds its path id and Function Block together, so a Reply
// with a Function Block round-trips.
#[test]
fn a_version_2_reply_round_trips_its_path_id_and_function_block() {
    let mut body = Vec::new();
    sample_reply().encode_body(&mut body);
    assert_eq!(body.len(), 18);
    assert_eq!(ReplyData::decode_body(&body), Some(sample_reply()));
}

/// Spec: m2-101/5.6.1#1
#[test]
fn a_reply_answering_a_discovery_echoes_its_path_id_and_a_version_1_one_is_path_0() {
    let reply = ReplyData::answering(&sample_discovery(), sample_body());
    assert_eq!(
        reply.version_2,
        Some(ReplyVersion2 {
            output_path_id: U7::of(0x05),
            function_block: None
        })
    );
    let version_1 = DiscoveryData {
        output_path_id: None,
        ..sample_discovery()
    };
    assert_eq!(
        ReplyData::answering(&version_1, sample_body())
            .version_2
            .map(|v2| v2.output_path_id),
        Some(U7::of(0))
    );
}

// The Reply of a device that sends Message Format Version 1 is a whole message
// that decodes, and the path 0 the Version 2 field names is not what it holds.
/// Spec: m2-101/5.3#6, m2-101/5.4#7
#[test]
fn a_version_1_discovery_reply_message_is_read() {
    let mut bytes = CiMessage::DiscoveryReply {
        header: CiHeader {
            ci_version: CiVersion::of(0x01),
            ..header()
        },
        to: Muid::of(DST),
        data: sample_reply(),
    }
    .encode();
    bytes.truncate(13 + 16);
    match decoded(&bytes) {
        CiMessage::DiscoveryReply { header, to, data } => {
            assert_eq!(header.ci_version, CiVersion::of(0x01));
            assert_eq!(to, Muid::of(DST));
            assert_eq!(data.version_2, None);
            assert_eq!(data.body, sample_body());
        }
        other => panic!("expected a Discovery Reply, got {other:?}"),
    }
}

// A body shorter than Version 1's has no Receivable Maximum SysEx Message Size
// or categories, which no version omits: it is malformed.
#[test]
fn a_reply_shorter_than_version_1_is_malformed() {
    let mut body = Vec::new();
    sample_reply().encode_body(&mut body);
    for len in [0, 11, 12, 15] {
        assert!(is_malformed(&body[..len], true), "{len} bytes");
        assert!(is_malformed(&body[..len], false), "{len} bytes");
    }
    assert!(!is_malformed(&body[..16], true));
    // The Version 2 trailer of a Reply is its path id and Function Block, both.
    assert!(is_malformed(&body[..17], true));
    assert!(!is_malformed(&body[..17], false));
    let mut bytes = CiMessage::DiscoveryReply {
        header: header(),
        to: Muid::of(DST),
        data: sample_reply(),
    }
    .encode();
    bytes.truncate(13 + 15);
    assert!(matches!(
        CiMessage::decode(&bytes),
        Decoded::Unread(Unread {
            reason: UnreadReason::Malformed,
            ..
        })
    ));
}

#[test]
fn a_discovery_reply_round_trips() {
    let msg = CiMessage::DiscoveryReply {
        header: header(),
        to: Muid::of(DST),
        data: sample_reply(),
    };
    let bytes = msg.encode();
    assert_eq!(bytes[3], SUB_ID2_DISCOVERY_REPLY);
    match decoded(&bytes) {
        CiMessage::DiscoveryReply { to, data, .. } => {
            assert_eq!(to, Muid::of(DST));
            assert_eq!(data, sample_reply());
        }
        other => panic!("expected Discovery reply, got {other:?}"),
    }
}

#[test]
fn categories_survive_round_trip() {
    let mut body = Vec::new();
    sample_reply().encode_body(&mut body);
    let back = ReplyData::decode_body(&body).expect("decodes").body;
    assert!(
        back.categories
            .contains(CiCategories::PROFILE_CONFIGURATION)
    );
    assert!(back.categories.contains(CiCategories::PROPERTY_EXCHANGE));
    assert!(!back.categories.contains(CiCategories::PROCESS_INQUIRY));
}

// Table 7: D2 Profile Configuration, D3 Property Exchange, D4 Process Inquiry;
// D1, Protocol Negotiation, is deprecated, so a 1.1 peer's D1 is not read.
#[test]
fn the_category_bits_are_table_7s_and_a_peers_deprecated_d1_is_dropped() {
    assert_eq!(CiCategories::PROFILE_CONFIGURATION.bits(), 0x04);
    assert_eq!(CiCategories::PROPERTY_EXCHANGE.bits(), 0x08);
    assert_eq!(CiCategories::PROCESS_INQUIRY.bits(), 0x10);
    let mut body = Vec::new();
    sample_reply().encode_body(&mut body);
    body[11] = 0x02 | 0x04;
    let back = ReplyData::decode_body(&body).expect("decodes").body;
    assert_eq!(back.categories, CiCategories::PROFILE_CONFIGURATION);
}

// §5.5.3: the Receivable Maximum SysEx Message Size is 128 or greater.
/// Spec: m2-101/5.5.3#1
#[test]
fn a_receivable_size_under_128_is_malformed() {
    let mut body = Vec::new();
    sample_discovery().encode_body(&mut body);
    let size = |body: &mut [u8], v: u32| {
        body[12..16].copy_from_slice(&[
            (v & 0x7F) as u8,
            ((v >> 7) & 0x7F) as u8,
            ((v >> 14) & 0x7F) as u8,
            ((v >> 21) & 0x7F) as u8,
        ]);
    };
    size(&mut body, 128);
    assert!(!is_malformed(&body, false));
    size(&mut body, 127);
    assert!(is_malformed(&body, false));
    size(&mut body, 0);
    assert!(is_malformed(&body, false));
}

// §5.6.2: the Function Block is the number of the Function Block, 0 to 31, or
// 0x7F for none; the numbers between are not a Function Block.
/// Spec: m2-101/5.6.2#1
#[test]
fn the_function_block_of_a_reply_is_0_to_31_or_none() {
    let mut body = Vec::new();
    sample_reply().encode_body(&mut body);
    let at = 16 + 1;
    for (raw, expected) in [(0, Some(0)), (31, Some(31)), (0x7F, None)] {
        body[at] = raw;
        let got = ReplyData::decode_body(&body).unwrap();
        assert_eq!(
            got.function_block().map(FunctionBlockId::get),
            expected,
            "{raw}"
        );
    }
    for raw in [32, 0x40, 0x7E] {
        body[at] = raw;
        assert!(is_malformed(&body, true), "{raw}");
    }
    // A Discovery has no such field: the byte is not read as one.
    body[at] = 0x40;
    assert!(!is_malformed(&body[..at], false));
}

#[test]
fn a_byte_with_its_top_bit_set_is_malformed() {
    let mut body = Vec::new();
    sample_reply().encode_body(&mut body);
    for i in [0, 3, 8, 11, 16] {
        let mut bad = body.clone();
        bad[i] |= 0x80;
        assert!(is_malformed(&bad, true), "byte {i}");
    }
}

#[test]
fn the_receivable_size_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    let c = RangeContract {
        new: MaxSysexSize::new,
        get: MaxSysexSize::get,
        of: Some(MaxSysexSize::of),
        valid: &[128, 512, 0x0FFF_FFFF],
        invalid: &[0, 127, 0x1000_0000, u32::MAX],
    };
    check_range_contract(&c);
}

// M2-101 Table 6: after the 13-byte preamble come the manufacturer's three
// bytes, the family and the model (each LSB first), the four software
// revision bytes, the category bitmap and the four size bytes. The identity
// is `DeviceIdentity`'s arrays in that order.
#[test]
fn the_identity_is_written_in_the_tables_order() {
    let mut body = Vec::new();
    sample_discovery().encode_body(&mut body);
    assert_eq!(
        &body[..11],
        [0x00, 0x21, 0x09, 0x34, 0x24, 0x55, 0x00, 1, 2, 3, 4]
    );
}

/// M2-101 §5.5.3: the size is four 7-bit bytes, least significant first.
#[test]
fn a_max_sysex_size_is_four_seven_bit_bytes_least_significant_first() {
    assert_eq!(MaxSysexSize::of(128).to_le7(), [0x00, 0x01, 0x00, 0x00]);
    assert_eq!(
        MaxSysexSize::of(0x0123_4567).to_le7(),
        [0x67, 0x0A, 0x0D, 0x09]
    );
    assert_eq!(MaxSysexSize::of(MaxSysexSize::MAX).to_le7(), [0x7F; 4]);
}

// M2-101 §5.3, §5.4: a message is written in one Message Format Version, and the
// fields of Discovery and its Reply are that version's: the Version 1 byte
// goes with a body that ends after the receive buffer size, and no other version
// does.
/// Spec: m2-101/5.4#8, m2-101/5.4#9
#[test]
fn a_written_discovery_has_the_body_its_version_names() {
    let v1 = |bytes: &[u8]| (bytes[4] == 1, bytes.len() == 13 + 16, bytes.len() == 13 + 17);
    for version in [1, 2] {
        let header = CiHeader {
            ci_version: CiVersion::of(version),
            ..header()
        };
        let mut v1_discovery = sample_discovery();
        v1_discovery.output_path_id = None;
        for data in [sample_discovery(), v1_discovery] {
            let bytes = CiMessage::Discovery { header, data }.encode();
            let (names_v1, short, _) = v1(&bytes);
            assert_eq!(names_v1, short, "Discovery: header {version}, {data:?}");
        }
        let mut v1_reply = sample_reply();
        v1_reply.version_2 = None;
        for data in [sample_reply(), v1_reply] {
            let bytes = CiMessage::DiscoveryReply {
                header,
                to: Muid::of(DST),
                data,
            }
            .encode();
            let (names_v1, _, _) = v1(&bytes);
            assert_eq!(
                names_v1,
                bytes.len() == 13 + 16,
                "Reply: header {version}, {data:?}"
            );
        }
    }
}
