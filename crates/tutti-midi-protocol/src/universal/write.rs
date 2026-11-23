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
    MAX_KEY_CHANGES, OctaveOffsets, Timing, TuningFrequency, TuningMessage, TuningName,
};

/// The longest payload [`Addressed::payload`] writes: a Single Note Tuning
/// Change of [`MAX_KEY_CHANGES`] changes with a bank (`7F dev 08 07 bank prog
/// ll` and four bytes a change), the longest message of the grammar.
pub const UNIVERSAL_MAX_PAYLOAD: usize = 7 + 4 * MAX_KEY_CHANGES;

/// Why an [`Addressed`] message has no payload to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum NotWritable {
    /// [`Universal::MachineControlTruncated`]: a command segment cut short,
    /// which a receiver makes and no sender names (RP-013 p.40 b).
    #[error("a truncated Machine Control command is only read, never written")]
    MachineControlTruncated,
    /// [`Universal::MachineControlResponse`]: a Response (`07`) comes from a
    /// device, and is read.
    #[error("a Machine Control response is only read, never written")]
    MachineControlResponse,
    /// A Non-Real Time Single Note Tuning Change names its bank (`08 07`); the
    /// bankless form (`08 02`) is Real Time (CA-020).
    #[error("a Non-Real Time Single Note Tuning Change names a bank")]
    NonRealTimeNoteChangeWithoutBank,
}

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

    fn put_tuning(&mut self, message: &TuningMessage) -> Result<(), NotWritable> {
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
                timing,
                bank,
                program,
                changes,
            } => {
                match (timing, bank) {
                    (Timing::RealTime, None) => {
                        self.put_all([0x08, 0x02]);
                        self.put_u7(*program);
                    }
                    (_, Some(bank)) => {
                        self.put_all([0x08, 0x07]);
                        self.put_u7s([*bank, *program]);
                    }
                    (Timing::NonRealTime, None) => {
                        return Err(NotWritable::NonRealTimeNoteChangeWithoutBank);
                    }
                }
                // At most `MAX_KEY_CHANGES` (127), a data byte.
                self.put_u7(U7::masked(changes.as_slice().len() as u8));
                for change in changes.as_slice() {
                    self.put_u7(U7::masked(change.key.get()));
                    self.put_frequency(change.frequency);
                }
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
        Ok(())
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
    ///
    /// # Errors
    ///
    /// [`NotWritable`] for a message that is only read
    /// ([`Universal::MachineControlTruncated`] and
    /// [`Universal::MachineControlResponse`], which no sender names) and for a
    /// non-real-time Single Note Tuning Change with no bank, which has no
    /// bytes.
    pub fn payload(&self) -> Result<UniversalPayload, NotWritable> {
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
                    TuningMessage::NoteChange { timing, .. }
                    | TuningMessage::OctaveTuning { timing, .. } => *timing,
                    _ => Timing::NonRealTime,
                };
                header(&mut out, timing);
                out.put_tuning(message)?;
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
            Universal::MachineControlTruncated(_) => {
                return Err(NotWritable::MachineControlTruncated);
            }
            Universal::MachineControlResponse(_) => {
                return Err(NotWritable::MachineControlResponse);
            }
        }
        Ok(out)
    }
}
