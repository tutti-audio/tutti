use super::*;
use tutti_ump::BlockName;
use tutti_ump::{OsFunctionBlock, Sysex8StreamCount, UiHint, read_function_block, read_protocol};

fn info(id: u8) -> FunctionBlockInfo {
    read_function_block(OsFunctionBlock {
        active: true,
        id,
        direction: 2,
        ui_hint: UiHint::Unknown,
        midi1: 0,
        first_group: 4,
        groups_spanned: 2,
        midi_ci_version: 0,
        max_sysex8_streams: Sysex8StreamCount::new(0),
    })
}

fn midi2(blocks: &[FunctionBlockInfo]) -> Endpoint {
    from_os(
        Protocol::Midi2,
        None,
        blocks.iter().map(|block| (*block, None)),
    )
    .endpoint
}

/// What an OS describes is the protocol in use and the blocks it declares: the
/// Endpoint reads back the active protocol (not what it offers), no JR
/// Timestamps, and each block as the OS said it.
#[test]
fn an_os_endpoint_is_the_active_protocol_and_the_blocks_it_declares() {
    let offered = EndpointInfoNotification {
        version: tutti_ump::UmpVersion::V1_1,
        static_function_blocks: false,
        function_blocks: tutti_ump::FunctionBlocks::Count(tutti_ump::FunctionBlockCount::of(1)),
        midi2: true,
        midi1: true,
        rx_jr: true,
        tx_jr: true,
    };
    let endpoint = from_os(Protocol::Midi1, Some(&offered), [(info(3), Some("Out"))]).endpoint;
    assert_eq!(
        endpoint.active().protocol,
        Protocol::Midi1,
        "active, not offered"
    );
    assert!(endpoint.capabilities().midi1 && endpoint.capabilities().midi2);
    assert!(!endpoint.active().rx_jr && !endpoint.active().tx_jr);
    let [block] = endpoint.function_blocks().collect::<Vec<_>>()[..] else {
        panic!("one block");
    };
    assert_eq!(block.info(), info(3));
    assert_eq!(block.name().map(BlockName::as_str), Some("Out"));
}

/// An OS block the spec cannot hold is left out and the others stay, never
/// clamped into a block it is not; a reserved protocol is spoken as MIDI 1.0.
#[test]
fn an_os_block_the_spec_cannot_hold_is_left_out() {
    let endpoint = midi2(&[info(1), info(32), info(2)]);
    let ids: Vec<u8> = endpoint.function_blocks().map(|b| b.id().get()).collect();
    assert_eq!(ids, [1, 2]);
    let reserved = from_os(read_protocol(0), None, []).endpoint;
    assert_eq!(reserved.active().protocol, Protocol::Midi1);
}

/// **Each block left out is counted**, so an app can tell a short list of blocks
/// from a device that declared fewer: two of the four declared are past the
/// spec's range (id 32, 40), and the described endpoint says two.
#[test]
fn the_blocks_an_os_declares_and_the_spec_cannot_hold_are_counted() {
    let described = from_os(
        Protocol::Midi2,
        None,
        [info(1), info(32), info(2), info(40)].map(|block| (block, None)),
    );
    assert_eq!(described.endpoint.function_blocks().count(), 2);
    assert_eq!(described.blocks_left_out, 2);
    assert_eq!(
        from_os(Protocol::Midi2, None, [(info(1), None)]).blocks_left_out,
        0
    );
}

/// An OS name is display text: one over 91 bytes is cut and the block kept, and
/// one with a NUL drops the name and keeps the block.
#[test]
fn an_os_name_is_cut_to_fit_and_a_nul_drops_only_the_name() {
    let long = "x".repeat(200);
    let endpoint = from_os(
        Protocol::Midi2,
        None,
        [(info(0), Some(long.as_str())), (info(1), Some("a\0b"))],
    )
    .endpoint;
    let names: Vec<Option<usize>> = endpoint
        .function_blocks()
        .map(|b| b.name().map(|n| n.as_str().len()))
        .collect();
    assert_eq!(names, [Some(91), None]);
}

/// A reserved protocol is spoken as MIDI 1.0 but claims no support the OS did
/// not report: an endpoint that offers MIDI 2.0 alone supports MIDI 2.0 alone;
/// only one that reports neither is claimed to support MIDI 1.0.
#[test]
fn a_reserved_protocol_claims_no_support_the_os_did_not_report() {
    let offering = |midi1, midi2| EndpointInfoNotification {
        version: tutti_ump::UmpVersion::V1_1,
        static_function_blocks: false,
        function_blocks: tutti_ump::FunctionBlocks::Count(tutti_ump::FunctionBlockCount::of(0)),
        midi2,
        midi1,
        rx_jr: false,
        tx_jr: false,
    };
    let only2 = from_os(read_protocol(0), Some(&offering(false, true)), []).endpoint;
    assert_eq!(only2.active().protocol, Protocol::Midi1);
    assert!(only2.capabilities().midi2 && !only2.capabilities().midi1);
    let neither = from_os(read_protocol(0), Some(&offering(false, false)), []).endpoint;
    assert!(neither.capabilities().midi1 && !neither.capabilities().midi2);
}

/// What is a UMP Endpoint to ask (M2-104 Appendix I): the MIDI 2.0 Protocol in
/// use, or blocks declared. A MIDI 1.0 port with none has no Stream.
#[test]
fn only_a_ump_endpoint_is_asked() {
    assert!(is_ump_endpoint(&midi2(&[])));
    assert!(is_ump_endpoint(
        &from_os(Protocol::Midi1, None, [(info(0), None)]).endpoint
    ));
    assert!(!is_ump_endpoint(&legacy()));
}

#[test]
fn a_midi1_endpoint_does_not_carry_midi2_only_messages() {
    assert!(!carries_midi2_only(&legacy()));
    assert!(carries_midi2_only(&midi2(&[])));
}

/// A MIDI 1.0 port that declares no block is presented as a MIDI 1.0 device,
/// with its inferred block; any other endpoint is presented as the OS says.
#[test]
fn a_legacy_port_is_presented_as_a_midi1_device() {
    assert_eq!(described(&legacy()), Endpoint::midi1_device(None));
    let os = midi2(&[info(1)]);
    assert_eq!(described(&os), os);
    assert_eq!(described(&midi2(&[])), midi2(&[]));
    // What the OS offers is kept whether or not it declares a block.
    let offers_midi2 = from_os(
        Protocol::Midi1,
        Some(&EndpointInfoNotification {
            version: tutti_ump::UmpVersion::V1_1,
            static_function_blocks: false,
            function_blocks: tutti_ump::FunctionBlocks::Count(tutti_ump::FunctionBlockCount::of(0)),
            midi2: true,
            midi1: true,
            rx_jr: false,
            tx_jr: false,
        }),
        [],
    )
    .endpoint;
    assert!(described(&offers_midi2).capabilities().midi2);
}

/// The default is the *conservative* one: a protocol the OS never reported is
/// MIDI 1.0. The engine is MIDI-2-native, the wrong polarity for a *device*:
/// claiming MIDI 2.0 for an unknown endpoint would send per-note messages to an
/// endpoint that cannot carry them, while MIDI 1.0 only costs resolution.
#[test]
fn an_unreported_endpoint_is_midi1() {
    assert_eq!(legacy().active().protocol, Protocol::Midi1);
    assert!(!carries_midi2_only(&legacy()));
}
