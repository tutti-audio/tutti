//! A Function Block as `MIDIUMPMutableFunctionBlock` is created: the spec's
//! fields as the platform's numbers, and the blocks CoreMIDI cannot carry.
//!
//! CoreMIDI numbers the blocks of an endpoint itself, in registration order, and
//! its block type has no MIDI-CI version. A [`FunctionBlockInfo`] whose number is
//! not its position, or that declares a MIDI-CI version, is refused by name
//! ([`BlockRefusal`]), never registered without it. The numbers other apps read are
//! the ones CoreMIDI gave, which `UmpEndpoint::blocks` reads back.

use tutti_ump::{BlockDirection, FunctionBlockInfo, Midi1Port, UiHint};

/// `MIDIUMPFunctionBlockDirection` of `MIDIMessages.h`: the spec's direction field
/// (M2-104 §7.1.8) `0b01` input, `0b10` output, `0b11` bidirectional.
pub(crate) const DIRECTION_INPUT: i32 = 1;
pub(crate) const DIRECTION_OUTPUT: i32 = 2;
pub(crate) const DIRECTION_BIDIRECTIONAL: i32 = 3;

/// `MIDIUMPFunctionBlockMIDI1Info`: the spec's MIDI 1.0 port field.
pub(crate) const MIDI1_NOT_A_PORT: i32 = 0;
pub(crate) const MIDI1_UNRESTRICTED: i32 = 1;
pub(crate) const MIDI1_RESTRICTED: i32 = 2;

/// `MIDIUMPFunctionBlockUIHint`: the spec's UI hint field.
pub(crate) const UI_HINT_UNKNOWN: i32 = 0;
pub(crate) const UI_HINT_RECEIVER: i32 = 1;
pub(crate) const UI_HINT_SENDER: i32 = 2;
pub(crate) const UI_HINT_BOTH: i32 = 3;

/// The largest value of a `MIDIUInteger7`, the type of the groups spanned and of
/// the SysEx 8 streams.
const MAX_UINTEGER7: u8 = 0x7F;

/// A field of a Function Block that holds a reserved value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockField {
    /// The block number.
    Id,
    /// The direction.
    Direction,
    /// The MIDI 1.0 port status.
    Midi1,
    /// The first Group.
    FirstGroup,
    /// The number of Groups spanned.
    GroupSpan,
}

impl core::fmt::Display for BlockField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Id => "block number",
            Self::Direction => "direction",
            Self::Midi1 => "MIDI 1.0 port status",
            Self::FirstGroup => "first Group",
            Self::GroupSpan => "Groups spanned",
        })
    }
}

/// Why a Function Block cannot be created on a CoreMIDI endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockRefusal {
    /// A field holds a value the spec reserves, which has no CoreMIDI value.
    #[error("a Function Block's {0} is a reserved value")]
    Reserved(BlockField),
    /// CoreMIDI numbers an endpoint's blocks by position, so block `id` is
    /// the `id`th block and the blocks' numbers run 0, 1, 2 without a gap.
    #[error("CoreMIDI numbers blocks by position: block {id} is at position {position}")]
    IdIsNotPosition {
        /// The block's number.
        id: u8,
        /// Its position among the blocks, ordered by number.
        position: usize,
    },
    /// `MIDIUMPMutableFunctionBlock` has no MIDI-CI version, so a block that
    /// declares one cannot say so.
    #[error("CoreMIDI cannot declare MIDI-CI version {version} on block {id}")]
    MidiCiVersion {
        /// The block's number.
        id: u8,
        /// The MIDI-CI message version it declares.
        version: u8,
    },
    /// The number of SysEx 8 streams is past the 7 bits CoreMIDI holds.
    #[error("block {id} declares {streams} SysEx 8 streams, and CoreMIDI holds at most 127")]
    Sysex8Streams {
        /// The block's number.
        id: u8,
        /// The streams it declares.
        streams: u8,
    },
}

/// One Function Block as `initWithName:direction:firstGroup:totalGroupsSpanned:
/// maxSysEx8Streams:MIDI1Info:UIHint:isEnabled:` takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlatformBlock {
    /// `name`: empty for a block with none.
    pub(crate) name: String,
    /// `direction`, `kMIDIUMPFunctionBlockDirection*`.
    pub(crate) direction: i32,
    /// `firstGroup`.
    pub(crate) first_group: u8,
    /// `totalGroupsSpanned`.
    pub(crate) groups_spanned: u8,
    /// `maxSysEx8Streams`.
    pub(crate) max_sysex8_streams: u8,
    /// `MIDI1Info`, `kMIDIUMPFunctionBlockMIDI1Info*`.
    pub(crate) midi1: i32,
    /// `UIHint`, `kMIDIUMPFunctionBlockUIHint*`.
    pub(crate) ui_hint: i32,
    /// `isEnabled`: the spec's active flag.
    pub(crate) enabled: bool,
}

/// The blocks as the platform creates them, in the order CoreMIDI numbers them.
///
/// # Errors
///
/// [`BlockRefusal`] for the first block (in number order) CoreMIDI cannot carry.
pub(crate) fn platform_blocks(
    blocks: &[(FunctionBlockInfo, Option<&str>)],
) -> Result<Vec<PlatformBlock>, BlockRefusal> {
    let mut numbered = Vec::with_capacity(blocks.len());
    for (info, name) in blocks {
        let id = info
            .id
            .value()
            .ok_or(BlockRefusal::Reserved(BlockField::Id))?;
        numbered.push((id.get(), info, *name));
    }
    numbered.sort_by_key(|(id, ..)| *id);
    numbered
        .into_iter()
        .enumerate()
        .map(|(position, (id, info, name))| {
            if usize::from(id) != position {
                return Err(BlockRefusal::IdIsNotPosition { id, position });
            }
            platform_block(id, info, name)
        })
        .collect()
}

fn platform_block(
    id: u8,
    info: &FunctionBlockInfo,
    name: Option<&str>,
) -> Result<PlatformBlock, BlockRefusal> {
    if info.midi_ci_version != 0 {
        return Err(BlockRefusal::MidiCiVersion {
            id,
            version: info.midi_ci_version,
        });
    }
    if info.max_sysex8_streams.get() > MAX_UINTEGER7 {
        return Err(BlockRefusal::Sysex8Streams {
            id,
            streams: info.max_sysex8_streams.get(),
        });
    }
    let direction = match info.direction {
        BlockDirection::Input => DIRECTION_INPUT,
        BlockDirection::Output => DIRECTION_OUTPUT,
        BlockDirection::Bidirectional => DIRECTION_BIDIRECTIONAL,
        _ => return Err(BlockRefusal::Reserved(BlockField::Direction)),
    };
    let midi1 = match info.midi1 {
        Midi1Port::No => MIDI1_NOT_A_PORT,
        Midi1Port::Unrestricted => MIDI1_UNRESTRICTED,
        Midi1Port::Restricted => MIDI1_RESTRICTED,
        _ => return Err(BlockRefusal::Reserved(BlockField::Midi1)),
    };
    let ui_hint = match info.ui_hint {
        UiHint::Unknown => UI_HINT_UNKNOWN,
        UiHint::Receiver => UI_HINT_RECEIVER,
        UiHint::Sender => UI_HINT_SENDER,
        UiHint::Both => UI_HINT_BOTH,
    };
    let first_group = info
        .first_group
        .value()
        .ok_or(BlockRefusal::Reserved(BlockField::FirstGroup))?;
    let groups = info
        .groups_spanned
        .value()
        .ok_or(BlockRefusal::Reserved(BlockField::GroupSpan))?;
    Ok(PlatformBlock {
        name: name.unwrap_or_default().to_owned(),
        direction,
        first_group: first_group.get(),
        groups_spanned: groups.get(),
        max_sysex8_streams: info.max_sysex8_streams.get(),
        midi1,
        ui_hint,
        enabled: info.active,
    })
}

/// The numbers CoreMIDI gave the blocks, in the order they were registered, from the
/// `(name, number)` pairs it lists: by name when every registered name is non-empty
/// and distinct, since a listing need not keep the order, and otherwise in the
/// listed order, which the count guards. `None` when the listing is not of the
/// registered blocks: another count, or a name that is not listed exactly once.
pub(crate) fn numbers_in_registered_order(
    registered: &[PlatformBlock],
    listed: &[(String, u8)],
) -> Option<Vec<u8>> {
    if registered.len() != listed.len() {
        return None;
    }
    let names: Vec<&str> = registered.iter().map(|b| b.name.as_str()).collect();
    let distinct = names
        .iter()
        .enumerate()
        .all(|(i, name)| !name.is_empty() && !names[..i].contains(name));
    if !distinct {
        return Some(listed.iter().map(|(_, number)| *number).collect());
    }
    names
        .iter()
        .map(|name| {
            let mut found = listed.iter().filter(|(listed, _)| listed == name);
            match (found.next(), found.next()) {
                (Some((_, number)), None) => Some(*number),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
