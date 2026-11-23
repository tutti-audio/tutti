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

/// A message a sender cannot name: the variants of [`Universal`] that are only
/// ever read, or a combination the grammar has no bytes for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("{0} cannot be written")]
pub struct NotWritable(pub &'static str);

/// The payload of one Universal message: the bytes between `F0` and `F7`, held
/// inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UniversalPayload {
    buf: [u8; UNIVERSAL_MAX_PAYLOAD],
    len: usize,
}

impl UniversalPayload {
    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn new() -> Self {
        Self {
            buf: [0; UNIVERSAL_MAX_PAYLOAD],
            len: 0,
        }
    }

    // The callers write at most `UNIVERSAL_MAX_PAYLOAD` bytes, the longest message.
    fn put(&mut self, byte: u8) {
        self.buf[self.len] = byte;
        self.len += 1;
    }

    fn put_all(&mut self, bytes: impl IntoIterator<Item = u8>) {
        for byte in bytes {
            self.put(byte);
        }
    }

    fn put_u7(&mut self, value: U7) {
        self.put(value.get());
    }

    /// A 14-bit value, least significant byte first (M1 p.40, p.57).
    fn put_u14_lsb_first(&mut self, value: U14) {
        self.put_all(value.to_le7());
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
        self.put_all(name.bytes().iter().map(|b| b.get()));
    }

    fn put_offsets(&mut self, offsets: &OctaveOffsets) {
        match offsets {
            OctaveOffsets::Cents(bytes) => self.put_all(bytes.iter().map(|b| b.get())),
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
        let sum = self.as_bytes().iter().fold(0, |acc, b| acc ^ b) & 0x7F;
        self.put(sum);
    }

    fn put_identity(&mut self, identity: &DeviceIdentity) {
        let [a, b, c] = identity.manufacturer.sysex_bytes();
        if identity.manufacturer.is_extended() {
            self.put_all([a, b, c].map(U7::get));
        } else {
            self.put_u7(a);
        }
        self.put_all(identity.family.map(U7::get));
        self.put_all(identity.model.map(U7::get));
        self.put_all(identity.revision.get().map(U7::get));
    }

    fn put_tuning(&mut self, message: &TuningMessage) -> Result<(), NotWritable> {
        match message {
            TuningMessage::DumpRequest {
                bank: None,
                program,
            } => {
                self.put_all([0x08, 0x00, program.get()]);
            }
            TuningMessage::DumpRequest {
                bank: Some(bank),
                program,
            } => self.put_all([0x08, 0x03, bank.get(), program.get()]),
            TuningMessage::KeyDump {
                bank,
                program,
                name,
                table,
            } => {
                match bank {
                    None => self.put_all([0x08, 0x01, program.get()]),
                    Some(bank) => self.put_all([0x08, 0x04, bank.get(), program.get()]),
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
                self.put_all([0x08, sub, bank.get(), program.get()]);
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
                    (Timing::RealTime, None) => self.put_all([0x08, 0x02, program.get()]),
                    (_, Some(bank)) => self.put_all([0x08, 0x07, bank.get(), program.get()]),
                    (Timing::NonRealTime, None) => {
                        return Err(NotWritable("a non-real-time note change with no bank"));
                    }
                }
                // At most `MAX_KEY_CHANGES`, a data byte.
                self.put(changes.as_slice().len() as u8);
                for change in changes.as_slice() {
                    self.put(change.key.get());
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
                self.put_all([
                    0x08,
                    sub,
                    (bits >> 14) as u8,
                    (bits >> 7) as u8 & 0x7F,
                    bits as u8 & 0x7F,
                ]);
                self.put_offsets(offsets);
            }
        }
        Ok(())
    }
}

impl core::ops::Deref for UniversalPayload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for UniversalPayload {
    fn as_ref(&self) -> &[u8] {
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
    /// [`Sysex7Packets::from_payload`](tutti_ump::Sysex7Packets::from_payload).
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
            out.put_all([class(timing), device]);
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
                out.put_all(bytes);
            }
            Universal::MachineControl(commands) => {
                out.put_all(commands.payload(self.to).as_bytes().iter().copied());
            }
            Universal::MachineControlTruncated(_) => {
                return Err(NotWritable("MachineControlTruncated"));
            }
            Universal::MachineControlResponse(_) => {
                return Err(NotWritable("MachineControlResponse"));
            }
        }
        Ok(out)
    }
}
