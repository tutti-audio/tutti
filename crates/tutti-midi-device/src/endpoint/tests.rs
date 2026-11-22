use tutti_ump::ManufacturerId;
use tutti_ump::{
    BlockDirection, BlockFilter, BlockSelect, CiVersion, DeviceIdentity, DiscoveryFilter,
    FirstGroup, FunctionBlockId, FunctionBlockInfo, GroupSpan, Message, MessageType, Midi1Port,
    OsFunctionBlock, Protocol, SoftwareRevision, Stream, StreamConfig, U7, UiHint, Ump,
    read_function_block,
};

use super::*;
use tutti_fixtures::midi;
use tutti_ump::{BlockName, EndpointName, ProductInstanceId};

const ALL: DiscoveryFilter = DiscoveryFilter {
    endpoint_info: true,
    device_identity: true,
    endpoint_name: true,
    product_instance_id: true,
    stream_configuration: true,
};
const NONE: DiscoveryFilter = DiscoveryFilter {
    endpoint_info: false,
    device_identity: false,
    endpoint_name: false,
    product_instance_id: false,
    stream_configuration: false,
};

fn id(n: u8) -> FunctionBlockId {
    FunctionBlockId::new(n).unwrap()
}

fn block(n: u8, first: u8, span: u8) -> FunctionBlockBuilder {
    FunctionBlock::builder(
        id(n),
        BlockDirection::Bidirectional,
        FirstGroup::new(first).unwrap(),
        GroupSpan::new(span).unwrap(),
    )
}

fn identity() -> DeviceIdentity {
    let u = |n| U7::new(n).unwrap();
    DeviceIdentity {
        manufacturer: ManufacturerId::of(0xA109),
        family: [u(1), u(2)],
        model: [u(3), u(4)],
        revision: SoftwareRevision::from([u(5), u(6), u(7), u(8)]),
    }
}

const FULL: Capabilities = Capabilities {
    midi1: true,
    midi2: true,
    rx_jr: true,
    tx_jr: true,
};

/// A device with a name, an id, an identity and two blocks, the second a
/// MIDI-CI 1.2 block (message version 2) with two SysEx 8 streams.
fn device() -> Endpoint {
    Endpoint::new(Supports::of(FULL))
        .identified(identity())
        .named(EndpointName::new("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123").unwrap())
        .instanced(ProductInstanceId::new("SN-0001").unwrap())
        .static_blocks(true)
        .block(
            block(0, 0, 1)
                .name(BlockName::new("Keys").unwrap())
                .build()
                .unwrap(),
        )
        .block(
            block(3, 4, 2)
                .ui_hint(UiHint::Both)
                .midi_ci(Some(CiVersion::of(2)), Sysex8StreamCount::new(2))
                .name(BlockName::new("Piano").unwrap())
                .build()
                .unwrap(),
        )
}

fn ask(endpoint: &mut Endpoint, stream: Stream) -> Vec<Ump> {
    let mut out = Vec::new();
    let _ = endpoint.reply(Message::Stream(stream), &mut out);
    out
}

fn discovery(filter: DiscoveryFilter) -> Stream {
    Stream::EndpointDiscovery {
        version: UMP_VERSION,
        filter,
    }
}

fn status(u: &Ump) -> u32 {
    assert_eq!(u.message_type(), MessageType::Stream);
    (u.0[0] >> 16) & 0x3FF
}

fn words(u: &Ump) -> [u32; 4] {
    u.0
}

fn block_discovery(block: BlockSelect, info: bool, name: bool) -> Stream {
    Stream::FunctionBlockDiscovery {
        block,
        filter: BlockFilter { info, name },
    }
}

// M2-104 §7.1.2: version 1.1, static, NFB = highest block number + 1 = 4,
// M2 and M1, RXJR and TXJR. Word 1: 1 << 31 | 4 << 24 | 1 << 9 | 1 << 8 | 2 | 1.
#[test]
fn endpoint_info_words() {
    let out = ask(
        &mut device(),
        discovery(DiscoveryFilter {
            endpoint_info: true,
            ..NONE
        }),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(words(&out[0]), [0xF001_0101, 0x8400_0303, 0, 0]);
}

#[test]
fn a_full_discovery_is_answered_in_the_order_of_the_spec() {
    let out = ask(&mut device(), discovery(ALL));
    let order: Vec<u32> = out.iter().map(status).collect();
    // Info, Identity, Name x3 packets, Product Instance Id, Stream Configuration.
    assert_eq!(order, [0x01, 0x02, 0x03, 0x03, 0x03, 0x04, 0x06]);
}

// §7.1.1: each bit set in the filter yields its own reply.
/// Spec: m2-104/7.1.2#1, m2-104/7.1.3#1, m2-104/7.1.4#1, m2-104/7.1.5#3
#[test]
fn each_filter_bit_asks_for_its_own_reply() {
    let rows = [
        (
            DiscoveryFilter {
                endpoint_info: true,
                ..NONE
            },
            vec![0x01],
        ),
        (
            DiscoveryFilter {
                device_identity: true,
                ..NONE
            },
            vec![0x02],
        ),
        (
            DiscoveryFilter {
                endpoint_name: true,
                ..NONE
            },
            vec![0x03, 0x03, 0x03],
        ),
        (
            DiscoveryFilter {
                product_instance_id: true,
                ..NONE
            },
            vec![0x04],
        ),
        (
            DiscoveryFilter {
                stream_configuration: true,
                ..NONE
            },
            vec![0x06],
        ),
        (NONE, vec![]),
    ];
    for (filter, expect) in rows {
        let out = ask(&mut device(), discovery(filter));
        assert_eq!(
            out.iter().map(status).collect::<Vec<_>>(),
            expect,
            "{filter:?}"
        );
    }
}

// A part the endpoint does not have is not answered (§7.1.3 to §7.1.5 make
// the reply optional).
#[test]
fn a_part_an_endpoint_lacks_is_left_out() {
    let mut bare = Endpoint::new(Supports::of(FULL));
    let out = ask(&mut bare, discovery(ALL));
    assert_eq!(out.iter().map(status).collect::<Vec<_>>(), [0x01, 0x06]);
}

// §7.1.1: a higher version's appended fields are ignored and the reply is the
// same.
/// Spec: m2-104/7.1.1#3
#[test]
fn a_discovery_of_another_version_is_answered() {
    let newer = Stream::EndpointDiscovery {
        version: tutti_ump::UmpVersion { major: 1, minor: 9 },
        filter: ALL,
    };
    assert_eq!(ask(&mut device(), newer).len(), 7);
}

// §7.1.1: "If the received version is lower than the Device's supported version,
// the Device shall only process the fields, values, and bits defined in the
// received version". Table 5 defines UMP 1.0 with no Stream messages (N/A), so no
// filter bit is defined in 1.0 or below, and the discovery gets no answer; 1.1,
// the first version that defines the message, is answered in full.
/// Spec: m2-104/7.1.1#4
#[test]
fn a_discovery_of_a_lower_version_is_read_as_that_version_defines_it() {
    for (major, minor) in [(1, 0), (0, 255), (0, 0)] {
        let older = Stream::EndpointDiscovery {
            version: tutti_ump::UmpVersion { major, minor },
            filter: ALL,
        };
        let mut out = Vec::new();
        let reply = device().reply(Message::Stream(older), &mut out);
        assert_eq!(
            (reply, out.len()),
            (EndpointReply::Ignored, 0),
            "version {major}.{minor}"
        );
    }
    assert_eq!(ask(&mut device(), discovery(ALL)).len(), 7);
}

// Figure 14, §7.1.3: word 1 holds the manufacturer's three bytes, word 2 the
// family and model, word 3 the revision. §7.1.4: a 30 byte name is a Start (14),
// a Continue (14) and an End (2 then zeros). §7.1.5: "SN-0001" is Complete.
/// Spec: m2-104/7.1.4#2, m2-104/7.1.5#7
#[test]
fn identity_name_and_instance_id_words() {
    let out = ask(&mut device(), discovery(ALL));
    let w: Vec<[u32; 4]> = out.iter().map(words).collect();
    assert_eq!(w[1], [0xF002_0000, 0x0000_2109, 0x0102_0304, 0x0506_0708]);
    assert_eq!(w[2], [0xF403_4142, 0x4344_4546, 0x4748_494A, 0x4B4C_4D4E]);
    assert_eq!(w[3], [0xF803_4F50, 0x5152_5354, 0x5556_5758, 0x595A_3031]);
    assert_eq!(w[4], [0xFC03_3233, 0, 0, 0]);
    assert_eq!(w[5], [0xF004_534E, 0x2D30_3030, 0x3100_0000, 0]);
    // §7.1.6.3, Figure 19: MIDI 1.0 Protocol, JR off: 0x01 << 8.
    assert_eq!(w[6], [0xF006_0100, 0, 0, 0]);
}

// §7.1.8 Figure 22: active bit 15, block number 14-8, UI hint 5-4, MIDI 1.0
// 3-2, direction 1-0; word 1: first group, groups spanned, MIDI-CI version, SysEx 8
// streams. Block 3: 0x8000 | 0x0300 | 0x30 | 0 | 0b11 = 0x8333, then 4, 2, 2, 2.
// §7.1.9 Figure 23: block number in bits 15-8 then thirteen name bytes.
#[test]
fn function_block_info_and_name_words() {
    let out = ask(
        &mut device(),
        block_discovery(BlockSelect::One(id(3)), true, true),
    );
    assert_eq!(out.len(), 2);
    assert_eq!(words(&out[0]), [0xF011_8333, 0x0402_0202, 0, 0]);
    assert_eq!(words(&out[1]), [0xF012_0350, 0x6961_6E6F, 0, 0]);

    // The other way round: MIDI-CI version 1, 16 streams, block 7, Groups 5 to 7,
    // so a swapped pair of bytes shows. 0x8000 | 0x0700 | 0b11, then 5, 3, 1, 16.
    let mut e = Endpoint::new(Supports::of(FULL)).block(
        block(7, 5, 3)
            .midi_ci(Some(CiVersion::of(1)), Sysex8StreamCount::new(16))
            .build()
            .unwrap(),
    );
    let out = ask(
        &mut e,
        block_discovery(BlockSelect::One(id(7)), true, false),
    );
    assert_eq!(words(&out[0]), [0xF011_8703, 0x0503_0110, 0, 0]);
}

// A 30 byte block name: 13 bytes a packet, so Start, Continue, End with 4.
/// Spec: m2-104/7.1.9#4, m2-104/7.1.9#5, m2-104/7.1.9#6, m2-104/7.1.9#7
#[test]
fn a_long_block_name_splits_at_thirteen_bytes() {
    let name = BlockName::new("0123456789abcdefghijklmnopqrst").unwrap();
    let mut e = Endpoint::new(Supports::of(FULL)).block(block(1, 0, 1).name(name).build().unwrap());
    let out = ask(
        &mut e,
        block_discovery(BlockSelect::One(id(1)), false, true),
    );
    assert_eq!(
        out.iter().map(words).collect::<Vec<_>>(),
        [
            [0xF412_0130, 0x3132_3334, 0x3536_3738, 0x3961_6263],
            [0xF812_0164, 0x6566_6768, 0x696A_6B6C, 0x6D6E_6F70],
            [0xFC12_0171, 0x7273_7400, 0, 0],
        ]
    );
}

// §7.1.7: 0xFF is every block, a number one block.
/// Spec: m2-104/6.1#1, m2-104/7.1.8#2, m2-104/7.1.9#2, m2-104/1.4#1
#[test]
fn function_block_discovery_answers_one_block_or_all() {
    let all = ask(
        &mut device(),
        block_discovery(BlockSelect::All, true, false),
    );
    assert_eq!(all.iter().map(status).collect::<Vec<_>>(), [0x11, 0x11]);
    assert_eq!((all[0].0[0] >> 8) & 0x7F, 0);
    assert_eq!((all[1].0[0] >> 8) & 0x7F, 3);
    let one = ask(
        &mut device(),
        block_discovery(BlockSelect::One(id(0)), true, false),
    );
    assert_eq!(one.len(), 1);
    assert_eq!((one[0].0[0] >> 8) & 0x7F, 0);
    let three = ask(
        &mut device(),
        block_discovery(BlockSelect::One(id(3)), true, false),
    );
    assert_eq!(three.len(), 1);
    assert_eq!((three[0].0[0] >> 8) & 0x7F, 3);
}

// The info and name filter bits are separate bits, and for 0xFF all infos come before the names.
/// Spec: m2-104/7.1.8#1, m2-104/7.1.9#1
#[test]
fn function_block_filter_bits_pick_info_and_name() {
    let both = ask(&mut device(), block_discovery(BlockSelect::All, true, true));
    assert_eq!(
        both.iter().map(status).collect::<Vec<_>>(),
        [0x11, 0x11, 0x12, 0x12]
    );
    let name = ask(
        &mut device(),
        block_discovery(BlockSelect::All, false, true),
    );
    assert_eq!(name.iter().map(status).collect::<Vec<_>>(), [0x12, 0x12]);
    assert!(
        ask(
            &mut device(),
            block_discovery(BlockSelect::All, false, false)
        )
        .is_empty()
    );
}

// The spec asks for 0 to 31 (§7.1.7) and names no reply for the rest: a number
// above 31, which decode keeps, and a block that is not there, answer nothing.
#[test]
fn an_out_of_range_or_absent_block_answers_nothing() {
    let wire = Ump([0xF010_2003, 0, 0, 0]);
    let Message::Stream(stream) = wire.decode().unwrap() else {
        panic!("a Stream message")
    };
    assert!(ask(&mut device(), stream).is_empty());
    assert!(
        ask(
            &mut device(),
            block_discovery(BlockSelect::One(id(31)), true, true)
        )
        .is_empty()
    );
    assert!(
        ask(
            &mut device(),
            block_discovery(BlockSelect::One(id(1)), true, true)
        )
        .is_empty()
    );
}

fn request(protocol: Protocol, rx_jr: bool, tx_jr: bool) -> Stream {
    Stream::StreamConfigurationRequest(StreamConfig {
        protocol,
        rx_jr,
        tx_jr,
    })
}

fn notified(out: &[Ump]) -> StreamConfig {
    assert_eq!(out.len(), 1);
    match out[0].decode().unwrap() {
        Message::Stream(Stream::StreamConfigurationNotification(c)) => c,
        other => panic!("{other:?}"),
    }
}

// §7.1.6.2 and §7.1.6.3: the reply reports what is now active, and the new
// configuration is kept.
/// Spec: m2-104/7.1.6.3#1
#[test]
fn a_supported_request_is_honoured_and_reported() {
    let mut e = device();
    let out = ask(&mut e, request(Protocol::Midi2, true, true));
    assert_eq!(words(&out[0]), [0xF006_0203, 0, 0, 0]);
    assert_eq!(e.active().protocol, Protocol::Midi2);
    let again = ask(
        &mut e,
        discovery(DiscoveryFilter {
            stream_configuration: true,
            ..NONE
        }),
    );
    assert_eq!(words(&again[0]), [0xF006_0203, 0, 0, 0]);
    // JR off is always possible.
    let out = ask(&mut e, request(Protocol::Midi2, false, false));
    assert_eq!(words(&out[0]), [0xF006_0200, 0, 0, 0]);
}

// §7.1.6.1 step 3: a receiver that cannot apply part of a request answers
// with the configuration it has.
/// Spec: m2-104/7.1.6.3#1
#[test]
fn what_an_endpoint_cannot_do_is_not_done() {
    let mut midi1_only = Endpoint::midi1_device(None);
    let c = notified(&ask(&mut midi1_only, request(Protocol::Midi2, true, true)));
    assert_eq!(
        c,
        StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false
        }
    );
    // Some parts: MIDI 2.0 and receive are possible, transmit is not.
    let mut e = Endpoint::new(Supports::of(Capabilities {
        midi2: true,
        rx_jr: true,
        ..Capabilities::default()
    }));
    let c = notified(&ask(&mut e, request(Protocol::Midi2, true, true)));
    assert_eq!(
        c,
        StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: false
        }
    );
    // A reserved Protocol changes nothing.
    let c = notified(&ask(
        &mut e,
        request(midi::reserved_protocol(7), false, false),
    ));
    assert_eq!(c.protocol, Protocol::Midi2);
}

#[test]
fn a_message_that_is_not_a_request_is_not_answered() {
    let mut e = device();
    let mut out: Vec<Ump> = Vec::new();
    let noop = e.reply(Message::Utility(tutti_ump::Utility::Noop), &mut out);
    let notification = e.reply(
        Message::Stream(Stream::StreamConfigurationNotification(e.active())),
        &mut out,
    );
    assert_eq!(
        (noop, notification),
        (EndpointReply::Ignored, EndpointReply::Ignored)
    );
    assert!(out.is_empty());
}

/// Spec: m2-104/6.2#2, m2-104/6.2.1.1#1, m2-104/6.2#1
#[test]
fn a_midi1_device_has_one_inferred_block() {
    let e = Endpoint::midi1_device(EndpointName::new("Keystep"));
    let blocks: Vec<&FunctionBlock> = e.function_blocks().collect();
    assert_eq!(blocks.len(), 1);
    let b = blocks[0];
    assert_eq!(
        (b.id().get(), b.first_group().get(), b.groups().get()),
        (0, 0, 1)
    );
    assert!(b.active());
    assert_eq!(b.direction(), BlockDirection::Bidirectional);
    assert_eq!(b.midi1(), Midi1Port::Unrestricted);
    assert_eq!(e.declared_function_blocks(), 1);
    assert!(e.static_function_blocks());
    assert_eq!(e.active().protocol, Protocol::Midi1);
    assert_eq!(e.name().unwrap().as_str(), "Keystep");
    assert_eq!(
        e.capabilities(),
        Capabilities {
            midi1: true,
            ..Capabilities::default()
        }
    );
}

// The range checks `FunctionBlockBuilder::build` adds to the field types: a
// block stays inside 16 Groups (§7.1.8), a MIDI 1.0 port is one Group, the
// direction and the port are not the reserved values, and each refusal says
// which. They span several fields, not one raw's range, so they do not fit
// `check_range_contract`; the field types' own ranges are `tutti-ump`'s tests.
/// Spec: m2-104/6.1.1#1, m2-104/7.1.8#6
#[test]
fn a_function_block_stays_inside_the_groups() {
    let new = |first, span, dir| {
        FunctionBlock::builder(
            id(0),
            dir,
            FirstGroup::new(first).unwrap(),
            GroupSpan::new(span).unwrap(),
        )
    };
    let bi = BlockDirection::Bidirectional;
    assert!(new(0, 16, bi).build().is_ok());
    assert!(new(15, 1, bi).build().is_ok());
    let past = |first, span| BlockError::PastGroup16 {
        first_group: FirstGroup::new(first).unwrap(),
        groups: GroupSpan::new(span).unwrap(),
    };
    assert_eq!(new(15, 2, bi).build(), Err(past(15, 2)));
    assert_eq!(new(1, 16, bi).build(), Err(past(1, 16)));
    assert_eq!(
        new(0, 1, midi::reserved_direction()).build(),
        Err(BlockError::ReservedDirection)
    );
    let one = || new(2, 1, bi);
    assert!(one().midi1(Midi1Port::Restricted).build().is_ok());
    assert_eq!(
        one().midi1(midi::reserved_midi1_port()).build(),
        Err(BlockError::ReservedMidi1Port)
    );
    let two = || new(2, 2, bi);
    assert_eq!(
        two().midi1(Midi1Port::Unrestricted).build(),
        Err(BlockError::Midi1PortSpansGroups {
            groups: GroupSpan::new(2).unwrap()
        })
    );
    assert!(two().midi1(Midi1Port::No).build().is_ok());
}

#[test]
fn a_block_is_replaced_by_number_and_the_count_follows_the_highest() {
    let e = Endpoint::new(Supports::of(FULL))
        .block(block(5, 0, 1).build().unwrap())
        .block(block(5, 2, 3).build().unwrap());
    assert_eq!(e.declared_function_blocks(), 6);
    assert_eq!(e.function_blocks().count(), 1);
    assert_eq!(e.function_block(id(5)).unwrap().first_group().get(), 2);
    assert!(e.function_block(id(0)).is_none());
}

fn replied(endpoint: &mut Endpoint, stream: Stream) -> EndpointReply {
    endpoint.reply(Message::Stream(stream), &mut Vec::new())
}

/// §7.1.6.1 step 3: a request the Endpoint can honour whole is `Granted` with
/// the configuration now active; one it could only partly honour is `Reduced`
/// and carries both what was asked and what is active.
#[test]
fn a_request_is_reported_as_granted_or_reduced() {
    let mut e = Endpoint::new(Supports::of(Capabilities {
        midi2: true,
        rx_jr: true,
        ..Capabilities::default()
    }));
    let granted = StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr: true,
        tx_jr: false,
    };
    assert_eq!(
        replied(&mut e, request(Protocol::Midi2, true, false)),
        EndpointReply::Configured(Configuration::Granted(granted))
    );
    let asked = StreamConfig {
        tx_jr: true,
        ..granted
    };
    let reduced = EndpointReply::Configured(Configuration::Reduced {
        asked,
        active: granted,
    });
    assert_eq!(
        replied(&mut e, request(Protocol::Midi2, true, true)),
        reduced
    );
    let EndpointReply::Configured(c) = reduced else {
        unreachable!()
    };
    assert_eq!(c.active(), granted);
    // A reserved Protocol is a part not done.
    assert!(matches!(
        replied(&mut e, request(midi::reserved_protocol(7), true, false)),
        EndpointReply::Configured(Configuration::Reduced { .. })
    ));
}

/// Discoveries are `Answered` even when they name a block the Endpoint lacks.
#[test]
fn a_discovery_is_answered_even_for_a_block_that_is_absent() {
    let mut e = device();
    assert_eq!(replied(&mut e, discovery(NONE)), EndpointReply::Answered);
    assert_eq!(
        replied(
            &mut e,
            block_discovery(BlockSelect::One(id(30)), true, true)
        ),
        EndpointReply::Answered
    );
}

/// The application's Endpoint (M2-104 §7.1.2, §7.1.8): UMP 1.1, both
/// Protocols, no JR Timestamps, one static, active, bidirectional block over
/// the Groups, which a discovery reports in the words of the spec.
/// Spec: m2-104/7.1.8#5, m2-104/7.1.8#9
#[test]
fn a_local_endpoint_declares_one_static_block_over_its_groups() {
    let mut e = Endpoint::local(
        Supports::of(Capabilities {
            midi1: true,
            midi2: true,
            ..Capabilities::default()
        }),
        GroupSpan::new(4).unwrap(),
    );
    assert_eq!(e.version(), UMP_VERSION);
    assert_eq!(
        e.capabilities(),
        Capabilities {
            midi1: true,
            midi2: true,
            rx_jr: false,
            tx_jr: false
        }
    );
    // Endpoint Info: version 1.1, static, one block, M2 and M1 set, no JR.
    let info = ask(
        &mut e,
        discovery(DiscoveryFilter {
            endpoint_info: true,
            ..NONE
        }),
    );
    assert_eq!(words(&info[0]), [0xF001_0101, 0x8100_0300, 0, 0]);
    let blocks = ask(&mut e, block_discovery(BlockSelect::All, true, false));
    let Message::Stream(Stream::FunctionBlockInfo(b)) = blocks[0].decode().unwrap() else {
        panic!("a Function Block Info");
    };
    assert!(b.active);
    assert_eq!(b.direction, BlockDirection::Bidirectional);
    assert_eq!(
        (
            b.first_group.value().map(FirstGroup::get),
            b.groups_spanned.value().map(GroupSpan::get)
        ),
        (Some(0), Some(4))
    );
    assert_eq!(blocks.len(), 1);
}

/// §7.1.8: an output-only block does not declare itself primarily a receiver,
/// and no hint contradicts the direction: an input-only block is no sender
/// either. A hint the direction allows is built.
/// Spec: m2-104/7.1.8#7, m2-104/7.1.8#8
#[test]
fn a_ui_hint_the_direction_rules_out_is_refused() {
    let with = |direction, hint| {
        FunctionBlock::builder(
            id(0),
            direction,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .ui_hint(hint)
        .build()
    };
    for (direction, hint) in [
        (BlockDirection::Output, UiHint::Receiver),
        (BlockDirection::Input, UiHint::Sender),
    ] {
        assert_eq!(
            with(direction, hint),
            Err(BlockError::UiHintContradictsDirection { direction, hint })
        );
    }
    for (direction, hint) in [
        (BlockDirection::Output, UiHint::Sender),
        (BlockDirection::Output, UiHint::Both),
        (BlockDirection::Output, UiHint::Unknown),
        (BlockDirection::Input, UiHint::Receiver),
        (BlockDirection::Input, UiHint::Both),
        (BlockDirection::Bidirectional, UiHint::Receiver),
        (BlockDirection::Bidirectional, UiHint::Sender),
    ] {
        assert!(with(direction, hint).is_ok(), "{direction:?} {hint:?}");
    }
}

/// §7.1.2 and §7.1.8: an Endpoint that declares its blocks static keeps its
/// declared count and every block's properties through any discovery it
/// answers.
/// Spec: m2-104/7.1.2#3, m2-104/7.1.2#4
#[test]
fn answering_discoveries_changes_no_block_of_a_static_endpoint() {
    let mut e = Endpoint::local(
        Supports::of(Capabilities {
            midi1: true,
            midi2: true,
            ..Capabilities::default()
        }),
        GroupSpan::new(4).unwrap(),
    );
    let before = e;
    ask(&mut e, discovery(ALL));
    ask(&mut e, block_discovery(BlockSelect::All, true, true));
    ask(&mut e, block_discovery(BlockSelect::One(id(0)), true, true));
    ask(&mut e, block_discovery(BlockSelect::One(id(7)), true, true));
    assert_eq!(e, before);
    assert_eq!(e.declared_function_blocks(), 1);
}

/// §7.1.5: an instance id of 30 bytes is a Start, a Continue and an End of 14
/// bytes each but the last, and the zero fill that ends it is in the End alone.
/// Spec: m2-104/7.1.5#5, m2-104/7.1.5#8
#[test]
fn a_long_instance_id_is_zero_filled_only_in_its_end() {
    let mut e = Endpoint::new(Supports::of(FULL))
        .instanced(ProductInstanceId::new("0123456789abcdefghijklmnopqrst").unwrap());
    let out = ask(
        &mut e,
        discovery(DiscoveryFilter {
            product_instance_id: true,
            ..NONE
        }),
    );
    assert_eq!(
        out.iter().map(words).collect::<Vec<_>>(),
        [
            [0xF404_3031, 0x3233_3435, 0x3637_3839, 0x6162_6364],
            [0xF804_6566, 0x6768_696A, 0x6B6C_6D6E, 0x6F70_7172],
            [0xFC04_7374, 0, 0, 0],
        ]
    );
}

/// §6.1.1 and §7.1.8, Figure 22: a block is bidirectional, input only or
/// output only, as the direction bits `0b11`, `0b01` and `0b10`.
/// Spec: m2-104/6.1.1#3
#[test]
fn a_block_is_bidirectional_input_only_or_output_only() {
    for (direction, bits) in [
        (BlockDirection::Bidirectional, 0b11),
        (BlockDirection::Input, 0b01),
        (BlockDirection::Output, 0b10),
    ] {
        let mut e = Endpoint::new(Supports::of(FULL)).block(
            FunctionBlock::builder(
                id(0),
                direction,
                FirstGroup::new(0).unwrap(),
                GroupSpan::new(1).unwrap(),
            )
            .build()
            .unwrap(),
        );
        let out = ask(&mut e, block_discovery(BlockSelect::All, true, false));
        assert_eq!(words(&out[0])[0], 0xF011_8000 | bits);
    }
}

/// §6.1.1 and §6.2: blocks may overlap and share a Group, and a MIDI 1.0 port
/// block may sit on a Group a MIDI 2.0 block spans: each is declared as it is.
/// Spec: m2-104/6.1.1#5, m2-104/6.1.1#6, m2-104/6.2.1.2#2, m2-104/6.2.2#1, m2-104/6.2.2#2
#[test]
fn blocks_may_share_and_overlap_groups() {
    let mut e = Endpoint::new(Supports::of(FULL))
        .block(block(0, 0, 4).build().unwrap())
        .block(block(1, 2, 1).midi1(Midi1Port::Restricted).build().unwrap())
        .block(block(2, 2, 1).build().unwrap());
    let out = ask(&mut e, block_discovery(BlockSelect::All, true, false));
    assert_eq!(
        out.iter().map(words).collect::<Vec<_>>(),
        [
            [0xF011_8003, 0x0004_0000, 0, 0],
            [0xF011_810B, 0x0201_0000, 0, 0],
            [0xF011_8203, 0x0201_0000, 0, 0],
        ]
    );
}

/// §7.1.6.3: what a capability change drops from the active configuration goes
/// off with a Stream Configuration Notification after the Endpoint Info: an
/// Endpoint in MIDI 2.0 with both JR directions that loses MIDI 2.0 and JR is
/// in MIDI 1.0 without JR (word 0 of the Notification: protocol 1 in bits 15-8).
/// A change that leaves the active configuration alone sends the Info only.
/// Spec: m2-104/7.1.2#2, m2-104/7.1.6.3#1
#[test]
fn a_capability_change_drops_what_it_no_longer_supports_from_the_active_set() {
    let active = StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr: true,
        tx_jr: true,
    };
    let mut e = changeable().in_use(active);
    let m1_only = Supports::of(Capabilities {
        midi1: true,
        ..Capabilities::default()
    });
    assert_eq!(
        changed(&mut e, EndpointChange::Capabilities(m1_only)),
        [[0xF001_0101, 0x0100_0100, 0, 0], [0xF006_0100, 0, 0, 0],]
    );
    assert_eq!(
        e.active(),
        StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false,
        }
    );
    let both = Supports::of(Capabilities {
        midi2: true,
        ..m1_only.get()
    });
    assert_eq!(
        changed(&mut e, EndpointChange::Capabilities(both)).len(),
        1,
        "nothing active was lost"
    );
}

/// A device whose blocks may change: not static, one named block 0 over Group 0.
fn changeable() -> Endpoint {
    Endpoint::new(Supports::of(FULL)).block(
        block(0, 0, 1)
            .name(BlockName::new("Keys").unwrap())
            .build()
            .unwrap(),
    )
}

fn changed(endpoint: &mut Endpoint, change: EndpointChange) -> Vec<[u32; 4]> {
    let mut out = Vec::new();
    endpoint.change(change, &mut out).unwrap();
    out.iter().map(words).collect()
}

/// §7.1.2: an Endpoint Info Notification goes out when a property of it
/// changes. Word 1 is the static bit, the block count (1 here), M2, M1, RXJR
/// and TXJR: 1 << 24 | 1 << 8 for MIDI 1.0 alone. A change to the same value
/// changes nothing and sends nothing.
/// Spec: m2-104/7.1.2#2
#[test]
fn a_changed_capability_sends_endpoint_info() {
    let mut e = changeable();
    let m1_only = Supports::of(Capabilities {
        midi1: true,
        ..Capabilities::default()
    });
    assert_eq!(
        changed(&mut e, EndpointChange::Capabilities(m1_only)),
        [[0xF001_0101, 0x0100_0100, 0, 0]]
    );
    assert_eq!(e.capabilities(), m1_only.get());
    assert!(changed(&mut e, EndpointChange::Capabilities(m1_only)).is_empty());
}

/// §7.1.2 and §7.1.8: a block added raises the declared count, so Endpoint
/// Info (count 3) comes first, then the block's Function Block Info: active,
/// block 2, no hint, bidirectional is 0x8000 | 0x0200 | 0b11, then first
/// Group 4, two Groups, no MIDI-CI, no streams. §7.1.9: a block with no name
/// has no name notification.
/// Spec: m2-104/7.1.2#2, m2-104/7.1.8#3
#[test]
fn a_block_added_sends_endpoint_info_then_its_block_info() {
    let mut e = changeable();
    let added = block(2, 4, 2).build().unwrap();
    assert_eq!(
        changed(&mut e, EndpointChange::Block(added)),
        [
            [0xF001_0101, 0x0300_0303, 0, 0],
            [0xF011_8203, 0x0402_0000, 0, 0],
        ]
    );
    assert_eq!(e.function_block(id(2)), Some(&added));
}

/// §7.1.8: a changed property of a block sends Function Block Info alone,
/// with the count unchanged: the active bit of word 0 goes low. §7.1.9: the
/// same block with a new name sends the name alone, a Complete message with
/// the block number then the text. The spec has no message that removes a
/// name; this crate sends an empty one as one Complete message of zeros, which
/// is pinned here.
/// Spec: m2-104/7.1.8#3, m2-104/7.1.9#3, m2-104/7.1.2#5
#[test]
fn a_changed_block_sends_the_part_that_changed() {
    let mut e = changeable();
    let keys = || block(0, 0, 1).name(BlockName::new("Keys").unwrap());
    assert_eq!(
        changed(
            &mut e,
            EndpointChange::Block(keys().active(false).build().unwrap())
        ),
        [[0xF011_0003, 0x0001_0000, 0, 0]]
    );
    let organ = block(0, 0, 1).name(BlockName::new("Organ").unwrap());
    assert_eq!(
        changed(&mut e, EndpointChange::Block(organ.build().unwrap())),
        [
            [0xF011_8003, 0x0001_0000, 0, 0],
            [0xF012_004F, 0x7267_616E, 0, 0],
        ]
    );
    assert_eq!(
        changed(
            &mut e,
            EndpointChange::Block(block(0, 0, 1).build().unwrap())
        ),
        [[0xF012_0000, 0, 0, 0]]
    );
    assert!(
        changed(
            &mut e,
            EndpointChange::Block(block(0, 0, 1).build().unwrap())
        )
        .is_empty()
    );
}

/// §7.1.2: with static blocks declared the Endpoint never changes a block
/// property or name, so the change is refused and nothing is sent or kept; a
/// replacement equal to the block held is no change.
/// Spec: m2-104/7.1.2#3, m2-104/7.1.9#3
#[test]
fn a_static_endpoint_refuses_a_block_change() {
    let mut e = device();
    let before = e;
    let mut out = Vec::new();
    let renamed = block(0, 0, 1)
        .name(BlockName::new("Organ").unwrap())
        .build()
        .unwrap();
    assert_eq!(
        e.change(EndpointChange::Block(renamed), &mut out),
        Err(ChangeError::StaticBlocks)
    );
    assert!(out.is_empty());
    assert_eq!(e, before);
    // A block of a number the Endpoint does not have is a new block: refused.
    let new = block(5, 8, 1).build().unwrap();
    assert_eq!(
        e.change(EndpointChange::Block(new), &mut out),
        Err(ChangeError::StaticBlocks)
    );
    assert!(out.is_empty());
    assert_eq!(e, before);
    let same = *before.function_block(id(0)).unwrap();
    assert_eq!(e.change(EndpointChange::Block(same), &mut out), Ok(()));
    assert!(out.is_empty());
}

/// §7.1.3 to §7.1.5: an identity, name or Product Instance Id may be sent at
/// any time, and a change of one sends it: the same words a discovery
/// answers with (Figure 14; a Complete name; "SN-0002" as ASCII).
/// Spec: m2-104/7.1.3#1, m2-104/7.1.4#1, m2-104/7.1.5#3
#[test]
fn a_changed_identity_name_or_instance_id_is_sent() {
    let mut e = changeable();
    assert_eq!(
        changed(&mut e, EndpointChange::Identity(identity())),
        [[0xF002_0000, 0x0000_2109, 0x0102_0304, 0x0506_0708]]
    );
    assert!(changed(&mut e, EndpointChange::Identity(identity())).is_empty());
    let name = EndpointName::new("Hi").unwrap();
    assert_eq!(
        changed(&mut e, EndpointChange::Name(name)),
        [[0xF003_4869, 0, 0, 0]]
    );
    assert_eq!(e.name(), Some(&name));
    assert!(changed(&mut e, EndpointChange::Name(name)).is_empty());
    let id = ProductInstanceId::new("SN-0002").unwrap();
    assert_eq!(
        changed(&mut e, EndpointChange::InstanceId(id)),
        [[0xF004_534E, 0x2D30_3030, 0x3200_0000, 0]]
    );
}

/// A merger of `sources` SysEx 8 payloads onto one receiver: each takes a
/// Stream ID of `ids` when one is free and waits when none is. Every running
/// message writes a packet in turn, and a message that is written frees its
/// ID. Returns the Stream IDs read off the wire words and what a receiver
/// assembles.
fn merge(ids: &Sysex8StreamIds, sources: &[Vec<u8>]) -> (Vec<u8>, Vec<Vec<u8>>) {
    use tutti_ump::{MidiGroup, Sysex8Assembler, Sysex8Step};

    let mut waiting: Vec<&Vec<u8>> = sources.iter().rev().collect();
    let mut running = Vec::new();
    let mut receiver = Sysex8Assembler::new(vec![0; 16 * 64], 64, 16);
    let (mut on_wire, mut done) = (Vec::new(), Vec::new());
    while !waiting.is_empty() || !running.is_empty() {
        while let Some(&payload) = waiting.last() {
            let Some(stream) = ids.open() else { break };
            running.push(stream.packets(MidiGroup::FIRST, payload));
            waiting.pop();
        }
        for packets in &mut running {
            let ump = packets.next().expect("a running message has a packet");
            on_wire.push(((ump.0[0] >> 8) & 0xFF) as u8);
            if let Sysex8Step::Complete(m) = receiver.push(ump) {
                done.push(m.payload.to_vec());
            }
        }
        running.retain(|packets| packets.len() > 0);
    }
    assert_eq!(receiver.dropped(), 0);
    (on_wire, done)
}

/// Function Block Info's `Max Number of SysEx8 Streams`: 0 is a receiver
/// without System Exclusive 8, 1 is one stream (ID 0), N is N.
/// Spec: m2-104/7.8#5, m2-104/7.8#7
#[test]
fn a_block_declares_the_streams_a_sender_may_use() {
    for (declared, count) in [(0, 0), (1, 1), (2, 2), (3, 3), (255, 255)] {
        let block = block(0, 0, 1)
            .midi_ci(None, Sysex8StreamCount::new(declared))
            .build()
            .unwrap();
        assert_eq!(block.sysex8_streams().count().get(), count);
    }
}

/// A block that declares 0 streams, the default, takes no System Exclusive 8:
/// a sender gets no stream.
#[test]
fn a_block_with_no_sysex8_gives_a_sender_no_stream() {
    let block = block(0, 0, 1).build().unwrap();
    assert_eq!(block.max_sysex8_streams(), Sysex8StreamCount::new(0));
    assert!(block.sysex8_streams().open().is_none());
}

/// By `Sysex8StreamIds`; no merger ships in tutti. Before discovery of more
/// than one stream, merged sources go one at a time on Stream ID 0.
/// Spec: m2-104/7.8#6, m2-104/7.8#8
#[test]
fn a_merger_without_discovery_sends_one_stream_at_a_time() {
    let sources = [vec![0x80; 30], vec![0x81; 30], vec![0x82; 20]];
    let (on_wire, done) = merge(&Sysex8StreamIds::undiscovered(), &sources);
    assert!(on_wire.iter().all(|&id| id == 0));
    assert_eq!(done, sources);
}

/// By `Sysex8StreamIds`; no merger ships in tutti. A merger that discovered N
/// streams uses only Stream IDs 0 to N-1: with N = 3 and five sources, the
/// fourth and fifth wait for an ID to be freed.
/// Spec: m2-104/7.8#7, m2-104/7.8#8
#[test]
fn a_merger_after_discovery_uses_only_stream_ids_below_the_count() {
    let block = block(0, 0, 1)
        .midi_ci(Some(CiVersion::of(1)), Sysex8StreamCount::new(3))
        .build()
        .unwrap();
    let sources: Vec<Vec<u8>> = (0..5)
        .map(|i| vec![0x90 + i; 40 + usize::from(i)])
        .collect();
    let (on_wire, done) = merge(&block.sysex8_streams(), &sources);
    assert!(on_wire.iter().all(|&id| id < 3));
    for id in 0..3 {
        assert!(on_wire.contains(&id), "stream {id} is used");
    }
    let mut got = done;
    got.sort();
    assert_eq!(got, sources);
}

/// The numbers an OS holds of a block that holds together: block 3, Groups 4
/// and 5, bidirectional, a MIDI-CI version and two SysEx 8 streams.
fn os() -> OsFunctionBlock {
    OsFunctionBlock {
        active: true,
        id: 3,
        direction: 3,
        ui_hint: UiHint::Both,
        midi1: 0,
        first_group: 4,
        groups_spanned: 2,
        midi_ci_version: 2,
        max_sysex8_streams: Sysex8StreamCount::new(2),
    }
}

/// The Function Block Info the OS numbers `block` read as.
fn read(block: OsFunctionBlock) -> FunctionBlockInfo {
    read_function_block(block)
}

fn info() -> FunctionBlockInfo {
    read(os())
}

/// `from_info` is strict: each field the block cannot hold is refused with the
/// error that names it, and nothing is repaired.
#[test]
fn from_info_refuses_what_the_block_cannot_hold() {
    let out_of_range = |field| Err(BlockError::OutOfRange { field });
    let refused = |block: OsFunctionBlock| FunctionBlock::from_info(&read(block), None);
    assert_eq!(
        refused(OsFunctionBlock { id: 32, ..os() }),
        out_of_range(BlockField::Id)
    );
    assert_eq!(
        refused(OsFunctionBlock {
            first_group: 16,
            ..os()
        }),
        out_of_range(BlockField::FirstGroup)
    );
    assert_eq!(
        refused(OsFunctionBlock {
            midi_ci_version: 0x20,
            ..os()
        }),
        out_of_range(BlockField::MidiCiVersion)
    );
    assert_eq!(
        refused(OsFunctionBlock {
            groups_spanned: 0,
            ..os()
        }),
        out_of_range(BlockField::Groups)
    );
    assert_eq!(
        refused(OsFunctionBlock {
            groups_spanned: 17,
            ..os()
        }),
        out_of_range(BlockField::Groups)
    );
    assert_eq!(
        refused(OsFunctionBlock {
            first_group: 15,
            ..os()
        }),
        Err(BlockError::PastGroup16 {
            first_group: FirstGroup::of(15),
            groups: GroupSpan::of(2)
        })
    );
    assert_eq!(
        refused(OsFunctionBlock {
            direction: 0,
            ..os()
        }),
        Err(BlockError::ReservedDirection)
    );
    assert_eq!(
        refused(OsFunctionBlock {
            direction: 2,
            ui_hint: UiHint::Receiver,
            ..os()
        }),
        Err(BlockError::UiHintContradictsDirection {
            direction: BlockDirection::Output,
            hint: UiHint::Receiver
        })
    );
    assert_eq!(
        refused(OsFunctionBlock { midi1: 3, ..os() }),
        Err(BlockError::ReservedMidi1Port)
    );
    assert_eq!(
        refused(OsFunctionBlock { midi1: 2, ..os() }),
        Err(BlockError::Midi1PortSpansGroups {
            groups: GroupSpan::of(2)
        })
    );
    assert_eq!(
        FunctionBlock::from_info(&info(), Some("a\0b")),
        Err(BlockError::Name(NameError::Nul))
    );
    assert_eq!(
        FunctionBlock::from_info(&info(), Some("\u{FEFF}a")),
        Err(BlockError::Name(NameError::Bom))
    );
    assert_eq!(
        FunctionBlock::from_info(&info(), Some(&"x".repeat(92))),
        Err(BlockError::Name(NameError::TooLong { max: 91 }))
    );
}

/// A block that holds together reads back field for field, with its name.
#[test]
fn from_info_keeps_every_field_of_a_block_that_holds() {
    let block = FunctionBlock::from_info(&info(), Some("Keys")).unwrap();
    assert_eq!(block.info(), info());
    assert_eq!(block.name().map(BlockName::as_str), Some("Keys"));
    assert_eq!(
        FunctionBlock::from_info(&info(), None).unwrap().name(),
        None
    );
}

/// The reading a receiver gives a peer's Function Block Info is lenient where
/// `from_info` refuses (§7.1.8): a contradicting hint reads as unknown, a
/// reserved or multi-Group MIDI 1.0 port as not a port. What no block can hold
/// is still refused.
#[test]
fn the_wire_reading_repairs_what_from_info_refuses() {
    let contradicting = read(OsFunctionBlock {
        direction: 1,
        ui_hint: UiHint::Sender,
        ..os()
    });
    assert!(FunctionBlock::from_info(&contradicting, None).is_err());
    let block = FunctionBlock::received(&contradicting, None).unwrap();
    assert_eq!(block.ui_hint(), UiHint::Unknown);
    assert_eq!(block.direction(), BlockDirection::Input);

    for midi1 in [3, 1] {
        let spanning = read(OsFunctionBlock { midi1, ..os() });
        assert!(FunctionBlock::from_info(&spanning, None).is_err());
        assert_eq!(
            FunctionBlock::received(&spanning, None).unwrap().midi1(),
            Midi1Port::No
        );
    }
    let one_group = read(OsFunctionBlock {
        midi1: 2,
        groups_spanned: 1,
        ..os()
    });
    assert_eq!(
        FunctionBlock::received(&one_group, None).unwrap().midi1(),
        Midi1Port::Restricted
    );
    for refused in [
        OsFunctionBlock { id: 32, ..os() },
        OsFunctionBlock {
            first_group: 16,
            ..os()
        },
        OsFunctionBlock {
            groups_spanned: 0,
            ..os()
        },
        OsFunctionBlock {
            direction: 0,
            ..os()
        },
    ] {
        assert_eq!(FunctionBlock::received(&read(refused), None), None);
    }
}

/// A name an OS reports is display text: the block is kept whatever it holds.
/// One over 91 bytes is cut, and one with a NUL or a Byte Order Mark is
/// dropped while the block stays.
#[test]
fn received_keeps_the_block_whatever_its_name_holds() {
    let name = |text: &str| {
        FunctionBlock::received(&info(), Some(text))
            .map(|b| b.name().map(BlockName::as_str).map(str::to_owned))
    };
    assert_eq!(name("Keys"), Some(Some("Keys".to_owned())));
    assert_eq!(
        name("\u{FEFF}Keys"),
        Some(None),
        "a BOM drops the name only"
    );
    assert_eq!(name("a\0b"), Some(None), "a NUL drops the name only");
    let long = FunctionBlock::received(&info(), Some(&"x".repeat(200))).unwrap();
    assert_eq!(long.name().map(|n| n.as_str().len()), Some(91));
}
