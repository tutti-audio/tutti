use super::*;
use tutti_alsa_ffi::UmpBlock;
use tutti_midi_device::{Endpoint, FunctionBlock};
use tutti_ump::{
    EndpointInfoNotification, FunctionBlockCount, FunctionBlocks, Midi1Port, OsFunctionBlock,
    Protocol, UiHint, UmpVersion, read_function_block, read_protocol,
};

/// The Endpoint a UMP record describes.
fn capability_of(ump: Option<&UmpEndpoint>) -> Endpoint {
    super::capability_of(ump).endpoint
}

fn block(
    id: u8,
    direction: u8,
    first_group: u8,
    groups: u8,
    midi1: u8,
    ui_hint: UiHint,
) -> UmpBlock {
    UmpBlock {
        info: read_function_block(OsFunctionBlock {
            active: true,
            id,
            direction,
            ui_hint,
            midi1,
            first_group,
            groups_spanned: groups,
            midi_ci_version: 0,
            max_sysex8_streams: 0,
        }),
        name: None,
    }
}

fn record(protocol: Protocol, blocks: Vec<UmpBlock>) -> UmpEndpoint {
    UmpEndpoint {
        protocol,
        offered: EndpointInfoNotification {
            version: UmpVersion::V1_1,
            static_function_blocks: false,
            function_blocks: FunctionBlocks::Count(FunctionBlockCount::of(0)),
            midi2: true,
            midi1: true,
            rx_jr: false,
            tx_jr: false,
        },
        name: None,
        device: None,
        instance: None,
        blocks,
    }
}

fn blocks_of(blocks: Vec<UmpBlock>) -> Vec<u8> {
    capability_of(Some(&record(Protocol::Midi2, blocks)))
        .function_blocks()
        .map(|b| b.id().get())
        .collect()
}

// A sequencer block the UMP spec cannot hold is left out, not clamped: a
// reserved direction (M2-104 §7.1.8 `0b00`), a span past Group 16, a number
// above 31.
/// Spec: m2-104/6.1.1#1, m2-104/6.1.1#2
#[test]
fn a_block_the_spec_cannot_hold_is_left_out() {
    let held = blocks_of(vec![
        block(0, 0, 0, 1, 0, UiHint::Unknown),
        block(1, 1, 15, 2, 0, UiHint::Unknown),
        block(2, 1, 0, 0, 0, UiHint::Unknown),
        block(32, 1, 0, 1, 0, UiHint::Unknown),
        block(3, 2, 15, 1, 0, UiHint::Unknown),
    ]);
    assert_eq!(held, [3]);
}

// A MIDI 1.0 port block reads as one, restricted when the sequencer says
// low speed, and only over a single Group (§7.1.8): over several Groups the
// block is kept as a plain block, as is one whose port value is reserved.
#[test]
fn a_midi1_port_block_keeps_its_bandwidth_and_is_kept_when_it_cannot_be_a_port() {
    let port = |midi1, groups| {
        capability_of(Some(&record(
            Protocol::Midi2,
            vec![block(3, 3, 0, groups, midi1, UiHint::Unknown)],
        )))
        .function_blocks()
        .next()
        .map(FunctionBlock::midi1)
    };
    assert_eq!(port(1, 1), Some(Midi1Port::Unrestricted));
    assert_eq!(port(2, 1), Some(Midi1Port::Restricted));
    assert_eq!(port(0, 1), Some(Midi1Port::No));
    assert_eq!(port(1, 2), Some(Midi1Port::No), "a port over two Groups");
    assert_eq!(port(3, 1), Some(Midi1Port::No), "a reserved port value");
}

/// A UI hint the direction rules out reads as unknown, and the block is kept.
#[test]
fn a_hint_that_contradicts_the_direction_reads_as_unknown() {
    let cap = capability_of(Some(&record(
        Protocol::Midi2,
        vec![block(0, 2, 0, 1, 0, UiHint::Receiver)],
    )));
    assert_eq!(cap.function_blocks().count(), 1);
    assert_eq!(
        cap.function_blocks().next().unwrap().ui_hint(),
        UiHint::Unknown
    );
}

/// A name with a Byte Order Mark drops the name and keeps the block.
#[test]
fn a_name_with_a_bom_drops_only_the_name() {
    let mut b = block(3, 3, 0, 1, 0, UiHint::Unknown);
    b.name = Some("\u{FEFF}Keys".to_owned());
    let cap = capability_of(Some(&record(Protocol::Midi2, vec![b])));
    assert_eq!(cap.function_blocks().count(), 1);
    assert_eq!(cap.function_blocks().next().unwrap().name(), None);
}

/// A name the OS gives past a block name's 91 bytes is cut for display, and the
/// block is kept.
#[test]
fn a_long_os_name_is_cut_not_refused() {
    let mut b = block(3, 3, 0, 1, 0, UiHint::Unknown);
    b.name = Some("é".repeat(60));
    let cap = capability_of(Some(&record(Protocol::Midi2, vec![b])));
    let name = cap.function_blocks().next().unwrap().name().unwrap();
    assert_eq!(name.as_str(), "é".repeat(45));
}

// A client with no UMP Endpoint record is MIDI 1.0 with no blocks, however
// the rest reads.
#[test]
fn a_client_with_no_endpoint_record_is_midi1() {
    assert_eq!(capability_of(None), legacy());
}

/// The protocol in use and those offered are kept apart: an Endpoint that
/// offers both and is in MIDI 1.0 is in MIDI 1.0 and supports MIDI 2.0; a
/// reserved protocol reads as MIDI 1.0.
#[test]
fn the_protocol_in_use_and_those_offered_are_kept_apart() {
    let cap = capability_of(Some(&record(Protocol::Midi1, Vec::new())));
    assert_eq!(cap.active().protocol, Protocol::Midi1);
    assert!(cap.capabilities().midi2 && cap.capabilities().midi1);
    let cap = capability_of(Some(&record(read_protocol(0), Vec::new())));
    assert_eq!(cap.active().protocol, Protocol::Midi1);
}
