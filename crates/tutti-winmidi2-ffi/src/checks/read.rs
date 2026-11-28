//! What the service declares of an endpoint, read as a receiver reads what a device reports:
//! the service's numbers become `tutti-ump`'s Endpoint Info and Function Block fields, and a
//! number outside a field's table is the field's reserved raw.
//!
//! The numbers are the spec's (M2-104 §7.1.2, §7.1.8), the same ones `declared` writes, so
//! the mapping runs on every target.

use tutti_ump::{
    EndpointInfoNotification, FunctionBlockCount, FunctionBlocks, OsFunctionBlock, Protocol,
    Sysex8StreamCount, UiHint, UmpVersion, read_function_block,
};

use super::declared::{Block, PlatformBlock, PlatformInfo};

/// What the service declares of an endpoint: its Endpoint Info and its Function Blocks.
///
/// An endpoint whose discovery has not completed declares what the service defaulted from
/// its transport, and no blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// The protocols it supports, its JR Timestamp capabilities, its UMP version and how
    /// many Function Blocks it declares.
    pub info: EndpointInfoNotification,
    /// The Function Blocks it declares, in the service's order.
    pub blocks: Vec<Block>,
    /// The protocol the service and the endpoint agreed on, filled in by MIDI 2.0 protocol
    /// negotiation. `None` when they have agreed on none (yet), which is neither MIDI 1.0 nor
    /// MIDI 2.0.
    pub protocol: Option<Protocol>,
}

/// The Endpoint Info Notification `info` reads as. A count of Function Blocks above 32
/// saturates to the most the field holds, as a count read from ALSA does.
pub(crate) fn endpoint_info(info: &PlatformInfo) -> EndpointInfoNotification {
    EndpointInfoNotification {
        version: UmpVersion::new(info.version_major, info.version_minor),
        static_function_blocks: info.static_blocks,
        function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(usize::from(
            info.block_count,
        ))),
        midi2: info.midi2,
        midi1: info.midi1,
        rx_jr: info.rx_jr,
        tx_jr: info.tx_jr,
    }
}

/// `MidiProtocol` as the service numbers it: `Midi1` is `1` and `Midi2` is `2`.
pub(crate) const PROTOCOL_MIDI1: i32 = 1;
pub(crate) const PROTOCOL_MIDI2: i32 = 2;

/// The protocol the service and the endpoint agreed on, as `MidiProtocol` numbers it. `Default`
/// (`0`), which the service reports until protocol negotiation has run, and any other number
/// are no agreement: `None`.
pub(crate) fn agreed_protocol(protocol: i32) -> Option<Protocol> {
    match protocol {
        PROTOCOL_MIDI1 => Some(Protocol::Midi1),
        PROTOCOL_MIDI2 => Some(Protocol::Midi2),
        _ => None,
    }
}

/// The service's number as the byte the spec's field holds: one that is no byte is
/// `u8::MAX`, which no field of a block defines.
fn byte(value: i32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// The Function Block `block` reads as, its name `None` when empty. The UI hint is the two
/// bits the wire carries, so the bits above them are not part of it.
pub(crate) fn function_block(block: &PlatformBlock) -> Block {
    let ui_hint = match block.ui_hint & 0x3 {
        0 => UiHint::Unknown,
        1 => UiHint::Receiver,
        2 => UiHint::Sender,
        _ => UiHint::Both,
    };
    Block {
        info: read_function_block(OsFunctionBlock {
            active: block.active,
            id: block.number,
            direction: byte(block.direction),
            ui_hint,
            midi1: byte(block.midi1),
            first_group: block.first_group,
            groups_spanned: block.groups,
            midi_ci_version: block.ci_version,
            max_sysex8_streams: Sysex8StreamCount::new(block.max_sysex8_streams),
        }),
        name: (!block.name.is_empty()).then(|| block.name.clone()),
    }
}

#[cfg(test)]
mod tests;
