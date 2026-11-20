//! Builders and observers for MIDI tests.
//!
//! A builder is one library call: it puts a `Voice1` or `Voice2` constructor
//! at an address with `Message::midi1` or `Message::midi2` and returns the
//! [`Ump`]. It takes a note as a bare `u8` where a test writes a literal, and
//! panics on a number the field type refuses. The address is a bare
//! [`MidiChannel`] for the first Group, or an [`Address`].
//!
//! A message with no builder is built with the library's constructors; a
//! 7-bit value reaches a MIDI 2.0 field through `NoteVelocity::from`,
//! `ControlValue::from` and `BendValue::from`.

use tutti_midi_protocol::{MpeMode, MpeZone, MpeZoneConfig, MpeZones, PitchBendSensitivity};
use tutti_ump::{
    Address, Attribute, Bank, BendValue, BlockDirection, CCNumber, ControlValue, DeviceIdentity,
    ManufacturerId, Message, Midi1Port, MidiChannel, NoteNumber, NoteVelocity, Protocol, Rpn,
    SoftwareRevision, Stream, U7, Ump, Voice1, Voice2,
};
use tutti_units::Semitones;

/// A Device Identity for a test endpoint: a 21-bit manufacturer id (`00 21 09`,
/// M2-104 §7.10) with a family, model and revision of which every byte differs,
/// so a swapped or shifted byte shows.
pub fn device_identity() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::of(0x8000 | 0x2109),
        family: [U7::of(0x11), U7::of(0x22)],
        model: [U7::of(0x33), U7::of(0x44)],
        revision: SoftwareRevision::from([0x55, 0x66, 0x77, 0x7F].map(U7::of)),
    }
}

/// A MIDI 2.0 Note On with no attribute.
pub fn note_on(address: impl Into<Address>, n: u8, velocity: u16) -> Ump {
    Message::midi2(
        address,
        Voice2::note_on(NoteNumber::of(n), NoteVelocity::new(velocity)),
    )
    .into()
}

/// A MIDI 2.0 Note Off with no attribute.
pub fn note_off(address: impl Into<Address>, n: u8, velocity: u16) -> Ump {
    Message::midi2(
        address,
        Voice2::note_off(NoteNumber::of(n), NoteVelocity::new(velocity)),
    )
    .into()
}

/// A MIDI 2.0 Control Change.
pub fn cc(address: impl Into<Address>, controller: CCNumber, value: u32) -> Ump {
    Message::midi2(
        address,
        Voice2::control_change(controller, ControlValue::new(value)),
    )
    .into()
}

/// A MIDI 2.0 Pitch Bend.
pub fn pitch_bend(address: impl Into<Address>, bend: u32) -> Ump {
    Message::midi2(address, Voice2::PitchBend(BendValue::new(bend))).into()
}

/// A MIDI 2.0 Per-Note Pitch Bend.
pub fn per_note_pitch_bend(address: impl Into<Address>, n: u8, bend: u32) -> Ump {
    Message::midi2(
        address,
        Voice2::per_note_pitch_bend(NoteNumber::of(n), BendValue::new(bend)),
    )
    .into()
}

/// A MIDI 2.0 Polyphonic Pressure.
pub fn poly_pressure(address: impl Into<Address>, n: u8, value: u32) -> Ump {
    Message::midi2(
        address,
        Voice2::poly_pressure(NoteNumber::of(n), ControlValue::new(value)),
    )
    .into()
}

/// A MIDI 2.0 Channel Pressure.
pub fn channel_pressure(address: impl Into<Address>, value: u32) -> Ump {
    Message::midi2(address, Voice2::ChannelPressure(ControlValue::new(value))).into()
}

/// A MIDI 2.0 Registered Controller at `rpn`.
pub fn registered(address: impl Into<Address>, rpn: Rpn, value: u32) -> Ump {
    Message::midi2(address, Voice2::registered(rpn, ControlValue::new(value))).into()
}

/// A MIDI 2.0 Program Change, with `bank` when the Bank Valid bit is set.
pub fn program_change(address: impl Into<Address>, program: u8, bank: Option<Bank>) -> Ump {
    let program = U7::of(program);
    Message::midi2(address, Voice2::ProgramChange { program, bank }).into()
}

/// A MIDI 1.0 Note On.
pub fn m1_note_on(address: impl Into<Address>, n: u8, velocity: U7) -> Ump {
    Message::midi1(address, Voice1::note_on(NoteNumber::of(n), velocity)).into()
}

/// A MIDI 1.0 Note Off.
pub fn m1_note_off(address: impl Into<Address>, n: u8, velocity: U7) -> Ump {
    Message::midi1(address, Voice1::note_off(NoteNumber::of(n), velocity)).into()
}

/// A MIDI 1.0 Control Change.
pub fn m1_cc(address: impl Into<Address>, controller: CCNumber, value: U7) -> Ump {
    Message::midi1(address, Voice1::control_change(controller, value)).into()
}

/// A receiver of the per-note bend sensitivity (RPN #00/07, M2-104 section
/// 7.4.13), for a test that checks what a stream tells one: each channel
/// starts at the MPE default, ±48 (RP-053), and [`observe`](Self::observe)
/// follows each message of the RPN. It reads the Group's channels as one
/// 16-channel space and ignores which Group a message is in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BendRanges([Semitones; 16]);

impl Default for BendRanges {
    fn default() -> Self {
        Self([PitchBendSensitivity::MPE_MEMBER_DEFAULT.semitones(); 16])
    }
}

impl BendRanges {
    /// The range on `channel`, in semitones.
    #[must_use]
    pub fn semitones(&self, channel: MidiChannel) -> Semitones {
        self.0[channel.index()]
    }

    /// Follows `ump`: a per-note bend sensitivity sets its channel's range, and
    /// `true` says it was one. Anything else leaves the ranges as they are.
    pub fn observe(&mut self, ump: &Ump) -> bool {
        let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) =
            (*ump).decode()
        else {
            return false;
        };
        if Rpn::new(bank, index) != Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY {
            return false;
        }
        self.0[address.channel.index()] =
            tutti_midi_protocol::PitchBendSensitivity::from_rpn_bits(value.get()).semitones();
        true
    }
}

/// `len` 7-bit bytes, counting up from 1 and wrapping inside `0..=0x7F`, so
/// a payload of any length is valid SysEx data and neighbours differ.
pub fn sysex_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i + 1) % 0x80) as u8).collect()
}

/// The `(channel, note, on)` of a Note On or Note Off packet of either
/// protocol, as its message type says (a MIDI 1.0 Note On with velocity 0 is
/// still `on` here: this reads the status, not the held-key meaning). `None`
/// for anything else.
pub fn key_of(ump: &Ump) -> Option<(MidiChannel, NoteNumber, bool)> {
    match (*ump).decode().ok()? {
        Message::Midi1(a, Voice1::NoteOn { note, .. })
        | Message::Midi2(a, Voice2::NoteOn { note, .. }) => Some((a.channel, note, true)),
        Message::Midi1(a, Voice1::NoteOff { note, .. })
        | Message::Midi2(a, Voice2::NoteOff { note, .. }) => Some((a.channel, note, false)),
        _ => None,
    }
}

/// An MPE lower zone of `members` member channels as an [`MpeMode`]. Panics
/// outside `1..=15`, which RP-053 gives no zone.
pub fn lower_zone(members: u8) -> MpeMode {
    zones(Some(members), None)
}

/// An MPE upper zone of `members` member channels as an [`MpeMode`].
pub fn upper_zone(members: u8) -> MpeMode {
    zones(None, Some(members))
}

/// Both MPE zones as an [`MpeMode`]. Panics when the two would share a member
/// channel (more than 14 members between them).
pub fn dual_zones(lower: u8, upper: u8) -> MpeMode {
    zones(Some(lower), Some(upper))
}

fn zones(lower: Option<u8>, upper: Option<u8>) -> MpeMode {
    let config = |zone, n: u8| {
        MpeZoneConfig::new(
            zone,
            n,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap_or_else(|| panic!("{n} is not an MPE member count"))
    };
    MpeMode::Zones(
        MpeZones::new(
            lower.map(|n| config(MpeZone::Lower, n)),
            upper.map(|n| config(MpeZone::Upper, n)),
        )
        .expect("zones that RP-053 allows"),
    )
}

/// The Roland GS reset as a framed SysEx message: Data Set 1 to device 0x10,
/// address 40 00 7F, data 00. The Windows built-in synth takes it, and a test
/// that opens that synth sends it.
pub const GS_RESET: [u8; 11] = [
    0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7,
];

/// Part of the name the Windows built-in synth is listed under.
pub const GS_WAVETABLE: &str = "GS Wavetable";

/// An attribute of a reserved type, read from the wire as a receiver reads one:
/// no sender can name a reserved value, so a test takes it from the packet.
///
/// # Panics
///
/// When `kind` is a defined attribute type (`0..=3`).
#[must_use]
pub fn reserved_attribute(kind: u8, data: u16) -> Attribute {
    assert!(kind >= 4, "attribute type {kind} is defined");
    let packet = Ump([
        0x4090_3C00 | u32::from(kind),
        0x8000_0000 | u32::from(data),
        0,
        0,
    ]);
    match packet.decode() {
        Ok(Message::Midi2(_, Voice2::NoteOn { attribute, .. })) => attribute,
        other => unreachable!("a Note On with attribute type {kind} decoded as {other:?}"),
    }
}

/// A reserved protocol value, read from the wire as a receiver reads one.
///
/// # Panics
///
/// When `raw` is `1` or `2`, the protocols M2-104 §7.1.6 defines.
#[must_use]
pub fn reserved_protocol(raw: u8) -> Protocol {
    assert!(!matches!(raw, 1 | 2), "protocol {raw} is defined");
    match (Ump([0xF006_0000 | u32::from(raw) << 8, 0, 0, 0])).decode() {
        Ok(Message::Stream(Stream::StreamConfigurationNotification(config))) => config.protocol,
        other => unreachable!("a Stream Configuration Notification decoded as {other:?}"),
    }
}

fn reserved_block_info(word0: u32) -> tutti_ump::FunctionBlockInfo {
    let packet = Ump([0xF011_0000 | word0, 0x0001_0000, 0, 0]);
    match packet.decode() {
        Ok(Message::Stream(Stream::FunctionBlockInfo(info))) => info,
        other => unreachable!("a Function Block Info decoded as {other:?}"),
    }
}

/// The reserved Function Block direction (`0b00`), read from the wire.
#[must_use]
pub fn reserved_direction() -> BlockDirection {
    reserved_block_info(0).direction
}

/// The reserved Function Block MIDI 1.0 port value (`0x03`), read from the wire.
#[must_use]
pub fn reserved_midi1_port() -> Midi1Port {
    reserved_block_info(0b1101).midi1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_of_reads_the_status_of_either_protocol() {
        let (c, n) = (MidiChannel::of(3), NoteNumber::of(60));
        let off = Ump::from(Message::midi1(c, Voice1::note_off(n, tutti_ump::U7::of(0))));
        assert_eq!(key_of(&off), Some((c, n, false)));
        let on2 = Ump::from(Message::midi2(
            MidiChannel::FIRST,
            Voice2::note_on(NoteNumber::of(61), tutti_ump::NoteVelocity::new(1)),
        ));
        assert_eq!(
            key_of(&on2),
            Some((MidiChannel::FIRST, NoteNumber::of(61), true))
        );
        let clock = Ump::from(Message::System(
            tutti_ump::MidiGroup::FIRST,
            tutti_ump::System::TimingClock,
        ));
        assert_eq!(key_of(&clock), None);
    }

    #[test]
    fn sysex_payload_counts_up_inside_seven_bits() {
        assert_eq!(&sysex_payload(3), &[1, 2, 3]);
        assert_eq!(sysex_payload(130)[126..], [0x7F, 0, 1, 2]);
    }
}
