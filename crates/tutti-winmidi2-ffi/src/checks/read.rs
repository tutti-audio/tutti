//! What the service declares of an endpoint, read as a receiver reads what a device reports:
//! the service's numbers become `tutti-ump`'s Endpoint Info and Function Block fields, and a
//! number outside a field's table is the field's reserved raw.
//!
//! The numbers are the spec's (M2-104 §7.1.2, §7.1.8), the same ones `declared` writes, so
//! the mapping runs on every target.

use tutti_ump::{
    EndpointInfoNotification, Message, OsFunctionBlock, Stream, Sysex8StreamCount, UiHint, Ump,
    read_function_block,
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
}

/// The Endpoint Info Notification `info` reads as: its Endpoint Info words decoded, so a
/// count of Function Blocks above 32 is the reserved count as read.
///
/// `None` only if the words do not decode as an Endpoint Info Notification, which words built
/// here always do.
pub(crate) fn endpoint_info(info: &PlatformInfo) -> Option<EndpointInfoNotification> {
    let word0 = 0xF001_0000 | (u32::from(info.version_major) << 8) | u32::from(info.version_minor);
    let word1 = (u32::from(info.static_blocks) << 31)
        | (u32::from(info.block_count & 0x7F) << 24)
        | (u32::from(info.midi2) << 9)
        | (u32::from(info.midi1) << 8)
        | (u32::from(info.rx_jr) << 1)
        | u32::from(info.tx_jr);
    match Ump([word0, word1, 0, 0]).decode() {
        Ok(Message::Stream(Stream::EndpointInfo(info))) => Some(info),
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
