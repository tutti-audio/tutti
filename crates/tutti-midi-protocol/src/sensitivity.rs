//! Per-note pitch-bend sensitivity as the MIDI 2.0 wire value.

use tutti_types::Semitones;
use tutti_ump::{Pitch, RegisteredData, U7};

/// Per-note pitch bend range as the spec's 7.25 fixed-point semitone value
/// (M2-104 §7.4.13, RPN #00/07, Sensitivity of Per-Note Pitch Bend): 7 integer
/// bits of semitones in the high word, 25 fractional bits below. This is the wire
/// form the MPE Configuration Message (RPN 0x0006) carries — modelling it as the
/// fixed-point value, not a bare `u8`, is what lets it round-trip through UMP.
///
/// Every `u32` is a sensitivity (§7.4.13.3: just under 128 semitones; the top
/// words round to 128.0 in `f32`), so the bits have no refused value; only a
/// whole-semitone count can be out of range ([`new`](Self::new), [`of`](Self::of)).
/// Serde carries the bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PitchBendSensitivity(u32);

impl Default for PitchBendSensitivity {
    /// The MPE default per-note bend range: ±48 semitones (MPE spec RP-053).
    #[inline]
    fn default() -> Self {
        Self::MPE_MEMBER_DEFAULT
    }
}

impl PitchBendSensitivity {
    /// Fraction bits of the Q7.25 word (M2-104 section 7.4.13).
    const FRAC_BITS: u32 = 25;

    /// The bend range an MPE 1.1 master channel starts at, and an MPE
    /// Configuration Message resets it to: 2 semitones (M1-100 section 2.2.5).
    pub const MPE_MASTER_DEFAULT: Self = Self(2 << Self::FRAC_BITS);

    /// The bend range an MPE 1.1 member channel starts at, and an MPE
    /// Configuration Message resets it to: the per-note default of 48 semitones
    /// (MPE spec RP-053, M1-100 section 2.2.5).
    pub const MPE_MEMBER_DEFAULT: Self = Self(48 << Self::FRAC_BITS);

    /// The bend range a MIDI-CI MPE Profile zone starts at, for its Manager
    /// and every Member: 48 semitones (M2-120 section 3.5).
    pub const MPE_PROFILE_DEFAULT: Self = Self(48 << Self::FRAC_BITS);

    /// The widest whole-semitone sensitivity: 7 integer bits.
    pub(crate) const MAX_SEMITONES: u8 = 127;

    /// Whole-semitone sensitivity (the common MPE case, e.g. 48), or `None`
    /// above `MAX_SEMITONES`. A `u8` count, not
    /// `Semitones`: that is an `f32`, and this is an exact integer count.
    #[inline]
    #[must_use]
    pub const fn new(semitones: u8) -> Option<Self> {
        if semitones <= Self::MAX_SEMITONES {
            Some(Self((semitones as u32) << Self::FRAC_BITS))
        } else {
            None
        }
    }

    /// [`new`](Self::new) for a literal or a constant.
    ///
    /// # Panics
    ///
    /// Above `MAX_SEMITONES`.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(semitones: u8) -> Self {
        match Self::new(semitones) {
            Some(s) => s,
            None => panic!("a pitch bend sensitivity is at most 127 semitones"),
        }
    }

    /// The raw 32-bit RPN data field.
    #[inline]
    #[must_use]
    pub const fn to_rpn_bits(self) -> u32 {
        self.0
    }

    /// From the raw 32-bit RPN data field. Total: every value is in range.
    #[inline]
    #[must_use]
    pub const fn from_rpn_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The sensitivity a Channel Pitch Bend Sensitivity message (RPN #00/00)
    /// sets: its MSB is whole semitones and its LSB cents (M2-104 section 7.4.7.1,
    /// M2-115 section 4.1). A cents field of 100 or more reads as the cents it
    /// says, and the top saturates at the largest word.
    #[must_use]
    pub const fn from_channel_data(data: RegisteredData) -> Self {
        let cents = (data.lsb().get() as u64) << Self::FRAC_BITS;
        let fraction = (cents + 50) / 100;
        let bits = ((data.msb().get() as u64) << Self::FRAC_BITS) + fraction;
        Self(if bits > u32::MAX as u64 {
            u32::MAX
        } else {
            bits as u32
        })
    }

    /// The data of a Channel Pitch Bend Sensitivity message (RPN #00/00) that
    /// sets this sensitivity: whole semitones and cents, rounded to the nearest
    /// cent (a word within half a cent of 128 semitones saturates at 127
    /// semitones 99 cents).
    #[must_use]
    pub const fn to_channel_data(self) -> RegisteredData {
        let hundredths = (self.0 as u64 * 100 + (1 << (Self::FRAC_BITS - 1))) >> Self::FRAC_BITS;
        let hundredths = if hundredths > 12_799 {
            12_799
        } else {
            hundredths
        };
        RegisteredData::new(
            U7::masked((hundredths / 100) as u8),
            U7::masked((hundredths % 100) as u8),
        )
    }

    /// The sensitivity in semitones (integer and fractional parts). The `f32`
    /// rounds the 32-bit word: a word near the top reads as 128.0.
    #[inline]
    #[must_use]
    pub fn semitones(self) -> Semitones {
        Semitones::new(Pitch::from_q7_25(self.0).note_number())
    }
}

#[cfg(test)]
mod tests;
