//! The Universal System Exclusive writer: an [`Addressed`] message as the
//! payload bytes [`read()`](fn@super::read) reads back.
//!
//! The Machine Control and MTC Full bytes are their own types' writers
//! ([`MmcCommands::payload`](super::MmcCommands::payload),
//! [`MtcFullMessage::payload`](super::MtcFullMessage::payload)); the rest is written here, field for field the
//! grammar of the reader. The payload is held inline, so writing one allocates
//! nothing.

use tutti_ump::{DeviceIdentity, U7, U14, UNIVERSAL_NON_REAL_TIME, UNIVERSAL_REAL_TIME};

use super::{Addressed, GeneralMidiMode, Universal};
use crate::tuning::{
    KeyChanges, MAX_KEY_CHANGES, OctaveOffsets, Timing, TuningFrequency, TuningMessage, TuningName,
};

/// The longest payload [`Addressed::payload`] writes: a Single Note Tuning
/// Change of [`MAX_KEY_CHANGES`] changes with a bank (`7F dev 08 07 bank prog
/// ll` and four bytes a change), the longest message of the grammar.
pub const UNIVERSAL_MAX_PAYLOAD: usize = 7 + 4 * MAX_KEY_CHANGES;

/// The payload of one Universal message: the bytes between `F0` and `F7`, held
/// inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UniversalPayload {
    buf: [U7; UNIVERSAL_MAX_PAYLOAD],
    len: usize,
}

impl UniversalPayload {
    /// The bytes, each seven-bit by type:
    /// [`Sysex7Packets::part`](tutti_ump::Sysex7Packets::part) packs them as they are.
    #[must_use]
    pub fn as_bytes(&self) -> &[U7] {
        &self.buf[..self.len]
    }

    fn new() -> Self {
        Self {
            buf: [U7::of(0); UNIVERSAL_MAX_PAYLOAD],
            len: 0,
        }
    }

    // The callers write at most `UNIVERSAL_MAX_PAYLOAD` bytes, the longest message.
    fn put_u7(&mut self, value: U7) {
        self.buf[self.len] = value;
        self.len += 1;
    }

    /// Data bytes of the grammar, written as literals: `U7::of` refuses an
    /// eight-bit one.
    fn put_all<const N: usize>(&mut self, bytes: [u8; N]) {
        for byte in bytes {
            self.put_u7(U7::of(byte));
        }
    }

    fn put_u7s(&mut self, values: impl IntoIterator<Item = U7>) {
        for value in values {
            self.put_u7(value);
        }
    }

    /// A 14-bit value, least significant byte first (M1 p.40, p.57).
    fn put_u14_lsb_first(&mut self, value: U14) {
        self.put_u7(value.lsb());
        self.put_u7(value.msb());
    }

    /// A 14-bit value, most significant byte first, as MTS sends its fractions.
    fn put_u14_msb_first(&mut self, value: U14) {
        self.put_u7(value.msb());
        self.put_u7(value.lsb());
    }

    fn put_frequency(&mut self, word: TuningFrequency) {
        self.put_u7(word.semitone());
        self.put_u14_msb_first(word.fraction());
    }

    fn put_name(&mut self, name: &TuningName) {
        self.put_u7s(name.bytes().iter().copied());
    }

    fn put_offsets(&mut self, offsets: &OctaveOffsets) {
        match offsets {
            OctaveOffsets::Cents(bytes) => self.put_u7s(bytes.iter().copied()),
            OctaveOffsets::Fine(words) => {
                for word in words {
                    self.put_u14_msb_first(*word);
                }
            }
        }
    }

    /// The checksum a dump ends with: the XOR of every byte before it, masked
    /// to seven bits (the reader checks the same).
    fn put_checksum(&mut self) {
        let sum = self.as_bytes().iter().fold(0, |acc, b| acc ^ b.get());
        self.put_u7(U7::masked(sum));
    }

    fn put_identity(&mut self, identity: &DeviceIdentity) {
        let [a, b, c] = identity.manufacturer.sysex_bytes();
        if identity.manufacturer.is_extended() {
            self.put_u7s([a, b, c]);
        } else {
            self.put_u7(a);
        }
        self.put_u7s(identity.family);
        self.put_u7s(identity.model);
        self.put_u7s(identity.revision.get());
    }

    fn put_tuning(&mut self, message: &TuningMessage) {
        match message {
            TuningMessage::DumpRequest {
                bank: None,
                program,
            } => {
                self.put_all([0x08, 0x00]);
                self.put_u7(*program);
            }
            TuningMessage::DumpRequest {
                bank: Some(bank),
                program,
            } => {
                self.put_all([0x08, 0x03]);
                self.put_u7s([*bank, *program]);
            }
            TuningMessage::KeyDump {
                bank,
                program,
                name,
                table,
            } => {
                match bank {
                    None => {
                        self.put_all([0x08, 0x01]);
                        self.put_u7(*program);
                    }
                    Some(bank) => {
                        self.put_all([0x08, 0x04]);
                        self.put_u7s([*bank, *program]);
                    }
                }
                self.put_name(name);
                for word in table.words() {
                    self.put_frequency(*word);
                }
                self.put_checksum();
            }
            TuningMessage::OctaveDump {
                bank,
                program,
                name,
                offsets,
            } => {
                let sub = if matches!(offsets, OctaveOffsets::Cents(_)) {
                    0x05
                } else {
                    0x06
                };
                self.put_all([0x08, sub]);
                self.put_u7s([*bank, *program]);
                self.put_name(name);
                self.put_offsets(offsets);
                self.put_checksum();
            }
            TuningMessage::NoteChange {
                bank,
                program,
                changes,
            } => {
                self.put_all([0x08, 0x07]);
                self.put_u7s([*bank, *program]);
                self.put_key_changes(changes);
            }
            TuningMessage::RealTimeNoteChange {
                bank,
                program,
                changes,
            } => {
                match bank {
                    None => {
                        self.put_all([0x08, 0x02]);
                        self.put_u7(*program);
                    }
                    Some(bank) => {
                        self.put_all([0x08, 0x07]);
                        self.put_u7s([*bank, *program]);
                    }
                }
                self.put_key_changes(changes);
            }
            TuningMessage::OctaveTuning {
                timing: _,
                channels,
                offsets,
            } => {
                let bits = channels.bits();
                let sub = if matches!(offsets, OctaveOffsets::Cents(_)) {
                    0x08
                } else {
                    0x09
                };
                self.put_all([0x08, sub]);
                // The channels as `ff gg hh`: 15 and 16, then 8 to 14, then 1 to 7.
                self.put_u7s([bits >> 14, bits >> 7, bits].map(|part| U7::masked(part as u8)));
                self.put_offsets(offsets);
            }
        }
    }

    /// The count and the changes of a Single Note Tuning Change.
    fn put_key_changes(&mut self, changes: &KeyChanges) {
        // At most `MAX_KEY_CHANGES` (127), a data byte.
        self.put_u7(U7::masked(changes.as_slice().len() as u8));
        for change in changes.as_slice() {
            self.put_u7(U7::masked(change.key.get()));
            self.put_frequency(change.frequency);
        }
    }
}

impl core::ops::Deref for UniversalPayload {
    type Target = [U7];

    fn deref(&self) -> &[U7] {
        self.as_bytes()
    }
}

/// The class byte (`7E` or `7F`) a message of `timing` opens with.
const fn class(timing: Timing) -> u8 {
    match timing {
        Timing::RealTime => UNIVERSAL_REAL_TIME,
        Timing::NonRealTime => UNIVERSAL_NON_REAL_TIME,
    }
}

impl Addressed {
    /// The payload of the message (the bytes between `F0` and `F7`), the
    /// inverse of [`read()`](fn@super::read). It goes out as SysEx7 through
    /// [`Sysex7Packets::part`](tutti_ump::Sysex7Packets::part) (`SysexPart::Only`).
    pub fn payload(&self) -> UniversalPayload {
        let mut out = UniversalPayload::new();
        let device = self.to.device_byte();
        let header = |out: &mut UniversalPayload, timing: Timing| {
            out.put_u7s([class(timing), device].map(U7::masked));
        };
        match &self.message {
            Universal::IdentityRequest => {
                header(&mut out, Timing::NonRealTime);
                out.put_all([0x06, 0x01]);
            }
            Universal::IdentityReply(identity) => {
                header(&mut out, Timing::NonRealTime);
                out.put_all([0x06, 0x02]);
                out.put_identity(identity);
            }
            Universal::GeneralMidi(mode) => {
                header(&mut out, Timing::NonRealTime);
                out.put_all([
                    0x09,
                    match mode {
                        GeneralMidiMode::On => 0x01,
                        GeneralMidiMode::Off => 0x02,
                    },
                ]);
            }
            Universal::MasterVolume(value) => {
                header(&mut out, Timing::RealTime);
                out.put_all([0x04, 0x01]);
                out.put_u14_lsb_first(*value);
            }
            Universal::MasterBalance(value) => {
                header(&mut out, Timing::RealTime);
                out.put_all([0x04, 0x02]);
                out.put_u14_lsb_first(*value);
            }
            Universal::MasterFineTuning(value) => {
                header(&mut out, Timing::RealTime);
                out.put_all([0x04, 0x03]);
                out.put_u14_lsb_first(*value);
            }
            Universal::MasterCoarseTuning(msb) => {
                header(&mut out, Timing::RealTime);
                out.put_all([0x04, 0x04, 0x00, msb.get()]);
            }
            Universal::Tuning(message) => {
                let timing = match message {
                    TuningMessage::RealTimeNoteChange { .. } => Timing::RealTime,
                    TuningMessage::OctaveTuning { timing, .. } => *timing,
                    _ => Timing::NonRealTime,
                };
                header(&mut out, timing);
                out.put_tuning(message);
            }
            Universal::MtcFullMessage(full) => {
                let mut bytes = full.payload();
                bytes[1] = device;
                out.put_u7s(bytes.map(U7::masked));
            }
            Universal::MachineControl(commands) => {
                out.put_u7s(
                    commands
                        .payload(self.to)
                        .as_bytes()
                        .iter()
                        .copied()
                        .map(U7::masked),
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MmcCommands, MtcFullMessage, MtcRate, Recipient, SysexDeviceId, read};
    use crate::{TimecodeHours, TimecodeMinutes, TimecodeSeconds, UniversalRead};
    use tutti_ump::{ManufacturerId, SoftwareRevision, SysexMessage};

    /// A distinct index for each variant: the match is exhaustive here, so a
    /// variant added to `Universal` fails to compile until it has a sample.
    const fn index(message: &Universal) -> usize {
        match message {
            Universal::IdentityRequest => 0,
            Universal::IdentityReply(_) => 1,
            Universal::GeneralMidi(_) => 2,
            Universal::MasterVolume(_) => 3,
            Universal::MasterBalance(_) => 4,
            Universal::MasterFineTuning(_) => 5,
            Universal::MasterCoarseTuning(_) => 6,
            Universal::Tuning(_) => 7,
            Universal::MtcFullMessage(_) => 8,
            Universal::MachineControl(_) => 9,
        }
    }

    /// Every variant of `Universal` has a payload, and `read` gives back the
    /// message it was written from, to each kind of recipient.
    #[test]
    fn every_universal_message_is_written_and_read_back() {
        let samples = [
            Universal::IdentityRequest,
            Universal::IdentityReply(DeviceIdentity {
                manufacturer: ManufacturerId::of(0x41),
                family: [U7::of(1), U7::of(2)],
                model: [U7::of(3), U7::of(4)],
                revision: SoftwareRevision::from([U7::of(5), U7::of(6), U7::of(7), U7::of(8)]),
            }),
            Universal::GeneralMidi(GeneralMidiMode::On),
            Universal::MasterVolume(U14::of(0x1234)),
            Universal::MasterBalance(U14::of(0x0102)),
            Universal::MasterFineTuning(U14::of(0x2000)),
            Universal::MasterCoarseTuning(U7::of(0x40)),
            Universal::Tuning(TuningMessage::DumpRequest {
                bank: None,
                program: U7::of(3),
            }),
            Universal::MtcFullMessage(
                MtcFullMessage::new(
                    MtcRate::Fps30,
                    TimecodeHours::of(1),
                    TimecodeMinutes::of(2),
                    TimecodeSeconds::of(3),
                    4,
                )
                .unwrap(),
            ),
            Universal::MachineControl(MmcCommands::stop()),
        ];
        let mut seen = [false; 10];
        for message in samples {
            seen[index(&message)] = true;
            for to in [Recipient::AllCall, Recipient::Device(SysexDeviceId::of(5))] {
                let payload = Addressed { to, message }.payload();
                let mut framed = alloc::vec![0xF0];
                framed.extend(payload.as_bytes().iter().map(|b| b.get()));
                framed.push(0xF7);
                let read = read(SysexMessage::from_framed(&framed).unwrap());
                assert_eq!(read, UniversalRead::Universal(Addressed { to, message }));
            }
        }
        assert!(seen.iter().all(|&s| s), "a variant has no sample");
    }
}
