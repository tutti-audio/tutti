//! The MIDI Tuning Standard's tables: what a tuning program stores and what a
//! channel's Scale/Octave Tuning adds.
//!
//! [`Tunings`] is the value [`VoiceAllocator`](crate::VoiceAllocator) keeps. A key's tuning is
//! its **offset** from equal temperament, in semitones: the stored word's pitch
//! (`TuningFrequency::pitch`, semitones above note 0) minus the key number, plus
//! the channel's Scale/Octave offset for the key's pitch class. A voice carries
//! it as [`VoiceCommand::Start::tune`](crate::VoiceCommand::Start) (and `Retarget`, and
//! `Tune` when it changes), and the channel's own pitch (the wheel, RPN 1 and 2,
//! the master tuning) stays a [`VoiceCommand::Retune`](crate::VoiceCommand::Retune): the
//! two add. Nothing here is in Hz, so the reference (A440 at note 69) is the
//! master tuning's alone.
//!
//! # What is held
//!
//! MTS asks for 128 tuning programs as a maximum: an instrument may have
//! fewer, and ignores a Tuning Program or Bank number it has none for. So the capacity is a [`TuningCapacity`], banks and programs a
//! bank, chosen with the [`VoiceConfig`](crate::VoiceConfig) (default one bank of 128
//! programs, 64 KiB of tables: the spec's temporary copies of tuning tables are one
//! 512-byte table a program here), allocated once in
//! [`VoiceAllocator::new`](crate::VoiceAllocator::new). A message
//! for a bank or program beyond it changes nothing and is counted
//! ([`VoiceAllocator::losses`](crate::VoiceAllocator::losses)).
//!
//! Every program starts as equal temperament (RP-020: the initial tuning is equal temperament, in Bank 0H
//! and Preset 0H). A message
//! with no bank byte (the original Bulk Tuning Dump and Single Note Tuning
//! Change) is for bank 0: the spec gives it no bank to name.
//!
//! The Scale/Octave offsets are per channel, not per program: the message offsets the tuning
//! from the currently selected preset, and each message offsets from the
//! original preset, not from the modified tuning, so a message replaces the channel's twelve offsets.

use alloc::boxed::Box;
use alloc::vec;
use tutti_midi_protocol::{KeyTable, OctaveOffsets, TuningFrequency};
use tutti_music::PitchClass;
use tutti_ump::{NoteNumber, U7};
use tutti_units::{Losses, Semitones};

/// How many tuning banks and programs a bank a core holds: each from 1 to 128
/// (a bank and a program are 7-bit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuningCapacity {
    banks: u8,
    programs: u8,
}

impl TuningCapacity {
    /// One bank of all 128 programs.
    pub const DEFAULT: Self = Self {
        banks: 1,
        programs: 128,
    };

    /// `banks` banks of `programs` programs each, `None` for a count of 0 or
    /// above 128.
    #[must_use]
    pub const fn new(banks: u8, programs: u8) -> Option<Self> {
        if banks == 0 || banks > 128 || programs == 0 || programs > 128 {
            None
        } else {
            Some(Self { banks, programs })
        }
    }

    /// The number of banks.
    #[must_use]
    pub const fn banks(self) -> u8 {
        self.banks
    }

    /// The number of programs in a bank.
    #[must_use]
    pub const fn programs(self) -> u8 {
        self.programs
    }
}

impl Default for TuningCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A tuning bank or program the core has no room for
/// ([`TuningCapacity`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no tuning program {} in bank {}", .program.get(), .bank.get())]
pub struct NoSuchTuning {
    /// The bank asked for.
    pub bank: U7,
    /// The program asked for.
    pub program: U7,
}

/// The tuning programs and the channels' Scale/Octave offsets. See the module
/// docs.
#[derive(Clone, Debug)]
pub(crate) struct Tunings {
    capacity: TuningCapacity,
    /// `banks * programs` tables, a bank's programs together.
    tables: Box<[KeyTable]>,
    /// Each channel's Scale/Octave offsets, C to B.
    octave: [[Semitones; 12]; 16],
    /// Messages for a bank or program there is no room for.
    refused: Losses,
}

impl Tunings {
    pub(crate) fn new(capacity: TuningCapacity) -> Self {
        let n = usize::from(capacity.banks) * usize::from(capacity.programs);
        Self {
            capacity,
            tables: vec![KeyTable::equal_temperament(); n].into_boxed_slice(),
            octave: [[Semitones::new(0.0); 12]; 16],
            refused: Losses::NONE,
        }
    }

    pub(crate) fn losses(&self) -> Losses {
        self.refused
    }

    /// Counts one message that named no program or bank we have.
    pub(crate) fn refuse(&mut self) {
        self.refused += Losses::refused(1);
    }

    pub(crate) fn has_bank(&self, bank: U7) -> bool {
        bank.get() < self.capacity.banks
    }

    pub(crate) fn has_program(&self, program: U7) -> bool {
        program.get() < self.capacity.programs
    }

    fn index(&self, bank: U7, program: U7) -> Option<usize> {
        (self.has_bank(bank) && self.has_program(program)).then(|| {
            usize::from(bank.get()) * usize::from(self.capacity.programs)
                + usize::from(program.get())
        })
    }

    pub(crate) fn table(&self, bank: U7, program: U7) -> Option<&KeyTable> {
        self.index(bank, program).map(|i| &self.tables[i])
    }

    /// The table of `bank` and `program`, to change; counted when there is none.
    pub(crate) fn table_mut(&mut self, bank: U7, program: U7) -> Option<&mut KeyTable> {
        let Some(i) = self.index(bank, program) else {
            self.refused += Losses::refused(1);
            return None;
        };
        Some(&mut self.tables[i])
    }

    /// Replaces a table, uncounted; whether there was one.
    pub(crate) fn store(&mut self, bank: U7, program: U7, table: KeyTable) -> bool {
        let Some(i) = self.index(bank, program) else {
            return false;
        };
        self.tables[i] = table;
        true
    }

    /// The table a Scale/Octave Tuning Dump stores: equal temperament with the
    /// offsets on each pitch class.
    pub(crate) fn octave_table(offsets: &OctaveOffsets) -> KeyTable {
        let mut table = KeyTable::equal_temperament();
        for k in 0..128u8 {
            let key = NoteNumber::of(k);
            let offset = offsets
                .offset(PitchClass::from_semitones_above_c(k))
                .to_semitones();
            table.set(
                key,
                TuningFrequency::nearest(Semitones::new(f64::from(k)) + offset),
            );
        }
        table
    }

    /// Replaces channel `channel`'s twelve Scale/Octave offsets.
    pub(crate) fn set_octave(&mut self, channel: usize, offsets: &OctaveOffsets) {
        for (class, slot) in PitchClass::ALL.into_iter().zip(&mut self.octave[channel]) {
            *slot = offsets.offset(class).to_semitones();
        }
    }

    pub(crate) fn clear_octave(&mut self) {
        self.octave = [[Semitones::new(0.0); 12]; 16];
    }

    /// How far `key` sounds from its equal-tempered pitch on `channel`, which
    /// has `bank` and `program` selected.
    pub(crate) fn offset(
        &self,
        bank: U7,
        program: U7,
        channel: usize,
        key: NoteNumber,
    ) -> Semitones {
        let stored = self
            .table(bank, program)
            .and_then(|t| t.get(key).pitch())
            .map_or(Semitones::new(0.0), |pitch| {
                pitch - Semitones::new(f64::from(key.get()))
            });
        stored + self.octave[channel][usize::from(key.get() % 12)]
    }
}
