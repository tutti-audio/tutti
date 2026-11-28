use super::*;
use tutti_ump::{
    FunctionBlockCount, FunctionBlocks, Message, OsFunctionBlock, Stream, Sysex8StreamCount, Ump,
    UmpVersion, read_function_block,
};

/// A block as an operating system would hold it; the fields a test varies are overridden.
fn os_block(id: u8) -> OsFunctionBlock {
    OsFunctionBlock {
        active: true,
        id,
        direction: 3,
        ui_hint: UiHint::Both,
        midi1: 0,
        first_group: 0,
        groups_spanned: 1,
        midi_ci_version: 0,
        max_sysex8_streams: Sysex8StreamCount::new(0),
    }
}

fn block(os: OsFunctionBlock, name: Option<&str>) -> Block {
    Block {
        info: read_function_block(os),
        name: name.map(str::to_owned),
    }
}

fn endpoint_info(blocks: usize) -> EndpointInfoNotification {
    EndpointInfoNotification {
        version: UmpVersion { major: 1, minor: 1 },
        static_function_blocks: true,
        function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(blocks)),
        midi2: true,
        midi1: false,
        rx_jr: false,
        tx_jr: false,
    }
}

/// M2-104 §7.1.8: a block's direction, UI hint and MIDI 1.0 port fields take the service's
/// numbers, which are the spec's, and every other field passes through, name included.
#[test]
fn a_blocks_fields_become_the_services_numbers_of_the_same_spec_values() {
    let (_, blocks) = platform(
        &endpoint_info(1),
        &[block(
            OsFunctionBlock {
                active: false,
                direction: 2,
                ui_hint: UiHint::Sender,
                midi1: 2,
                first_group: 5,
                groups_spanned: 3,
                midi_ci_version: 2,
                max_sysex8_streams: Sysex8StreamCount::new(7),
                ..os_block(9)
            },
            Some("Keys"),
        )],
    )
    .unwrap();
    assert_eq!(
        blocks,
        [PlatformBlock {
            number: 9,
            name: "Keys".into(),
            active: false,
            direction: DIRECTION_OUTPUT,
            ui_hint: UI_HINT_SENDER,
            midi1: MIDI1_RESTRICTED,
            first_group: 5,
            groups: 3,
            ci_version: 2,
            max_sysex8_streams: 7,
        }]
    );
}

/// Each defined value of the three enumerations has its own number, in the spec's order.
/// Spec: m2-104/6.1.1#3
#[test]
fn each_defined_enumeration_value_has_its_own_number() {
    let map =
        |os: OsFunctionBlock| platform(&endpoint_info(1), &[block(os, None)]).unwrap().1[0].clone();
    assert_eq!(
        [1, 2, 3].map(|direction| map(OsFunctionBlock {
            direction,
            ..os_block(0)
        })
        .direction),
        [DIRECTION_INPUT, DIRECTION_OUTPUT, DIRECTION_BIDIRECTIONAL]
    );
    assert_eq!(
        [0, 1, 2].map(|midi1| map(OsFunctionBlock {
            midi1,
            ..os_block(0)
        })
        .midi1),
        [MIDI1_NOT_A_PORT, MIDI1_UNRESTRICTED, MIDI1_RESTRICTED]
    );
    let hints = [
        UiHint::Unknown,
        UiHint::Receiver,
        UiHint::Sender,
        UiHint::Both,
    ]
    .map(|ui_hint| {
        map(OsFunctionBlock {
            ui_hint,
            ..os_block(0)
        })
        .ui_hint
    });
    assert_eq!(
        hints,
        [
            UI_HINT_UNKNOWN,
            UI_HINT_RECEIVER,
            UI_HINT_SENDER,
            UI_HINT_BOTH
        ]
    );
}

/// A block with no name and none of MIDI-CI declares an empty name and version 0.
#[test]
fn a_block_with_no_name_or_ci_version_declares_empty_and_zero() {
    let (_, blocks) = platform(&endpoint_info(1), &[block(os_block(0), None)]).unwrap();
    assert_eq!((blocks[0].name.as_str(), blocks[0].ci_version), ("", 0));
}

/// A reserved direction (`0b00`) or MIDI 1.0 status (`0b11`) has no number in the service.
#[test]
fn a_reserved_direction_or_midi1_status_is_refused() {
    let direction = block(
        OsFunctionBlock {
            direction: 0,
            ..os_block(0)
        },
        None,
    );
    assert_eq!(
        platform(&endpoint_info(1), &[direction]).unwrap_err(),
        Refusal::Reserved(DeclaredField::Direction)
    );
    let midi1 = block(
        OsFunctionBlock {
            midi1: 3,
            ..os_block(0)
        },
        None,
    );
    assert_eq!(
        platform(&endpoint_info(1), &[midi1]).unwrap_err(),
        Refusal::Reserved(DeclaredField::Midi1)
    );
}

/// A block number, first Group, group span or MIDI-CI version outside its range is refused
/// by the field's name.
/// Spec: m2-104/6.1.1#1
#[test]
fn a_reserved_number_group_span_or_ci_version_is_refused() {
    for (os, field) in [
        (
            OsFunctionBlock {
                id: 40,
                ..os_block(0)
            },
            DeclaredField::BlockNumber,
        ),
        (
            OsFunctionBlock {
                first_group: 16,
                ..os_block(0)
            },
            DeclaredField::FirstGroup,
        ),
        (
            OsFunctionBlock {
                groups_spanned: 0,
                ..os_block(0)
            },
            DeclaredField::GroupSpan,
        ),
        (
            OsFunctionBlock {
                midi_ci_version: 0x40,
                ..os_block(0)
            },
            DeclaredField::CiVersion,
        ),
    ] {
        assert_eq!(
            platform(&endpoint_info(1), &[block(os, None)]).unwrap_err(),
            Refusal::Reserved(field)
        );
    }
}

/// The endpoint's declared count is the number of blocks given, and a reserved count is
/// refused.
/// Spec: m2-104/6.1.1#2
#[test]
fn the_declared_block_count_is_the_blocks_given() {
    let blocks = [block(os_block(0), None), block(os_block(1), None)];
    let (info, _) = platform(&endpoint_info(2), &blocks).unwrap();
    assert_eq!(info.block_count, 2);
    assert_eq!(
        platform(&endpoint_info(1), &blocks).unwrap_err(),
        Refusal::BlockCount {
            declared: 1,
            given: 2
        }
    );
    // A count of 40, read from an Endpoint Info Notification (M2-104 §7.1.2).
    let Ok(Message::Stream(Stream::EndpointInfo(reserved))) =
        Ump([0xF001_0101, 0x8000_0000 | (40 << 24) | 0x200, 0, 0]).decode()
    else {
        panic!("an Endpoint Info Notification");
    };
    assert_eq!(
        platform(&reserved, &blocks).unwrap_err(),
        Refusal::Reserved(DeclaredField::BlockCount)
    );
}

/// The info's protocols, JR Timestamps, static flag and UMP version pass through.
#[test]
fn the_endpoint_info_passes_through() {
    let (info, _) = platform(&endpoint_info(0), &[]).unwrap();
    assert_eq!(
        info,
        PlatformInfo {
            midi1: false,
            midi2: true,
            rx_jr: false,
            tx_jr: false,
            static_blocks: true,
            block_count: 0,
            version_major: 1,
            version_minor: 1,
        }
    );
}
