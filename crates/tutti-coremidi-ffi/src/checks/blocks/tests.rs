use super::*;
use tutti_ump::{OsFunctionBlock, read_function_block};

/// A block as an operating system would hold it; the fields a test varies are
/// overridden.
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
        max_sysex8_streams: 0,
    }
}

fn info(block: OsFunctionBlock) -> FunctionBlockInfo {
    read_function_block(block)
}

/// M2-104 §7.1.8: the direction field's values `0b01`, `0b10` and `0b11` are
/// the numbers `MIDIMessages.h` gives input, output and bidirectional, and the
/// Group fields, the stream count and the active flag pass through.
#[test]
fn a_blocks_fields_become_the_platform_numbers_of_the_same_spec_values() {
    for (direction, platform) in [(1, 1), (2, 2), (3, 3)] {
        let blocks = platform_blocks(&[(
            info(OsFunctionBlock {
                direction,
                ..os_block(0)
            }),
            None,
        )])
        .unwrap();
        assert_eq!(blocks[0].direction, platform);
    }
    let block = OsFunctionBlock {
        active: false,
        direction: 2,
        ui_hint: UiHint::Sender,
        midi1: 2,
        first_group: 5,
        groups_spanned: 3,
        max_sysex8_streams: 7,
        ..os_block(0)
    };
    let blocks = platform_blocks(&[(info(block), Some("Keys"))]).unwrap();
    assert_eq!(
        blocks,
        [PlatformBlock {
            name: "Keys".into(),
            direction: 2,
            first_group: 5,
            groups_spanned: 3,
            max_sysex8_streams: 7,
            midi1: 2,
            ui_hint: 2,
            enabled: false,
        }]
    );
}

/// The UI hint and MIDI 1.0 port fields are two bits on the wire (§7.1.8), and
/// each of their defined values maps to the one number of that value.
#[test]
fn each_ui_hint_and_midi1_port_value_has_its_own_platform_number() {
    let hints = [
        UiHint::Unknown,
        UiHint::Receiver,
        UiHint::Sender,
        UiHint::Both,
    ]
    .map(|ui_hint| {
        platform_blocks(&[(
            info(OsFunctionBlock {
                ui_hint,
                ..os_block(0)
            }),
            None,
        )])
        .unwrap()[0]
            .ui_hint
    });
    assert_eq!(hints, [0, 1, 2, 3]);
    let ports = [0, 1, 2].map(|midi1| {
        platform_blocks(&[(
            info(OsFunctionBlock {
                midi1,
                ..os_block(0)
            }),
            None,
        )])
        .unwrap()[0]
            .midi1
    });
    assert_eq!(ports, [0, 1, 2]);
}

/// CoreMIDI numbers blocks by position, so blocks given out of order come out in
/// number order, which is the number CoreMIDI will give each.
#[test]
fn blocks_are_ordered_by_their_number() {
    let blocks = platform_blocks(&[
        (info(os_block(1)), Some("second")),
        (info(os_block(0)), Some("first")),
    ])
    .unwrap();
    let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["first", "second"]);
}

/// A gap in the numbers is refused: CoreMIDI would number the second block 1, and
/// a peer reading block 2 would find none.
#[test]
fn a_gap_in_the_block_numbers_is_refused_by_name() {
    assert_eq!(
        platform_blocks(&[(info(os_block(0)), None), (info(os_block(2)), None)]),
        Err(BlockRefusal::IdIsNotPosition { id: 2, position: 1 })
    );
    assert_eq!(
        platform_blocks(&[(info(os_block(3)), None)]),
        Err(BlockRefusal::IdIsNotPosition { id: 3, position: 0 })
    );
}

/// `MIDIUMPMutableFunctionBlock` has no MIDI-CI version: a block that declares
/// one is refused, never created without it.
#[test]
fn a_midi_ci_version_is_refused_by_name() {
    let block = OsFunctionBlock {
        midi_ci_version: 2,
        ..os_block(0)
    };
    assert_eq!(
        platform_blocks(&[(info(block), None)]),
        Err(BlockRefusal::MidiCiVersion { id: 0, version: 2 })
    );
}

/// `maxSysEx8Streams` is a `MIDIUInteger7`: 127 fits and 128 is refused.
#[test]
fn sysex8_streams_past_seven_bits_are_refused() {
    let streams = |n| OsFunctionBlock {
        max_sysex8_streams: n,
        ..os_block(0)
    };
    assert!(platform_blocks(&[(info(streams(127)), None)]).is_ok());
    assert_eq!(
        platform_blocks(&[(info(streams(128)), None)]),
        Err(BlockRefusal::Sysex8Streams {
            id: 0,
            streams: 128
        })
    );
}

/// A reserved direction (`0b00`) or MIDI 1.0 status (`0b11`) has no CoreMIDI
/// value and is refused.
#[test]
fn a_reserved_direction_or_midi1_status_is_refused() {
    assert_eq!(
        platform_blocks(&[(
            info(OsFunctionBlock {
                direction: 0,
                ..os_block(0)
            }),
            None
        )]),
        Err(BlockRefusal::Reserved(BlockField::Direction))
    );
    assert_eq!(
        platform_blocks(&[(
            info(OsFunctionBlock {
                midi1: 3,
                ..os_block(0)
            }),
            None
        )]),
        Err(BlockRefusal::Reserved(BlockField::Midi1))
    );
}

/// No blocks is an endpoint with none: the empty list maps to the empty list.
#[test]
fn no_blocks_map_to_no_blocks() {
    assert_eq!(platform_blocks(&[]), Ok(Vec::new()));
}

fn named(name: &str) -> PlatformBlock {
    PlatformBlock {
        name: name.into(),
        direction: 3,
        first_group: 0,
        groups_spanned: 1,
        max_sysex8_streams: 0,
        midi1: 0,
        ui_hint: 0,
        enabled: true,
    }
}

fn listed(pairs: &[(&str, u8)]) -> Vec<(String, u8)> {
    pairs.iter().map(|(n, k)| ((*n).into(), *k)).collect()
}

/// Distinct names are matched by name, so a listing in another order still gives
/// each registered block its own number.
#[test]
fn numbers_are_matched_to_registered_blocks_by_name() {
    let registered = [named("a"), named("b")];
    assert_eq!(
        numbers_in_registered_order(&registered, &listed(&[("b", 7), ("a", 6)])),
        Some(vec![6, 7])
    );
}

/// Names that repeat or are empty cannot identify a block, so the listed order is
/// the registered order.
#[test]
fn numbers_of_unnamed_or_repeated_blocks_follow_the_listed_order() {
    let registered = [named(""), named("")];
    assert_eq!(
        numbers_in_registered_order(&registered, &listed(&[("", 4), ("", 5)])),
        Some(vec![4, 5])
    );
    let twins = [named("x"), named("x")];
    assert_eq!(
        numbers_in_registered_order(&twins, &listed(&[("x", 1), ("x", 2)])),
        Some(vec![1, 2])
    );
}

/// A listing of another count, or without a registered name, is not of the
/// registered blocks.
#[test]
fn a_listing_that_is_not_of_the_registered_blocks_has_no_numbers() {
    let registered = [named("a"), named("b")];
    assert_eq!(
        numbers_in_registered_order(&registered, &listed(&[("a", 1)])),
        None
    );
    assert_eq!(
        numbers_in_registered_order(&registered, &listed(&[("a", 1), ("c", 2)])),
        None
    );
    assert_eq!(
        numbers_in_registered_order(&registered, &listed(&[("a", 1), ("a", 2)])),
        None
    );
}
