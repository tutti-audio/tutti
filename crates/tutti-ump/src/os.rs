//! What an operating system reports of a UMP Endpoint, read as the decoder reads
//! the wire.
//!
//! An OS keeps a Function Block or an Endpoint's protocol as plain numbers, and
//! a number the spec does not define is a reserved value, as it is on the wire:
//! the block's table fields stay numbers here because a typed value cannot hold
//! one outside its table.
//! The readers here put the numbers in the words the wire carries them in and
//! read those words with the decoder, so a value outside a field's table is
//! kept as [`Reserved`](crate::Reserved) as read and a value in range is never
//! reserved.

use crate::decode::{function_block_info, stream_config};
use crate::{FunctionBlockInfo, Protocol, Sysex8StreamCount, UiHint};

/// The numbers an operating system holds of a Function Block: the fields of a
/// Function Block Info Notification (M2-104 §7.1.8), each as the OS keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OsFunctionBlock {
    /// Whether the block is active.
    pub active: bool,
    /// The block number.
    pub id: u8,
    /// The direction: `1` input, `2` output, `3` bidirectional.
    pub direction: u8,
    /// The UI hint, mapped by the OS edge from its own numbers: the spec has
    /// no reserved value for it.
    pub ui_hint: UiHint,
    /// The MIDI 1.0 port field: `0` not a port, `1` unrestricted, `2` restricted.
    pub midi1: u8,
    /// The first Group it spans.
    pub first_group: u8,
    /// How many Groups it spans.
    pub groups_spanned: u8,
    /// The MIDI-CI message version: `0` for none, a number above `0x1F` is
    /// reserved.
    pub midi_ci_version: u8,
    /// The simultaneous SysEx 8 streams.
    pub max_sysex8_streams: Sysex8StreamCount,
}

/// The Function Block Info the numbers of `block` read as. A number outside its
/// field's table is the field's reserved raw: a direction other than `1..=3`, a
/// MIDI 1.0 port other than `0..=2`, a block number above 31, a first Group
/// above 15, a Group span outside `1..=16`. A number wider than its field on
/// the wire (the block number is 7 bits, the direction and the MIDI 1.0 port
/// two) is held as a reserved value of that field, which is not the OS's number:
/// block number 200 is held as 127, and a direction or MIDI 1.0 port of 4 or more
/// as `0` and `3`.
#[must_use]
pub fn read_function_block(block: OsFunctionBlock) -> FunctionBlockInfo {
    // A value wider than its field cannot be put in the word: it is the field's
    // reserved value the wire could carry.
    let narrow = |raw: u8, width: u8, reserved: u8| if raw > width { reserved } else { raw };
    let ui_hint = match block.ui_hint {
        UiHint::Unknown => 0,
        UiHint::Receiver => 1,
        UiHint::Sender => 2,
        UiHint::Both => 3,
    };
    let w0 = 0xF011_0000
        | (u32::from(block.active) << 15)
        | (u32::from(narrow(block.id, 0x7f, 0x7f)) << 8)
        | (ui_hint << 4)
        | (u32::from(narrow(block.midi1, 3, 3)) << 2)
        | u32::from(narrow(block.direction, 3, 0));
    let w1 = (u32::from(block.first_group) << 24)
        | (u32::from(block.groups_spanned) << 16)
        | (u32::from(block.midi_ci_version) << 8)
        | u32::from(block.max_sysex8_streams.get());
    function_block_info(w0, w1)
}

/// The Protocol the number `raw` an operating system holds reads as: `1` MIDI
/// 1.0, `2` MIDI 2.0, any other value the reserved raw.
#[must_use]
pub fn read_protocol(raw: u8) -> Protocol {
    stream_config(0xF006_0000 | (u32::from(raw) << 8)).protocol
}
