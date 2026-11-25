//! Parameter-to-MIDI mapping: the CC-to-parameter routing table
//! (`kAudioUnitProperty_AllParameterMIDIMappings` and its three siblings).
//!
//! The host writes the table into the plugin and the plugin routes internally
//! during render, so there is no per-block routing pass here: once a mapping is
//! installed, the CC arrives through [`send_midi`](crate::processor::AuProcessor::send_midi)
//! and the AU moves its own parameter. This module reads, adds, removes and
//! replaces the table and drives "learn" mode.
//!
//! The family is implemented by instrument and music-effect units only; any
//! other unit answers `kAudioUnitErr_InvalidProperty` (-10879).
//!
//! The deprecated read-only `kAudioUnitProperty_MIDIControlMapping` (17) is not
//! supported.
//!
//! # AU deviations from the header
//!
//! 1. The table cannot be cleared by writing an empty one: a NULL/0-size write
//!    answers `kAudioUnitErr_InvalidPropertyValue` (-10851) and a non-NULL/0-length
//!    write answers `paramErr` (-50), both leaving the table unchanged. So
//!    [`set_all`] with an empty slice routes through *remove* instead.
//! 2. `HotMap` reads `noErr` with an all-zero struct when nothing is armed, so
//!    [`hot_map_pending`] decides on `status == 0` rather than on the `Result`.
//! 3. Nothing is validated: a mapping naming an unknown parameter id or scope is
//!    accepted with `noErr` and appears in the read-back table.
//! 4. The table comes back in no particular order; compare as sets.

use std::mem::size_of;
use tutti_au_ffi::property::{
    AddParameterMidiMapping, AllParameterMidiMappings, HotMapParameterMidiMapping,
    RemoveParameterMidiMapping,
};
use tutti_au_ffi::{Element, Property, Scope};

use tutti_plugin_types::{MidiAssignment, MidiSource};
use tutti_ump::{MidiChannel, U7};
use tutti_units::ParamId;

use crate::error::Result;
use tutti_au_ffi::Unit;

/// Apple's `AUParameterMIDIMapping` from `AudioUnitProperties.h`.
///
/// Hand-declared because `coreaudio-sys` does not export it. Layout pinned by
/// `tests::the_mapping_struct_matches_the_c_abi`: 32 bytes, align 4, `mStatus`
/// at offset 24 and `reserved3` at 28. The three `reserved*` fields must be zero;
/// [`AuMidiMapping::to_raw`] is the only constructor and zeroes them.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AuParameterMidiMappingRaw {
    scope: u32,
    element: u32,
    parameter_id: u32,
    flags: u32,
    sub_range_min: f32,
    sub_range_max: f32,
    status: u8,
    data1: u8,
    reserved1: u8,
    reserved2: u8,
    reserved3: u32,
}

/// The record's size in bytes.
// `AUParameterMIDIMappingFlags`, `1 << 0` .. `1 << 5`.
/// Match the message on any MIDI channel, ignoring the status byte's low nibble.
const K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL: u32 = 1 << 0;
/// For a note command, match any note number.
const K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE: u32 = 1 << 1;
/// Confine the controller to `mSubRangeMin..=mSubRangeMax` of the parameter.
const K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE: u32 = 1 << 2;
/// The message flips the parameter rather than setting it from the value.
const K_AU_PARAMETER_MIDI_MAPPING_TOGGLE: u32 = 1 << 3;
/// The parameter takes only two states, thresholded at controller 64/65.
const K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR: u32 = 1 << 4;
/// Which end of the controller is the parameter's "on" state. Only meaningful
/// with [`K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR`] set.
const K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON: u32 = 1 << 5;

const RAW_SIZE: usize = size_of::<AuParameterMidiMappingRaw>();

impl AuParameterMidiMappingRaw {
    /// The record in the byte order and layout of the C struct, written field by
    /// field at the offsets the ABI test pins.
    fn to_bytes(self) -> [u8; RAW_SIZE] {
        let mut b = [0u8; RAW_SIZE];
        b[0..4].copy_from_slice(&self.scope.to_ne_bytes());
        b[4..8].copy_from_slice(&self.element.to_ne_bytes());
        b[8..12].copy_from_slice(&self.parameter_id.to_ne_bytes());
        b[12..16].copy_from_slice(&self.flags.to_ne_bytes());
        b[16..20].copy_from_slice(&self.sub_range_min.to_ne_bytes());
        b[20..24].copy_from_slice(&self.sub_range_max.to_ne_bytes());
        b[24] = self.status;
        b[25] = self.data1;
        b[26] = self.reserved1;
        b[27] = self.reserved2;
        b[28..32].copy_from_slice(&self.reserved3.to_ne_bytes());
        b
    }

    /// The inverse of [`Self::to_bytes`].
    fn from_bytes(b: &[u8; RAW_SIZE]) -> Self {
        let u32_at = |i: usize| u32::from_ne_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let f32_at = |i: usize| f32::from_bits(u32_at(i));
        Self {
            scope: u32_at(0),
            element: u32_at(4),
            parameter_id: u32_at(8),
            flags: u32_at(12),
            sub_range_min: f32_at(16),
            sub_range_max: f32_at(20),
            status: b[24],
            data1: b[25],
            reserved1: b[26],
            reserved2: b[27],
            reserved3: u32_at(28),
        }
    }
}

/// What MIDI message a mapping triggers on, decoded from `mStatus` / `mData1`.
///
/// `mData1` is a controller id for `ControlChange`, a note number for the three
/// note commands, a patch number for `ProgramChange`, and unused for the two
/// pressure/bend forms, which is why this is a typed enum rather than a
/// `(u8, u8)` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiTrigger {
    /// `0x8n` — note off. `mData1` is the note number.
    NoteOff {
        /// Note number, or ignored when [`AuMidiMapping::any_note`] is set.
        note: u8,
    },
    /// `0x9n` — note on. `mData1` is the note number.
    NoteOn {
        /// Note number, or ignored when [`AuMidiMapping::any_note`] is set.
        note: u8,
    },
    /// `0xAn` — polyphonic key pressure. `mData1` is the note number.
    KeyPressure {
        /// Note number, or ignored when [`AuMidiMapping::any_note`] is set.
        note: u8,
    },
    /// `0xBn` — control change. `mData1` is the controller id (CC 1 = mod wheel,
    /// CC 2 = breath, CC 11 = expression, CC 64 = sustain).
    ControlChange {
        /// Controller number, 0..=127.
        controller: u8,
    },
    /// `0xCn` — program change. `mData1` is the patch number.
    ProgramChange {
        /// Patch number.
        patch: u8,
    },
    /// `0xDn` channel pressure. `mData1` is unused; no payload.
    ChannelPressure,
    /// `0xEn` — pitch bend. `mData1` is **unused**, as for
    /// [`ChannelPressure`](Self::ChannelPressure).
    PitchBend,
    /// A status byte whose high nibble is not one of the seven above.
    /// Decoded from a value an AU supplied, so it must not make
    /// `AuMidiMapping::from_raw` panic or claim "CC 0".
    Other {
        /// The raw status byte, high nibble and channel both.
        status: u8,
        /// The raw `mData1`, uninterpreted.
        data1: u8,
    },
}

impl MidiTrigger {
    /// The status high nibble this trigger occupies (`0x80`, `0x90`, …).
    fn status_nibble(&self) -> u8 {
        match self {
            Self::NoteOff { .. } => 0x80,
            Self::NoteOn { .. } => 0x90,
            Self::KeyPressure { .. } => 0xA0,
            Self::ControlChange { .. } => 0xB0,
            Self::ProgramChange { .. } => 0xC0,
            Self::ChannelPressure => 0xD0,
            Self::PitchBend => 0xE0,
            Self::Other { status, .. } => *status & 0xF0,
        }
    }

    /// The `mData1` byte this trigger carries, `0` for the two unused forms.
    fn data1(&self) -> u8 {
        match self {
            Self::NoteOff { note } | Self::NoteOn { note } | Self::KeyPressure { note } => *note,
            Self::ControlChange { controller } => *controller,
            Self::ProgramChange { patch } => *patch,
            Self::ChannelPressure | Self::PitchBend => 0,
            Self::Other { data1, .. } => *data1,
        }
    }

    /// Decodes a raw `(mStatus, mData1)` pair.
    fn from_raw(status: u8, data1: u8) -> Self {
        match status & 0xF0 {
            0x80 => Self::NoteOff { note: data1 },
            0x90 => Self::NoteOn { note: data1 },
            0xA0 => Self::KeyPressure { note: data1 },
            0xB0 => Self::ControlChange { controller: data1 },
            0xC0 => Self::ProgramChange { patch: data1 },
            0xD0 => Self::ChannelPressure,
            0xE0 => Self::PitchBend,
            _ => Self::Other { status, data1 },
        }
    }

    /// Whether the note-number fields are meaningful for this trigger, i.e.
    /// whether [`kAUParameterMIDIMapping_AnyNoteFlag`](K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE)
    /// can apply (note on, note off and polyphonic pressure only).
    pub fn is_note_command(&self) -> bool {
        matches!(
            self,
            Self::NoteOff { .. } | Self::NoteOn { .. } | Self::KeyPressure { .. }
        )
    }
}

/// One parameter-to-MIDI mapping: which MIDI message drives which parameter, and
/// how the controller's value is interpreted on the way.
///
/// The flags word is five named fields because three of them change what a
/// controller value *means*: [`sub_range`](Self::sub_range) confines the
/// parameter to a slice of its range, [`toggle`](Self::toggle) flips it, and
/// [`bipolar`](Self::bipolar) thresholds it at 64/65 with
/// [`bipolar_on`](Self::bipolar_on) choosing the "on" end. The two "any" flags
/// widen what matches: [`any_channel`](Self::any_channel) ignores the channel,
/// [`any_note`](Self::any_note) the note number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AuMidiMapping {
    /// The parameter's scope. `mScope`, `mElement` and `mParameterID` identify a
    /// mapping for remove.
    pub scope: Scope,
    /// The parameter's element within `scope`.
    pub element: Element,
    /// The `AudioUnitParameterID` this mapping drives. The AU does not validate
    /// it; cross-check against
    /// [`get_parameter_list`](crate::instance::AuLoaded::get_parameter_list).
    pub parameter_id: u32,
    /// The MIDI message that triggers this mapping.
    pub trigger: MidiTrigger,
    /// MIDI channel, `0..=15`, from the low nibble of `mStatus`. Ignored when
    /// [`any_channel`](Self::any_channel) is set, but preserved on the round trip.
    pub channel: u8,
    /// `kAUParameterMIDIMapping_AnyChannelFlag`: match this message on **any**
    /// channel, ignoring [`channel`](Self::channel).
    pub any_channel: bool,
    /// `kAUParameterMIDIMapping_AnyNoteFlag`: for a note command, match **any**
    /// note number. Only meaningful when
    /// [`MidiTrigger::is_note_command`] holds.
    pub any_note: bool,
    /// `kAUParameterMIDIMapping_SubRange`: confine the controller to
    /// `[min, max]` of the parameter's range. `None` means the whole range;
    /// `Some` values are in the **parameter's own units**, not normalized.
    pub sub_range: Option<(f32, f32)>,
    /// `kAUParameterMIDIMapping_Toggle`: the mapped message *flips* the
    /// parameter rather than setting it from the controller value. For boolean
    /// parameters.
    pub toggle: bool,
    /// `kAUParameterMIDIMapping_Bipolar`: the parameter takes only two states,
    /// with the controller thresholded at 64/65.
    pub bipolar: bool,
    /// `kAUParameterMIDIMapping_Bipolar_On`: which end of the controller maps to
    /// the parameter's "on" state. Ignored by the AU with [`bipolar`](Self::bipolar)
    /// clear.
    pub bipolar_on: bool,
}

impl AuMidiMapping {
    /// A control-change message on one channel driving a parameter's full range:
    /// global scope, element 0, no flags.
    pub fn control_change(parameter_id: u32, channel: u8, controller: u8) -> Self {
        Self {
            scope: Scope::GLOBAL,
            element: Element::ZERO,
            parameter_id,
            trigger: MidiTrigger::ControlChange { controller },
            channel,
            any_channel: false,
            any_note: false,
            sub_range: None,
            toggle: false,
            bipolar: false,
            bipolar_on: false,
        }
    }

    /// The same, matching the controller on **every** channel; sets
    /// [`channel`](Self::channel) to 0, which the AU ignores.
    pub fn control_change_any_channel(parameter_id: u32, controller: u8) -> Self {
        Self {
            any_channel: true,
            ..Self::control_change(parameter_id, 0, controller)
        }
    }

    /// Decodes a mapping the AU handed back.
    ///
    /// Total: every bit pattern decodes. An unrecognised status byte becomes
    /// [`MidiTrigger::Other`] and an out-of-range `scope` is preserved verbatim.
    pub(crate) fn from_raw(raw: &AuParameterMidiMappingRaw) -> Self {
        let sub_range = (raw.flags & K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE != 0)
            .then_some((raw.sub_range_min, raw.sub_range_max));
        Self {
            scope: Scope::from_raw(raw.scope),
            element: Element(raw.element),
            parameter_id: raw.parameter_id,
            trigger: MidiTrigger::from_raw(raw.status, raw.data1),
            channel: raw.status & 0x0F,
            any_channel: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL != 0,
            any_note: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE != 0,
            sub_range,
            toggle: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_TOGGLE != 0,
            bipolar: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR != 0,
            bipolar_on: raw.flags & K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON != 0,
        }
    }

    /// Encodes for the AU. The `reserved*` fields are zeroed, as the header
    /// demands; the sub-range values are written only when
    /// [`sub_range`](Self::sub_range) is `Some`.
    pub(crate) fn to_raw(self) -> AuParameterMidiMappingRaw {
        let mut flags = 0u32;
        if self.any_channel {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL;
        }
        if self.any_note {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE;
        }
        if self.sub_range.is_some() {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE;
        }
        if self.toggle {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_TOGGLE;
        }
        if self.bipolar {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR;
        }
        if self.bipolar_on {
            flags |= K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON;
        }
        let (lo, hi) = self.sub_range.unwrap_or((0.0, 0.0));
        AuParameterMidiMappingRaw {
            scope: self.scope.raw(),
            element: self.element.0,
            parameter_id: self.parameter_id,
            flags,
            sub_range_min: lo,
            sub_range_max: hi,
            // The channel lives in the status byte's low nibble, masked so a
            // caller passing 16..=255 cannot corrupt the command nibble above it.
            status: self.trigger.status_nibble() | (self.channel & 0x0F),
            data1: self.trigger.data1(),
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
        }
    }

    /// True when this mapping names the same `(scope, element, parameter_id)` as
    /// `other`: the triple the AU matches on for remove and replace (one mapping
    /// per parameter).
    pub fn targets_same_parameter(&self, other: &Self) -> bool {
        self.scope == other.scope
            && self.element == other.element
            && self.parameter_id == other.parameter_id
    }
}

/// The mappings of `table` that drive a global-scope parameter from a control
/// surface message (Control Change, pressure, pitch bend, program change), as
/// the assignments a plugin declares ([`MidiAssignment`]).
pub(crate) fn declared(table: &[AuMidiMapping]) -> Vec<MidiAssignment> {
    table
        .iter()
        .filter(|m| m.scope == Scope::GLOBAL && m.element == Element::ZERO)
        .filter_map(|m| {
            let source = match m.trigger {
                MidiTrigger::ControlChange { controller } => {
                    MidiSource::ControlChange(U7::new(controller)?)
                }
                MidiTrigger::ChannelPressure => MidiSource::ChannelPressure,
                MidiTrigger::PitchBend => MidiSource::PitchBend,
                MidiTrigger::ProgramChange { .. } => MidiSource::ProgramChange,
                MidiTrigger::KeyPressure { .. } => MidiSource::PolyPressure,
                _ => return None,
            };
            Some(MidiAssignment {
                channel: if m.any_channel {
                    None
                } else {
                    MidiChannel::new(m.channel)
                },
                source,
                param: ParamId::new(m.parameter_id),
            })
        })
        .collect()
}

impl AuMidiMapping {
    /// The entry that drives `assignment.param` from its message: global scope,
    /// element 0, no flags but the one that matches any channel (or any key for
    /// polyphonic pressure) where the assignment names none. `None` for a
    /// message the unit's table has no trigger for.
    fn of_assignment(assignment: &MidiAssignment) -> Option<Self> {
        let trigger = match assignment.source {
            MidiSource::ControlChange(controller) => MidiTrigger::ControlChange {
                controller: controller.get(),
            },
            MidiSource::ChannelPressure => MidiTrigger::ChannelPressure,
            MidiSource::PitchBend => MidiTrigger::PitchBend,
            MidiSource::ProgramChange => MidiTrigger::ProgramChange { patch: 0 },
            MidiSource::PolyPressure => MidiTrigger::KeyPressure { note: 0 },
            _ => return None,
        };
        Some(Self {
            trigger,
            channel: assignment.channel.map_or(0, MidiChannel::get),
            any_channel: assignment.channel.is_none(),
            any_note: matches!(trigger, MidiTrigger::KeyPressure { .. }),
            ..Self::control_change(assignment.param.get(), 0, 0)
        })
    }
}

/// One write to a unit's mapping table.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    /// Removes whatever holds the triple of each entry.
    Remove(Vec<AuMidiMapping>),
    /// Adds the entries, each replacing the one for its parameter.
    Add(Vec<AuMidiMapping>),
}

/// The writes that move a unit's table to a new app set, and the writes that
/// move it back from any point of them.
#[derive(Debug, PartialEq)]
struct Plan {
    forward: Vec<Op>,
    undo: Vec<Op>,
    /// The plugin's entries the new set replaces.
    replaced: Vec<AuMidiMapping>,
}

/// How a [`AppMap::set`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    /// The unit holds the entries.
    Applied,
    /// The unit refused, and its table is as it was.
    Refused,
    /// The unit refused partway and the writes that restore its table failed
    /// too: it may hold part of the set, and the plugin's own entries the set
    /// replaced may be missing, until the next [`AppMap::set`] converges.
    /// The failed undo is the caller's to log.
    Unrestored,
}

/// The app's entries as written into a unit's table, and the entries of the
/// plugin's own that they replaced.
///
/// The unit keeps one mapping per `(scope, element, parameter_id)`: adding an
/// entry replaces the plugin's for that parameter, and removing it removes
/// whatever holds the triple. So the plugin's replaced entries are kept here and
/// put back when the app's set changes, and a plugin entry no app entry names is
/// never written.
#[derive(Debug, Default)]
pub(crate) struct AppMap {
    applied: Vec<AuMidiMapping>,
    replaced: Vec<AuMidiMapping>,
}

impl AppMap {
    /// The plugin's own table: `table`, the unit's, without the app's entries and
    /// with the plugin's entries they replaced.
    pub(crate) fn plugins_own(&self, table: &[AuMidiMapping]) -> Vec<AuMidiMapping> {
        let mut own: Vec<AuMidiMapping> = table
            .iter()
            .filter(|held| !self.applied.iter().any(|a| a.targets_same_parameter(held)))
            .copied()
            .collect();
        add_missing(&mut own, &self.replaced);
        own
    }

    /// The writes from the unit's `table` to `entries` in place of the last set:
    /// the last set is removed, the plugin's entries it replaced come back, and
    /// `entries` are added. The undo removes every parameter touched and writes
    /// back what `table` held for it.
    fn plan(&self, table: &[AuMidiMapping], entries: &[AuMidiMapping]) -> Plan {
        let own = self.plugins_own(table);
        let replaced: Vec<AuMidiMapping> = own
            .iter()
            .filter(|held| entries.iter().any(|e| e.targets_same_parameter(held)))
            .copied()
            .collect();
        let touched: Vec<AuMidiMapping> = self
            .applied
            .iter()
            .chain(&self.replaced)
            .chain(entries)
            .copied()
            .collect();
        let before = table
            .iter()
            .filter(|held| touched.iter().any(|t| t.targets_same_parameter(held)))
            .copied()
            .collect();
        Plan {
            forward: vec![
                Op::Remove(self.applied.clone()),
                Op::Add(self.replaced.clone()),
                Op::Add(entries.to_vec()),
            ],
            undo: vec![Op::Remove(touched), Op::Add(before)],
            replaced,
        }
    }

    /// What a set that ended `written` leaves this map holding. An undo that
    /// failed leaves the unit holding any part of the new set and of the last, so
    /// the map holds all of both (and every plugin entry either replaced): the
    /// next set removes them all, which the unit ignores where absent, puts the
    /// replaced entries back and writes its own, so the table converges to the
    /// plugin's own plus that set.
    fn record(
        &mut self,
        written: Written,
        entries: Vec<AuMidiMapping>,
        replaced: Vec<AuMidiMapping>,
    ) {
        match written {
            Written::Applied => {
                self.applied = entries;
                self.replaced = replaced;
            }
            Written::Unrestored => {
                add_missing(&mut self.applied, &entries);
                add_missing(&mut self.replaced, &replaced);
            }
            Written::Refused => {}
        }
    }

    /// Replaces the app's entries in the unit's table with `app`. A refusal
    /// partway puts the table back as it was; a set naming a message the table
    /// has no trigger for is refused whole, before anything is written.
    ///
    /// When the undo of a refusal fails too ([`Written::Unrestored`]) the unit
    /// may hold part of the set until the next call, whose writes converge it:
    /// the answer is still a refusal, so the host's node translates the set as
    /// well and a mapped message can be applied twice in the meantime. `Plugin`
    /// would be wrong, as the unit may hold only some of the entries.
    pub(crate) fn set(&mut self, unit: &Unit, app: &[MidiAssignment]) -> Written {
        let Some(entries) = app
            .iter()
            .map(AuMidiMapping::of_assignment)
            .collect::<Option<Vec<AuMidiMapping>>>()
        else {
            return Written::Refused;
        };
        // A unit without the mapping family refuses the read: nothing is written.
        let Ok(table) = all(unit) else {
            return Written::Refused;
        };
        let plan = self.plan(&table, &entries);
        let written = transact(&plan, |op| {
            match op {
                Op::Remove(m) => remove(unit, m),
                Op::Add(m) => add(unit, m),
            }
            .is_ok()
        });
        self.record(written, entries, plan.replaced);
        written
    }
}

/// Appends to `held` the entries of `more` whose parameter it holds none for.
fn add_missing(held: &mut Vec<AuMidiMapping>, more: &[AuMidiMapping]) {
    for entry in more {
        if !held.iter().any(|h| h.targets_same_parameter(entry)) {
            held.push(*entry);
        }
    }
}

/// Runs the writes of `plan` through `write` (which says whether the unit took
/// one), and on a refusal runs the undo, every write of it.
fn transact(plan: &Plan, mut write: impl FnMut(&Op) -> bool) -> Written {
    if plan.forward.iter().all(&mut write) {
        return Written::Applied;
    }
    // Every undo write runs, whether or not an earlier one was refused.
    let mut restored = true;
    for op in &plan.undo {
        restored &= write(op);
    }
    if restored {
        Written::Refused
    } else {
        Written::Unrestored
    }
}

/// Reads the AU's whole mapping table.
///
/// Returns an empty `Vec` when the AU has no mappings installed, distinct from
/// not implementing the property, which is an error.
///
/// # Errors
/// [`AuError::OsStatus`](crate::error::AuError::OsStatus) with
/// `kAudioUnitErr_InvalidProperty` (-10879) from a unit that does not implement
/// the property.
pub(crate) fn all(unit: &Unit) -> Result<Vec<AuMidiMapping>> {
    let bytes = unit.get_bytes::<AllParameterMidiMappings>(Scope::GLOBAL, Element::ZERO)?;
    Ok(decode_table(&bytes))
}

/// Decodes a byte buffer of `AUParameterMIDIMapping` structs.
///
/// A buffer whose length is not a multiple of the struct size is truncated to
/// whole structs rather than rejected.
fn decode_table(bytes: &[u8]) -> Vec<AuMidiMapping> {
    bytes
        .as_chunks::<RAW_SIZE>()
        .0
        .iter()
        .map(|chunk| AuMidiMapping::from_raw(&AuParameterMidiMappingRaw::from_bytes(chunk)))
        .collect()
}

/// Encodes mappings into the flat buffer the property expects.
///
/// Separate from the write calls so the encode is testable without an AU, and so
/// both [`add`] and [`set_all`] share one layout.
fn encode_table(mappings: &[AuMidiMapping]) -> Vec<AuParameterMidiMappingRaw> {
    mappings
        .iter()
        .copied()
        .map(AuMidiMapping::to_raw)
        .collect()
}

/// Writes `mappings` to a property that takes an array of them.
fn write_table<P: Property>(unit: &Unit, mappings: &[AuMidiMapping]) -> Result<()> {
    let bytes: Vec<u8> = encode_table(mappings)
        .into_iter()
        .flat_map(AuParameterMidiMappingRaw::to_bytes)
        .collect();
    Ok(unit.set_bytes::<P>(Scope::GLOBAL, Element::ZERO, &bytes)?)
}

/// Adds `mappings` to whatever the AU already has, replacing any mapping that
/// already targets the same parameter.
///
/// A no-op for an empty slice: a zero-length write is `paramErr` (-50).
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
/// family. A `noErr` return does not prove the mapping is meaningful; verify
/// with [`all`].
pub(crate) fn add(unit: &Unit, mappings: &[AuMidiMapping]) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    write_table::<AddParameterMidiMapping>(unit, mappings)
}

/// Removes the mappings targeting each of `mappings`'
/// `(scope, element, parameter_id)`.
///
/// Only that triple is matched. A mapping that is not installed is silently
/// ignored. An empty slice is a no-op, as in [`add`].
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
/// family.
pub(crate) fn remove(unit: &Unit, mappings: &[AuMidiMapping]) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    write_table::<RemoveParameterMidiMapping>(unit, mappings)
}

/// Replaces the AU's entire mapping table with `mappings`.
///
/// An empty slice reads the table back and [`remove`]s every entry, because a
/// zero-length write does not clear it. That is not atomic: a mapping the plugin
/// adds between the two calls survives the clear.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
/// family. The empty-slice path additionally surfaces the [`all`] read's error.
pub(crate) fn set_all(unit: &Unit, mappings: &[AuMidiMapping]) -> Result<()> {
    if mappings.is_empty() {
        let existing = all(unit)?;
        return remove(unit, &existing);
    }
    write_table::<AllParameterMidiMappings>(unit, mappings)
}

/// Arms "learn" mode: the AU maps the **next MIDI message it sees** to the
/// parameter `mapping` names.
///
/// The AU ignores the trigger and channel of `mapping` and fills them from what
/// arrives. The AU notifies on this property when the mapping completes; watch
/// for it through [`crate::listener`].
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
/// family.
pub(crate) fn arm_hot_map(unit: &Unit, mapping: &AuMidiMapping) -> Result<()> {
    let raw = mapping.to_raw();
    Ok(unit.set_bytes::<HotMapParameterMidiMapping>(
        Scope::GLOBAL,
        Element::ZERO,
        &raw.to_bytes(),
    )?)
}

/// Reads the pending or just-completed hot map.
///
/// `None` means nothing is armed or nothing has arrived yet, decided on
/// `mStatus == 0`: zero is not a valid MIDI status byte, so it cannot collide
/// with a genuine mapping. An AU that errors is also reported as `None`.
pub(crate) fn hot_map(unit: &Unit) -> Option<AuMidiMapping> {
    let bytes = unit
        .get_bytes::<HotMapParameterMidiMapping>(Scope::GLOBAL, Element::ZERO)
        .ok()?;
    let raw = AuParameterMidiMappingRaw::from_bytes(bytes.as_slice().try_into().ok()?);
    // `mStatus == 0` is the only reliable "nothing here" signal.
    (raw.status != 0).then(|| AuMidiMapping::from_raw(&raw))
}

/// Whether the AU currently reports a completed hot map waiting to be read.
pub(crate) fn hot_map_pending(unit: &Unit) -> bool {
    hot_map(unit).is_some()
}

/// Whether this AU implements the parameter-to-MIDI mapping family at all.
///
/// Asks `AudioUnitGetPropertyInfo` for property 41 rather than reading the
/// table, because a unit that implements it answers `noErr` with size 0 when it
/// has no mappings.
pub(crate) fn supports_parameter_midi_mapping(unit: &Unit) -> bool {
    unit.property_info::<AllParameterMidiMappings>(Scope::GLOBAL, Element::ZERO)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit's table under the writes the unit accepts: one entry per parameter.
    fn apply(table: &mut Vec<AuMidiMapping>, op: &Op) {
        match op {
            Op::Remove(ms) => table.retain(|t| !ms.iter().any(|m| m.targets_same_parameter(t))),
            Op::Add(ms) => {
                for m in ms {
                    table.retain(|t| !t.targets_same_parameter(m));
                    table.push(*m);
                }
            }
        }
    }

    fn same_set(a: &[AuMidiMapping], b: &[AuMidiMapping]) -> bool {
        a.len() == b.len() && a.iter().all(|x| b.contains(x))
    }

    /// The table of a unit that declares CC 74 for parameter 900, the app's
    /// entries for 900 (an override) and 901 (a new one).
    fn fixtures() -> (Vec<AuMidiMapping>, AuMidiMapping, AuMidiMapping) {
        (
            vec![AuMidiMapping::control_change(900, 0, 74)],
            AuMidiMapping::control_change(900, 0, 22),
            AuMidiMapping::control_change(901, 2, 21),
        )
    }

    /// Whatever the app's set is, the plugin's own table reads the same, and an
    /// empty set brings the unit's table back to the plugin's.
    #[test]
    fn the_plugins_own_table_is_the_same_before_during_and_after_a_set() {
        let (own, over, extra) = fixtures();
        let mut unit = own.clone();
        let mut app = AppMap::default();
        for entries in [vec![extra], vec![extra, over], vec![]] {
            let plan = app.plan(&unit, &entries);
            assert_eq!(
                transact(&plan, |op| {
                    apply(&mut unit, op);
                    true
                }),
                Written::Applied
            );
            app.applied = entries.clone();
            app.replaced = plan.replaced;
            assert!(same_set(&app.plugins_own(&unit), &own), "{entries:?}");
            let mut held = own.clone();
            if entries.contains(&over) {
                held.clear();
            }
            held.extend(entries);
            assert!(same_set(&unit, &held));
        }
    }

    /// A refusal at any write leaves the unit's table as it was, and the answer
    /// is a refusal; with the undo refused too it is unrestored.
    #[test]
    fn a_refused_write_puts_the_table_back() {
        let (own, over, extra) = fixtures();
        let mut app = AppMap::default();
        let mut unit = own.clone();
        let first = app.plan(&unit, &[extra]);
        transact(&first, |op| {
            apply(&mut unit, op);
            true
        });
        app.applied = vec![extra];
        app.replaced = first.replaced;
        let before = unit.clone();
        let plan = app.plan(&unit, &[extra, over]);
        for failing in 0..plan.forward.len() {
            let mut held = before.clone();
            let mut calls = 0;
            let written = transact(&plan, |op| {
                let refused = calls == failing;
                calls += 1;
                if !refused {
                    apply(&mut held, op);
                }
                !refused
            });
            assert_eq!(written, Written::Refused, "refused at write {failing}");
            assert!(same_set(&held, &before), "refused at write {failing}");
        }
        let written = transact(&plan, |op| {
            let _ = op;
            false
        });
        assert_eq!(written, Written::Unrestored);
    }

    /// After a refusal whose undo failed too, with the unit holding part of the
    /// new set, the next set leaves the unit holding exactly the plugin's own
    /// entries and the new set, and the plugin's own table reads as before.
    #[test]
    fn a_set_after_an_unrestored_one_converges_to_own_and_the_new_set() {
        let (own, over, extra) = fixtures();
        let mut unit = own.clone();
        let mut app = AppMap::default();
        let first = app.plan(&unit, &[extra]);
        transact(&first, |op| {
            apply(&mut unit, op);
            true
        });
        app.record(Written::Applied, vec![extra], first.replaced);

        // The last write lands and is reported refused, and every undo is.
        let entries = vec![extra, over];
        let plan = app.plan(&unit, &entries);
        let mut calls = 0;
        let written = transact(&plan, |op| {
            calls += 1;
            if calls <= plan.forward.len() {
                apply(&mut unit, op);
                calls < plan.forward.len()
            } else {
                false
            }
        });
        assert_eq!(written, Written::Unrestored);
        app.record(written, entries, plan.replaced);
        assert!(same_set(&unit, &[extra, over]), "the unit holds the strays");

        let next = app.plan(&unit, &[extra]);
        assert_eq!(
            transact(&next, |op| {
                apply(&mut unit, op);
                true
            }),
            Written::Applied
        );
        app.record(Written::Applied, vec![extra], next.replaced);
        assert!(same_set(&unit, &[own[0], extra]), "own and the new set");
        assert!(same_set(&app.plugins_own(&unit), &own));
    }

    /// An assignment written into the unit reads back as the same assignment,
    /// on its channel or on any.
    #[test]
    fn an_assignment_becomes_the_entry_that_declares_it_back() {
        let table = [
            MidiAssignment {
                channel: MidiChannel::new(3),
                source: MidiSource::ControlChange(U7::of(74)),
                param: ParamId::new(900),
            },
            MidiAssignment {
                channel: None,
                source: MidiSource::PitchBend,
                param: ParamId::new(901),
            },
        ];
        let entries =
            table.map(|a| AuMidiMapping::of_assignment(&a).expect("each source has a trigger"));
        assert_eq!(declared(&entries), table);
        assert!(entries[1].any_channel && !entries[0].any_channel);
    }

    /// A Control Change mapping is an assignment on its channel, or on any
    /// channel when the flag is set; a note or program trigger is none, and a
    /// mapping that drives another scope is left out.
    #[test]
    fn declared_keeps_the_control_surface_mappings_only() {
        let on_channel = AuMidiMapping::control_change(900, 3, 74);
        let any = AuMidiMapping::control_change_any_channel(901, 1);
        let note = AuMidiMapping {
            trigger: MidiTrigger::NoteOn { note: 60 },
            ..AuMidiMapping::control_change(902, 0, 0)
        };
        let input_scope = AuMidiMapping {
            scope: Scope::INPUT,
            ..AuMidiMapping::control_change(903, 0, 7)
        };
        let bend = AuMidiMapping {
            trigger: MidiTrigger::PitchBend,
            ..AuMidiMapping::control_change(904, 0, 0)
        };
        assert_eq!(
            declared(&[on_channel, any, note, input_scope, bend]),
            [
                MidiAssignment {
                    channel: MidiChannel::new(3),
                    source: MidiSource::ControlChange(U7::of(74)),
                    param: ParamId::new(900),
                },
                MidiAssignment {
                    channel: None,
                    source: MidiSource::ControlChange(U7::of(1)),
                    param: ParamId::new(901),
                },
                MidiAssignment {
                    channel: MidiChannel::new(0),
                    source: MidiSource::PitchBend,
                    param: ParamId::new(904),
                },
            ]
        );
    }

    /// The C ABI of `AUParameterMIDIMapping` (`AudioUnitProperties.h`): size 32,
    /// align 4, `mStatus` at 24, `reserved3` at 28. The byte codec writes every
    /// field where the `#[repr(C)]` struct holds it and reads back what it wrote.
    #[test]
    fn the_byte_codec_places_every_field_at_its_c_offset() {
        use std::mem::offset_of;
        let raw = AuParameterMidiMappingRaw {
            scope: 1,
            element: 2,
            parameter_id: 3,
            flags: 4,
            sub_range_min: 1.5,
            sub_range_max: 2.5,
            status: 0xB0,
            data1: 7,
            reserved1: 8,
            reserved2: 9,
            reserved3: 10,
        };
        let b = raw.to_bytes();
        let u32_at = |off: usize| u32::from_ne_bytes(b[off..off + 4].try_into().unwrap());
        assert_eq!(u32_at(offset_of!(AuParameterMidiMappingRaw, scope)), 1);
        assert_eq!(u32_at(offset_of!(AuParameterMidiMappingRaw, element)), 2);
        assert_eq!(
            u32_at(offset_of!(AuParameterMidiMappingRaw, parameter_id)),
            3
        );
        assert_eq!(u32_at(offset_of!(AuParameterMidiMappingRaw, flags)), 4);
        assert_eq!(
            f32::from_bits(u32_at(offset_of!(AuParameterMidiMappingRaw, sub_range_min))),
            1.5
        );
        assert_eq!(
            f32::from_bits(u32_at(offset_of!(AuParameterMidiMappingRaw, sub_range_max))),
            2.5
        );
        assert_eq!(b[offset_of!(AuParameterMidiMappingRaw, status)], 0xB0);
        assert_eq!(b[offset_of!(AuParameterMidiMappingRaw, data1)], 7);
        assert_eq!(b[offset_of!(AuParameterMidiMappingRaw, reserved1)], 8);
        assert_eq!(b[offset_of!(AuParameterMidiMappingRaw, reserved2)], 9);
        assert_eq!(u32_at(offset_of!(AuParameterMidiMappingRaw, reserved3)), 10);
        assert_eq!(AuParameterMidiMappingRaw::from_bytes(&b), raw);
    }

    #[test]
    fn the_mapping_struct_matches_the_c_abi() {
        assert_eq!(size_of::<AuParameterMidiMappingRaw>(), 32);
        assert_eq!(std::mem::align_of::<AuParameterMidiMappingRaw>(), 4);

        // Field offsets, computed from a real value rather than asserted by
        // eye — `offset_of!` over every field, including the two `u8`s that
        // share a word with the reserved bytes.
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, scope), 0);
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, element), 4);
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, parameter_id),
            8
        );
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, flags), 12);
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, sub_range_min),
            16
        );
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, sub_range_max),
            20
        );
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, status), 24);
        assert_eq!(std::mem::offset_of!(AuParameterMidiMappingRaw, data1), 25);
        assert_eq!(
            std::mem::offset_of!(AuParameterMidiMappingRaw, reserved3),
            28
        );
    }

    /// The flag constants must equal Apple's, which are `1 << 0 .. 1 << 5`.
    /// Spelled out rather than derived, because `coreaudio-sys` does not export
    /// them and a transposed pair (`Toggle` for `SubRange`) would change what an
    /// incoming controller value means with no compile error.
    #[test]
    fn the_flag_bits_match_the_header() {
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL, 1);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE, 2);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE, 4);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_TOGGLE, 8);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR, 16);
        assert_eq!(K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON, 32);
        // And the property ids, from the same C run: All=41 Add=42 Remove=43
        // HotMap=44.
        assert_eq!(AllParameterMidiMappings::ID, 41);
        assert_eq!(AddParameterMidiMapping::ID, 42);
        assert_eq!(RemoveParameterMidiMapping::ID, 43);
        assert_eq!(HotMapParameterMidiMapping::ID, 44);
    }

    /// Every trigger must survive `to_raw`/`from_raw`, and the status byte must
    /// carry the command nibble Apple's table specifies. The table itself is the
    /// assertion — a transposed nibble maps note-off where the host meant CC.
    #[test]
    fn trigger_round_trips_through_the_status_byte() {
        let cases = [
            (MidiTrigger::NoteOff { note: 60 }, 0x80u8, 60u8),
            (MidiTrigger::NoteOn { note: 61 }, 0x90, 61),
            (MidiTrigger::KeyPressure { note: 62 }, 0xA0, 62),
            (MidiTrigger::ControlChange { controller: 1 }, 0xB0, 1),
            (MidiTrigger::ProgramChange { patch: 5 }, 0xC0, 5),
            // The two forms whose mData1 the header marks unused must encode 0.
            (MidiTrigger::ChannelPressure, 0xD0, 0),
            (MidiTrigger::PitchBend, 0xE0, 0),
        ];
        for (trigger, nibble, data1) in cases {
            let m = AuMidiMapping {
                trigger,
                channel: 3,
                ..AuMidiMapping::control_change(0, 3, 0)
            };
            let raw = m.to_raw();
            assert_eq!(
                raw.status,
                nibble | 3,
                "{trigger:?} must encode status {nibble:#x} | channel 3"
            );
            assert_eq!(raw.data1, data1, "{trigger:?} mData1");
            assert_eq!(
                AuMidiMapping::from_raw(&raw).trigger,
                trigger,
                "{trigger:?} must survive the round trip"
            );
            assert_eq!(AuMidiMapping::from_raw(&raw).channel, 3);
        }
    }

    /// A status byte outside the seven channel-voice commands must decode to
    /// `Other`, not be mistaken for the nearest match. `0xF8` (MIDI clock) and
    /// `0x00` (not a status byte at all) are both shapes an AU could store —
    /// one implementer stores an out-of-range `mScope` verbatim, so garbage in
    /// this field is not hypothetical.
    #[test]
    fn unknown_status_bytes_decode_to_other() {
        for status in [0xF8u8, 0xFF, 0x00, 0x7F] {
            let raw = AuParameterMidiMappingRaw {
                status,
                data1: 42,
                ..Default::default()
            };
            assert_eq!(
                AuMidiMapping::from_raw(&raw).trigger,
                MidiTrigger::Other { status, data1: 42 },
                "status {status:#x} must not be decoded as a known command"
            );
        }
    }

    /// The channel nibble must be masked on the way out. Without the mask a
    /// caller passing an out-of-range channel sets bits above the low nibble and
    /// silently changes *which MIDI message* the mapping listens for — a
    /// `ControlChange` (`0xB0`) becomes a system message (`0xF0`), so the mapping
    /// never fires.
    ///
    /// The values here are chosen to actually expose that. This test originally
    /// used `channel: 16` — the natural off-by-one from the 1-based channel
    /// numbering every DAW displays — and **passed with the mask deleted**,
    /// because `0xB0 | 16 == 0xB0`: bit 4 is already set in the CC status
    /// nibble, so 16 is absorbed. Each case below is asserted against the
    /// unmasked result it would produce, so no case can be silently absorbed
    /// again.
    #[test]
    fn an_out_of_range_channel_cannot_corrupt_the_command_nibble() {
        // (channel, what `0xB0 | channel` would give unmasked, masked low nibble)
        let cases = [
            (64u8, 0xF0u8, 0u8),
            (100, 0xF0, 4),
            (255, 0xF0, 15),
            (17, 0xB0, 1),
        ];
        for (channel, unmasked_nibble, expected_low) in cases {
            let raw = AuMidiMapping::control_change(7, channel, 74).to_raw();
            assert_eq!(
                raw.status & 0xF0,
                0xB0,
                "channel {channel} must not bleed into the command nibble — \
                 unmasked it would produce {unmasked_nibble:#x}"
            );
            assert_eq!(
                raw.status & 0x0F,
                expected_low,
                "channel {channel} must mask to its low nibble"
            );
            assert_eq!(
                AuMidiMapping::from_raw(&raw).trigger,
                MidiTrigger::ControlChange { controller: 74 },
                "channel {channel} must still decode as a control change"
            );
        }
        // At least one case must be one the unmasked path gets WRONG, or this
        // test cannot fail. `64` and `255` both land on 0xF0; assert that here
        // so a future edit cannot narrow the table down to absorbed values only.
        assert!(
            cases.iter().any(|(_, unmasked, _)| *unmasked != 0xB0),
            "the case table must contain a channel the unmasked path corrupts"
        );
    }

    /// Each flag must occupy its own bit and survive the round trip. Asserted
    /// one flag at a time *and* all together, because a `|=` written against the
    /// wrong constant only shows up when the two flags are set independently.
    #[test]
    fn each_flag_round_trips_independently() {
        let base = AuMidiMapping::control_change(1, 0, 1);

        let cases: [(&str, AuMidiMapping, u32); 5] = [
            (
                "any_channel",
                AuMidiMapping {
                    any_channel: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL,
            ),
            (
                "any_note",
                AuMidiMapping {
                    any_note: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE,
            ),
            (
                "toggle",
                AuMidiMapping {
                    toggle: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_TOGGLE,
            ),
            (
                "bipolar",
                AuMidiMapping {
                    bipolar: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR,
            ),
            (
                "bipolar_on",
                AuMidiMapping {
                    bipolar_on: true,
                    ..base
                },
                K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON,
            ),
        ];
        for (name, mapping, bit) in cases {
            let raw = mapping.to_raw();
            assert_eq!(raw.flags, bit, "{name} must set exactly bit {bit}");
            assert_eq!(
                AuMidiMapping::from_raw(&raw),
                mapping,
                "{name} must survive the round trip"
            );
        }

        // All six at once, sub-range included.
        let every = AuMidiMapping {
            any_channel: true,
            any_note: true,
            sub_range: Some((0.25, 0.75)),
            toggle: true,
            bipolar: true,
            bipolar_on: true,
            ..base
        };
        let raw = every.to_raw();
        assert_eq!(raw.flags, 0b11_1111, "all six flags");
        assert_eq!(AuMidiMapping::from_raw(&raw), every);
    }

    /// The sub-range flag and its two value fields must move together. This is
    /// the case a `bool` + two floats would get wrong: `None` must not leave
    /// stale bounds in the struct (the AU stores what it is handed), and a
    /// mapping whose flag is clear must decode as `None` **whatever** the two
    /// value fields hold — otherwise a host would show a sub-range that the AU
    /// is ignoring.
    #[test]
    fn sub_range_travels_with_its_flag() {
        let none = AuMidiMapping::control_change(1, 0, 1);
        let raw = none.to_raw();
        assert_eq!(raw.flags & K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE, 0);
        assert_eq!(
            (raw.sub_range_min, raw.sub_range_max),
            (0.0, 0.0),
            "a None sub-range must zero the bounds, not leave them stale"
        );

        // Flag clear but bounds populated — an AU could hand this back.
        let stale = AuParameterMidiMappingRaw {
            flags: 0,
            sub_range_min: 0.1,
            sub_range_max: 0.9,
            status: 0xB0,
            ..Default::default()
        };
        assert_eq!(
            AuMidiMapping::from_raw(&stale).sub_range,
            None,
            "bounds without the flag must read as no sub-range"
        );

        // And a real sub-range survives verbatim, in parameter units (not
        // normalized) — the AUSampler `Gain` case is -96..=12 dB.
        let dbs = AuMidiMapping {
            sub_range: Some((-24.0, 6.0)),
            ..AuMidiMapping::control_change(900, 0, 7)
        };
        let raw = dbs.to_raw();
        assert_ne!(raw.flags & K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE, 0);
        assert_eq!((raw.sub_range_min, raw.sub_range_max), (-24.0, 6.0));
        assert_eq!(AuMidiMapping::from_raw(&raw), dbs);
    }

    /// The reserved fields must be zeroed on every encode. Apple's header says
    /// "MUST be set to zero"; they are private precisely so no call site can
    /// leave them holding whatever was on the stack.
    #[test]
    fn reserved_fields_are_always_zero() {
        let m = AuMidiMapping {
            sub_range: Some((0.1, 0.2)),
            any_channel: true,
            toggle: true,
            ..AuMidiMapping::control_change(9, 5, 64)
        };
        let raw = m.to_raw();
        assert_eq!((raw.reserved1, raw.reserved2, raw.reserved3), (0, 0, 0));
    }

    /// A table decodes as a whole, and a buffer with a partial trailing struct
    /// is truncated to whole structs rather than reading uninitialized bytes.
    /// The unaligned read matters: the bytes come from a `Vec<u8>` (align 1)
    /// while the struct wants align 4, so a plain pointer deref would be UB.
    #[test]
    fn decode_table_handles_whole_and_partial_buffers() {
        let mappings = [
            AuMidiMapping::control_change(900, 0, 1),
            AuMidiMapping {
                sub_range: Some((0.25, 0.75)),
                toggle: true,
                ..AuMidiMapping::control_change(901, 3, 74)
            },
        ];
        let bytes: Vec<u8> = encode_table(&mappings)
            .into_iter()
            .flat_map(AuParameterMidiMappingRaw::to_bytes)
            .collect();
        let bytes = bytes.as_slice();
        assert_eq!(bytes.len(), 64);
        assert_eq!(decode_table(bytes), mappings);

        // 63 bytes: one whole struct plus a fragment.
        assert_eq!(decode_table(&bytes[..63]), mappings[..1]);
        // Shorter than one struct: nothing decodes.
        assert!(decode_table(&bytes[..31]).is_empty());
        assert!(decode_table(&[]).is_empty());
    }

    /// Remove and replace both match on `(scope, element, parameter_id)` only,
    /// so the predicate must ignore the trigger and every flag — and must NOT
    /// ignore the element, which is what distinguishes two parameters that share
    /// an id across elements.
    #[test]
    fn targets_same_parameter_keys_on_the_documented_triple() {
        let a = AuMidiMapping::control_change(900, 0, 1);
        let b = AuMidiMapping {
            trigger: MidiTrigger::PitchBend,
            channel: 9,
            toggle: true,
            sub_range: Some((0.0, 0.5)),
            ..a
        };
        assert!(
            a.targets_same_parameter(&b),
            "trigger and flags must not count"
        );

        let other_param = AuMidiMapping::control_change(901, 0, 1);
        assert!(!a.targets_same_parameter(&other_param));

        let other_element = AuMidiMapping {
            element: Element(1),
            ..a
        };
        assert!(
            !a.targets_same_parameter(&other_element),
            "the element is part of the key"
        );

        let other_scope = AuMidiMapping {
            scope: Scope::INPUT,
            ..a
        };
        assert!(
            !a.targets_same_parameter(&other_scope),
            "the scope is part of the key"
        );
    }

    /// The any-channel constructor must set the flag *and* leave a channel the
    /// AU will ignore — not silently produce a channel-1 mapping that looks
    /// identical to a deliberate one.
    #[test]
    fn any_channel_constructor_sets_the_flag() {
        let m = AuMidiMapping::control_change_any_channel(900, 1);
        assert!(m.any_channel);
        assert_eq!(m.channel, 0);
        assert_eq!(m.trigger, MidiTrigger::ControlChange { controller: 1 });
        assert_ne!(
            m.to_raw().flags & K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL,
            0
        );
        // And it differs from the explicit-channel form, so the two are not
        // interchangeable at a call site.
        assert_ne!(m, AuMidiMapping::control_change(900, 0, 1));
    }

    /// `is_note_command` gates the any-note flag, and the header restricts that
    /// flag to exactly note on / note off / polyphonic pressure. A wrong answer
    /// here would let a UI offer "any note" for a pitch-bend mapping.
    #[test]
    fn only_note_commands_are_note_commands() {
        assert!(MidiTrigger::NoteOn { note: 0 }.is_note_command());
        assert!(MidiTrigger::NoteOff { note: 0 }.is_note_command());
        assert!(MidiTrigger::KeyPressure { note: 0 }.is_note_command());
        assert!(!MidiTrigger::ControlChange { controller: 1 }.is_note_command());
        assert!(!MidiTrigger::ProgramChange { patch: 0 }.is_note_command());
        assert!(!MidiTrigger::ChannelPressure.is_note_command());
        assert!(!MidiTrigger::PitchBend.is_note_command());
        assert!(
            !MidiTrigger::Other {
                status: 0xF8,
                data1: 0
            }
            .is_note_command()
        );
    }
}
