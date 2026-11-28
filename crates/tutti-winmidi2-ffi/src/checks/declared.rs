//! What a virtual device declares, as the service's numbers: the Function Blocks and the
//! endpoint's info, with the values the service has no number for refused by name.
//!
//! The service takes each field of a Function Block as its own property and numbers the
//! enumerations as M2-104 §7.1.8 does (direction, MIDI 1.0 port, UI hint), so the mapping
//! is a refusal of the reserved reads and the spec's numbers, and runs on every target.

use tutti_ump::{BlockDirection, EndpointInfoNotification, FunctionBlockInfo, Midi1Port, UiHint};

/// `MidiFunctionBlockDirection`: M2-104's direction field, `0b01` input, `0b10` output,
/// `0b11` bidirectional.
pub(crate) const DIRECTION_INPUT: i32 = 1;
pub(crate) const DIRECTION_OUTPUT: i32 = 2;
pub(crate) const DIRECTION_BIDIRECTIONAL: i32 = 3;

/// `MidiFunctionBlockRepresentsMidi10Connection`: the MIDI 1.0 port field.
pub(crate) const MIDI1_NOT_A_PORT: i32 = 0;
pub(crate) const MIDI1_UNRESTRICTED: i32 = 1;
pub(crate) const MIDI1_RESTRICTED: i32 = 2;

/// `MidiFunctionBlockUIHint`: the UI hint field.
pub(crate) const UI_HINT_UNKNOWN: i32 = 0;
pub(crate) const UI_HINT_RECEIVER: i32 = 1;
pub(crate) const UI_HINT_SENDER: i32 = 2;
pub(crate) const UI_HINT_BOTH: i32 = 3;

/// One Function Block an endpoint declares, whether a virtual device publishes it or the
/// service reports it: its fields and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The block's fields, its number included.
    pub info: FunctionBlockInfo,
    /// Its name, if it has one.
    pub name: Option<String>,
}

/// A field that holds a value the spec reserves, which the service has no number for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeclaredField {
    /// The block number.
    BlockNumber,
    /// The direction.
    Direction,
    /// The MIDI 1.0 port status.
    Midi1,
    /// The first Group.
    FirstGroup,
    /// The number of Groups spanned.
    GroupSpan,
    /// The MIDI-CI message version.
    CiVersion,
    /// The number of Function Blocks the endpoint declares.
    BlockCount,
}

impl core::fmt::Display for DeclaredField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::BlockNumber => "block number",
            Self::Direction => "direction",
            Self::Midi1 => "MIDI 1.0 port status",
            Self::FirstGroup => "first Group",
            Self::GroupSpan => "Groups spanned",
            Self::CiVersion => "MIDI-CI version",
            Self::BlockCount => "Function Block count",
        })
    }
}

/// Why the service cannot be given what a virtual device declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// A field holds a value the spec reserves.
    #[error("a declared {0} is a reserved value")]
    Reserved(DeclaredField),
    /// The endpoint declares a count of Function Blocks other than the blocks it is given.
    #[error("the endpoint declares {declared} Function Blocks and is given {given}")]
    BlockCount {
        /// The count the endpoint's info declares.
        declared: u8,
        /// The blocks given.
        given: usize,
    },
}

/// One Function Block as the service's properties take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlatformBlock {
    pub(crate) number: u8,
    /// Empty for a block with no name.
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) direction: i32,
    pub(crate) ui_hint: i32,
    pub(crate) midi1: i32,
    pub(crate) first_group: u8,
    pub(crate) groups: u8,
    /// `0` for no MIDI-CI.
    pub(crate) ci_version: u8,
    pub(crate) max_sysex8_streams: u8,
}

/// The endpoint's declared info as the service's factory takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlatformInfo {
    pub(crate) midi1: bool,
    pub(crate) midi2: bool,
    pub(crate) rx_jr: bool,
    pub(crate) tx_jr: bool,
    pub(crate) static_blocks: bool,
    pub(crate) block_count: u8,
    pub(crate) version_major: u8,
    pub(crate) version_minor: u8,
}

/// The endpoint info and its blocks as the service takes them, the blocks in the order given.
///
/// # Errors
///
/// [`Refusal`] for the first reserved field, and for a declared count that is not the number
/// of blocks.
pub(crate) fn platform(
    info: &EndpointInfoNotification,
    blocks: &[Block],
) -> Result<(PlatformInfo, Vec<PlatformBlock>), Refusal> {
    let declared = info
        .function_blocks
        .count()
        .ok_or(Refusal::Reserved(DeclaredField::BlockCount))?
        .get();
    if usize::from(declared) != blocks.len() {
        return Err(Refusal::BlockCount {
            declared,
            given: blocks.len(),
        });
    }
    let blocks = blocks
        .iter()
        .map(platform_block)
        .collect::<Result<_, _>>()?;
    let info = PlatformInfo {
        midi1: info.midi1,
        midi2: info.midi2,
        rx_jr: info.rx_jr,
        tx_jr: info.tx_jr,
        static_blocks: info.static_function_blocks,
        block_count: declared,
        version_major: info.version.major,
        version_minor: info.version.minor,
    };
    Ok((info, blocks))
}

fn platform_block(block: &Block) -> Result<PlatformBlock, Refusal> {
    let info = &block.info;
    let reserved = Refusal::Reserved;
    let number = info
        .id
        .value()
        .ok_or(reserved(DeclaredField::BlockNumber))?
        .get();
    let direction = match info.direction {
        BlockDirection::Input => DIRECTION_INPUT,
        BlockDirection::Output => DIRECTION_OUTPUT,
        BlockDirection::Bidirectional => DIRECTION_BIDIRECTIONAL,
        BlockDirection::Reserved(_) => return Err(reserved(DeclaredField::Direction)),
    };
    let midi1 = match info.midi1 {
        Midi1Port::No => MIDI1_NOT_A_PORT,
        Midi1Port::Unrestricted => MIDI1_UNRESTRICTED,
        Midi1Port::Restricted => MIDI1_RESTRICTED,
        Midi1Port::Reserved(_) => return Err(reserved(DeclaredField::Midi1)),
    };
    let ui_hint = match info.ui_hint {
        UiHint::Unknown => UI_HINT_UNKNOWN,
        UiHint::Receiver => UI_HINT_RECEIVER,
        UiHint::Sender => UI_HINT_SENDER,
        UiHint::Both => UI_HINT_BOTH,
    };
    let ci_version = match info.midi_ci_version {
        None => 0,
        Some(version) => version
            .value()
            .ok_or(reserved(DeclaredField::CiVersion))?
            .get(),
    };
    Ok(PlatformBlock {
        number,
        name: block.name.clone().unwrap_or_default(),
        active: info.active,
        direction,
        ui_hint,
        midi1,
        first_group: info
            .first_group
            .value()
            .ok_or(reserved(DeclaredField::FirstGroup))?
            .get(),
        groups: info
            .groups_spanned
            .value()
            .ok_or(reserved(DeclaredField::GroupSpan))?
            .get(),
        ci_version,
        max_sysex8_streams: info.max_sysex8_streams.get(),
    })
}

#[cfg(test)]
mod tests;
