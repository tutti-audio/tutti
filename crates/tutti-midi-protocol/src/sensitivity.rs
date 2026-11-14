//! Per-note pitch-bend sensitivity as the MIDI 2.0 wire value.

use tutti_types::Semitones;
use tutti_ump::{Pitch, U7, U14};

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
        let cents = (data.lsb.get() as u64) << Self::FRAC_BITS;
        let fraction = (cents + 50) / 100;
        let bits = ((data.msb.get() as u64) << Self::FRAC_BITS) + fraction;
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
        RegisteredData {
            msb: U7::masked((hundredths / 100) as u8),
            lsb: U7::masked((hundredths % 100) as u8),
        }
    }

    /// The sensitivity in semitones (integer and fractional parts). The `f32`
    /// rounds the 32-bit word: a word near the top reads as 128.0.
    #[inline]
    #[must_use]
    pub fn semitones(self) -> Semitones {
        Semitones::new(Pitch::from_q7_25(self.0).note_number())
    }
}

/// The data of a Registered Controller whose value is two 7-bit fields (M2-104
/// section 7.4.7.1, M2-115 section 4.1): the MSB in the top 7 bits of the 32-bit
/// MIDI 2.0 value, the LSB in the 7 below, and the 18 bits under them unused.
/// The pitch bend range is whole semitones and cents, the modulation depth range
/// semitones and 100/128 cents, the channel tuning a 14-bit value or its MSB.
///
/// ```
/// use tutti_midi_protocol::RegisteredData;
/// use tutti_ump::U7;
///
/// let data = RegisteredData::new(U7::of(2), U7::of(50));
/// assert_eq!(data.to_word(), 2 << 25 | 50 << 18);
/// assert_eq!(RegisteredData::from_word(data.to_word()), data);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegisteredData {
    msb: U7,
    lsb: U7,
}

impl RegisteredData {
    /// The data of `msb` and `lsb`.
    #[must_use]
    pub const fn new(msb: U7, lsb: U7) -> Self {
        Self { msb, lsb }
    }

    /// The two fields of a 32-bit value; the 18 bits below them are ignored.
    #[must_use]
    pub const fn from_word(word: u32) -> Self {
        Self {
            msb: U7::masked((word >> 25) as u8),
            lsb: U7::masked((word >> 18) as u8),
        }
    }

    /// The 32-bit value, zero-extended below the fields.
    #[must_use]
    pub const fn to_word(self) -> u32 {
        (self.msb.get() as u32) << 25 | (self.lsb.get() as u32) << 18
    }

    /// The data of a 14-bit value `MSB << 7 | LSB`.
    #[must_use]
    pub const fn from_fourteen(value: U14) -> Self {
        Self {
            msb: value.msb(),
            lsb: value.lsb(),
        }
    }

    /// The fields as the 14-bit value `MSB << 7 | LSB`.
    #[must_use]
    pub const fn fourteen(self) -> U14 {
        U14::from_halves(self.msb, self.lsb)
    }

    /// The most significant field.
    #[must_use]
    pub const fn msb(self) -> U7 {
        self.msb
    }

    /// The least significant field.
    #[must_use]
    pub const fn lsb(self) -> U7 {
        self.lsb
    }
}

#[cfg(test)]
mod tests;
