//! The address of a Registered Parameter Number.

use crate::{U7, U14};

/// The address of a Registered Controller (RPN): a 7-bit bank and a 7-bit
/// index (M2-104 section 7.4.7), the two halves a
/// [`Voice2::Registered`](crate::Voice2::Registered) carries.
///
/// The addresses tutti reads are named constants; any other address is
/// [`new`](Self::new).
///
/// ```
/// use tutti_ump::{ControlValue, Rpn, U7, Voice2};
///
/// let message = Voice2::Registered {
///     bank: U7::of(0),
///     index: U7::of(7),
///     value: ControlValue::new(24 << 25),
/// };
/// let Voice2::Registered { bank, index, .. } = message else { unreachable!() };
/// assert_eq!(Rpn::new(bank, index), Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rpn {
    bank: U7,
    index: U7,
}

impl Rpn {
    /// RPN #00/00, **Channel Pitch Bend Sensitivity**. Its data is the classic
    /// MSB semitones, LSB cents form, not the per-note 7.25 fixed point.
    pub const PITCH_BEND_SENSITIVITY: Self = Self::new(U7::of(0), U7::of(0));

    /// RPN #00/01, **Channel Fine Tuning** (RP-022 renamed it from Master Fine
    /// Tuning): 14 bits centred at `0x2000`, 100 cents over the half range.
    pub const CHANNEL_FINE_TUNING: Self = Self::new(U7::of(0), U7::of(1));

    /// RPN #00/02, **Channel Coarse Tuning** (RP-022): the data MSB is semitones
    /// centred at 64; the LSB is not used.
    pub const CHANNEL_COARSE_TUNING: Self = Self::new(U7::of(0), U7::of(2));

    /// RPN #00/03, **Tuning Program Select** (MIDI Tuning Standard): the data
    /// MSB is the tuning program, 0 to 127 (the tuning spec numbers them 1 to
    /// 128).
    pub const TUNING_PROGRAM_SELECT: Self = Self::new(U7::of(0), U7::of(3));

    /// RPN #00/04, **Tuning Bank Select** (MIDI Tuning Standard): the data MSB
    /// is the tuning bank, 0 to 127.
    pub const TUNING_BANK_SELECT: Self = Self::new(U7::of(0), U7::of(4));

    /// RPN #00/05, **Modulation Depth Range** (CA-026): scales the effective
    /// range of the Modulation Wheel (CC 1). The MSB is semitones and the LSB
    /// 100/128 cent, as GM2 section 3.4.4 words it.
    pub const MODULATION_DEPTH_RANGE: Self = Self::new(U7::of(0), U7::of(5));

    /// RPN #00/06, the **MPE Configuration Message** (RP-053; M2-104 lists it as
    /// RPN 0x0006, "MPE MCM"). Its data is the member-channel count of the zone
    /// the message's channel names; `0` disables the zone.
    pub const MPE_CONFIGURATION: Self = Self::new(U7::of(0), U7::of(6));

    /// RPN #00/07, **Sensitivity of Per-Note Pitch Bend** (M2-104 section
    /// 7.4.13.1). Its data is a 7.25 fixed-point semitone range, shared by every
    /// note number on the channel.
    pub const PER_NOTE_PITCH_BEND_SENSITIVITY: Self = Self::new(U7::of(0), U7::of(7));

    /// The address of `bank` and `index`.
    #[inline]
    #[must_use]
    pub const fn new(bank: U7, index: U7) -> Self {
        Self { bank, index }
    }

    /// The bank (the RPN MSB).
    #[inline]
    #[must_use]
    pub const fn bank(self) -> U7 {
        self.bank
    }

    /// The index (the RPN LSB).
    #[inline]
    #[must_use]
    pub const fn index(self) -> U7 {
        self.index
    }
}

/// The data of a Registered Controller whose value is two 7-bit fields (M2-104
/// section 7.4.7.1, M2-115 section 4.1): the MSB in the top 7 bits of the 32-bit
/// MIDI 2.0 value, the LSB in the 7 below, and the 18 bits under them unused.
/// The pitch bend range is whole semitones and cents, the modulation depth range
/// semitones and 100/128 cents, the channel tuning a 14-bit value or its MSB.
///
/// ```
/// use tutti_ump::{RegisteredData, U7};
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
mod tests {
    use super::*;

    /// Each constant against the number the spec gives it, in one table
    /// (M2-104 sections 7.4.7 and 7.4.13.1: bank, index).
    #[test]
    fn the_named_addresses_are_the_specs_numbers() {
        let table = [
            (Rpn::PITCH_BEND_SENSITIVITY, 0u8, 0u8),
            (Rpn::CHANNEL_FINE_TUNING, 0, 1),
            (Rpn::CHANNEL_COARSE_TUNING, 0, 2),
            (Rpn::TUNING_PROGRAM_SELECT, 0, 3),
            (Rpn::TUNING_BANK_SELECT, 0, 4),
            (Rpn::MODULATION_DEPTH_RANGE, 0, 5),
            (Rpn::MPE_CONFIGURATION, 0, 6),
            (Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY, 0, 7),
        ];
        for (rpn, bank, index) in table {
            assert_eq!((rpn.bank().get(), rpn.index().get()), (bank, index));
        }
    }

    #[test]
    fn new_keeps_both_halves() {
        let rpn = Rpn::new(U7::of(0x12), U7::of(0x34));
        assert_eq!((rpn.bank().get(), rpn.index().get()), (0x12, 0x34));
        assert_ne!(rpn, Rpn::new(U7::of(0x34), U7::of(0x12)));
    }

    /// The two fields are the top 14 bits, and the 18 bits under them are ignored.
    /// Spec: m2-104/7.4.7.1#1
    #[test]
    fn registered_data_reads_the_top_fourteen_bits() {
        let data = RegisteredData::from_word(0xFFFF_FFFF);
        assert_eq!((data.msb().get(), data.lsb().get()), (127, 127));
        assert_eq!(data.fourteen(), U14::MAX);
        assert_eq!(
            RegisteredData::from_fourteen(U14::of(0x2001)).to_word(),
            0x2001 << 18
        );
        assert_eq!(RegisteredData::from_word(0x0003_FFFF).to_word(), 0);
    }
}
