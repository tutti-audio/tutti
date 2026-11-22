//! Wire-width integers: a value of a stated range, so a field cannot hold one
//! the packet cannot carry.
//!
//! The public constructor refuses a value out of range, and no constructor
//! stores one. The codec builds a bit-width field from bits it has just masked,
//! through `masked`; a bounded value out of range it reads as a `Field::Reserved`.

use crate::ControlValue;
use tutti_music::Note;

tutti_units::ranged! {
    /// A 4-bit value: an MDS id, a chord degree.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash, bytemuck::NoUninit)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    U4(u8), U4OutOfRange, bits 4
}
tutti_units::ranged! {
    /// A 7-bit data byte (`0..=127`): a velocity, a pressure, a program, a
    /// controller value, a SysEx byte.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash, bytemuck::NoUninit)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    U7(u8), U7OutOfRange, bits 7
}
tutti_units::ranged! {
    /// A 14-bit value (`0..=16383`): a MIDI 1.0 pitch bend, a song position.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash, bytemuck::NoUninit)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
    U14(u16), U14OutOfRange, bits 14
}
tutti_units::ranged! {
    /// A 28-bit value (`0..=0x0FFF_FFFF`): a MIDI-CI MUID, a 28-bit size.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash, bytemuck::NoUninit)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
    U28(u32), U28OutOfRange, bits 28
}
tutti_units::ranged! {
    /// A MIDI note number on the wire (`0..=127`), not a musical pitch.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash, bytemuck::NoUninit)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    NoteNumber(u8), NoteNumberOutOfRange, bits 7
}

/// The error converting a [`Note`] outside MIDI's `0..=127` note numbers to a
/// [`NoteNumber`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} has no MIDI note number")]
pub struct NotOnMidiScale(pub Note);

/// MIDI's note-number encoding of a pitch: number 0 is C-1 and 69 is A4.
impl From<NoteNumber> for Note {
    /// Every note number names a note, so this cannot fail.
    fn from(number: NoteNumber) -> Self {
        Note::from_semitones_from_c0(i32::from(number.get()) - MIDI_C0_OFFSET)
    }
}

impl TryFrom<Note> for NoteNumber {
    type Error = NotOnMidiScale;

    /// A note above G9 or below C-1 has no number and is refused.
    fn try_from(note: Note) -> Result<Self, Self::Error> {
        u8::try_from(note.semitones_from_c0() + MIDI_C0_OFFSET)
            .ok()
            .and_then(NoteNumber::new)
            .ok_or(NotOnMidiScale(note))
    }
}

/// MIDI note 0 is C-1, twelve semitones below C0.
const MIDI_C0_OFFSET: i32 = 12;
tutti_units::ranged! {
    /// A Delta Clockstamp: ticks since the previous event (20 bits).
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash, bytemuck::NoUninit)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
    DeltaTicks(u32), DeltaTicksOutOfRange, bits 20
}

impl U14 {
    /// `0x2000`, a pitch bend at rest (M1).
    pub const CENTER: Self = Self::of(0x2000);

    /// The value whose upper seven bits are `msb` and lower seven `lsb`: the
    /// inverse of [`msb`](Self::msb) and [`lsb`](Self::lsb).
    #[inline]
    #[must_use]
    pub const fn from_halves(msb: U7, lsb: U7) -> Self {
        Self((msb.get() as u16) << 7 | lsb.get() as u16)
    }

    /// The upper seven bits: the second data byte of a pitch bend or a song
    /// position.
    #[inline]
    #[must_use]
    pub const fn msb(self) -> U7 {
        U7::masked((self.0 >> 7) as u8)
    }

    /// The lower seven bits: the first data byte.
    #[inline]
    #[must_use]
    pub const fn lsb(self) -> U7 {
        U7::masked((self.0 & 0x7F) as u8)
    }

    /// The two 7-bit wire bytes, least significant first (the MIDI-CI and
    /// MIDI Tuning order).
    #[inline]
    #[must_use]
    pub const fn to_le7(self) -> [u8; 2] {
        [self.lsb().get(), self.msb().get()]
    }

    /// The value of a length, or `None` above 14 bits.
    #[inline]
    #[must_use]
    pub const fn from_len(len: usize) -> Option<Self> {
        if len <= Self::MAX.0 as usize {
            Some(Self(len as u16))
        } else {
            None
        }
    }

    /// The value of a length held to 14 bits: a length above [`MAX`](Self::MAX)
    /// is [`MAX`](Self::MAX). For a collection whose type bounds its length
    /// to 14 bits, which has no length to refuse.
    #[inline]
    #[must_use]
    pub const fn saturating_from_len(len: usize) -> Self {
        match Self::from_len(len) {
            Some(value) => value,
            None => Self::MAX,
        }
    }

    /// The value of two 7-bit wire bytes, least significant first, or `None`
    /// when a byte has its top bit set.
    #[inline]
    #[must_use]
    pub const fn from_le7(bytes: [u8; 2]) -> Option<Self> {
        match (U7::new(bytes[0]), U7::new(bytes[1])) {
            (Some(lsb), Some(msb)) => Some(Self::from_halves(msb, lsb)),
            _ => None,
        }
    }
}

impl U28 {
    /// The four 7-bit wire bytes, least significant first.
    #[inline]
    #[must_use]
    pub const fn to_le7(self) -> [u8; 4] {
        [
            (self.0 & 0x7F) as u8,
            ((self.0 >> 7) & 0x7F) as u8,
            ((self.0 >> 14) & 0x7F) as u8,
            (self.0 >> 21) as u8,
        ]
    }

    /// The value of a length, or `None` above 28 bits.
    #[inline]
    #[must_use]
    pub const fn from_len(len: usize) -> Option<Self> {
        if len <= Self::MAX.0 as usize {
            Some(Self(len as u32))
        } else {
            None
        }
    }

    /// The value of four 7-bit wire bytes, least significant first, or `None`
    /// when a byte has its top bit set.
    #[inline]
    #[must_use]
    pub const fn from_le7(bytes: [u8; 4]) -> Option<Self> {
        if (bytes[0] | bytes[1] | bytes[2] | bytes[3]) & 0x80 != 0 {
            return None;
        }
        Some(Self(
            (bytes[0] as u32)
                | (bytes[1] as u32) << 7
                | (bytes[2] as u32) << 14
                | (bytes[3] as u32) << 21,
        ))
    }
}

/// The index of the first byte of `bytes` with its top bit set, or `None` when
/// every byte is a 7-bit data byte.
#[must_use]
pub fn first_non_seven_bit(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b & 0x80 != 0)
}

tutti_units::ranged! {
    /// A Function Block number (`0..=31`). A reserved number read from the
    /// wire is a [`Field::Reserved`](crate::Field::Reserved), not a value of
    /// this type.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    FunctionBlockId(u8), FunctionBlockIdOutOfRange, "within 0..=31", |raw| raw <= 31
}
tutti_units::ranged! {
    /// A Group number as a Function Block declares its first Group (`0..=15`).
    /// A reserved number read from the wire is a
    /// [`Field::Reserved`](crate::Field::Reserved).
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    FirstGroup(u8), FirstGroupOutOfRange, "within 0..=15", |raw| raw <= 15
}
tutti_units::ranged! {
    /// How many Groups a Function Block spans (`1..=16`). A reserved count read
    /// from the wire is a [`Field::Reserved`](crate::Field::Reserved).
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    GroupSpan(u8), GroupSpanOutOfRange, "within 1..=16", |raw| raw >= 1 && raw <= 16
}
tutti_units::ranged! {
    /// How many channels an MPE Profile zone has, its Manager Channel and one
    /// to fifteen Member Channels: `2..=16` (M2-120 §2.1).
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    MpeChannels(u8), MpeChannelsOutOfRange, "within 2..=16", |raw| raw >= 2 && raw <= 16
}
tutti_units::ranged! {
    /// A chord type, `0x00..=0x1B` (Table 14). A reserved type read from the
    /// wire is a [`Field::Reserved`](crate::Field::Reserved), not a value of
    /// this type.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    ChordType(u8), ChordTypeOutOfRange, "within 0..=0x1B", |raw| raw <= 0x1B
}

tutti_units::ranged! {
    /// How many Function Blocks an Endpoint Info Notification declares
    /// (`0..=32`, M2-104 §7.1.2). A reserved count (`33..=127`) read from the
    /// wire is a [`FunctionBlocks::Reserved`](crate::FunctionBlocks::Reserved),
    /// not a value of this type.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    FunctionBlockCount(u8), FunctionBlockCountOutOfRange, "within 0..=32", |raw| raw <= 32
}

impl FunctionBlockCount {
    /// The most blocks an Endpoint declares.
    pub const MAX: Self = Self(32);

    /// The count of `blocks` blocks, or [`MAX`](Self::MAX) for more: an OS that
    /// lists more than an Endpoint holds is read as one that lists them all.
    #[must_use]
    pub fn saturating(blocks: usize) -> Self {
        u8::try_from(blocks)
            .ok()
            .and_then(Self::new)
            .unwrap_or(Self::MAX)
    }
}

tutti_units::ranged! {
    /// The Delta Clockstamp ticks per quarter note (`1..=65535`, M2-104
    /// §7.2.3.1); `0` is reserved. A zero read from the wire is a
    /// [`Utility::ReservedTicksPerQuarterNote`](crate::Utility::ReservedTicksPerQuarterNote),
    /// not a value of this type.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
    TicksPerQuarterNote(u16), TicksPerQuarterNoteOutOfRange, "within 1..=65535", |raw| raw >= 1
}

impl TicksPerQuarterNote {
    /// The most ticks, 65535.
    pub const MAX: Self = Self(u16::MAX);
}

tutti_units::ranged! {
    /// The channel count M of Mono Mode On, Control Change 126 (MIDI 1.0
    /// Detailed Specification, Channel Mode Messages, Omni-Off/Mono): `1..=16`
    /// channels from the Basic Channel, or `0`, which assigns every voice the
    /// receiver has one channel each, up to Channel 16. The MIDI 2.0 Protocol
    /// carries it in the top 7 bits of the Control Change's 32-bit value
    /// (M2-104 §7.4.6.1).
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    MonoChannels(u8), MonoChannelsOutOfRange, "within 0..=16", |raw| raw <= 16
}

impl MonoChannels {
    /// `0`: one channel for each voice of the receiver.
    pub const ALL_VOICES: Self = Self(0);

    /// The count in a MIDI 2.0 Control Change 126's value: its top 7 bits,
    /// the other 25 ignored (M2-104 §7.4.6.1). `None` for a count past 16,
    /// which the MIDI 1.0 specification does not define.
    #[must_use]
    pub const fn from_value(value: ControlValue) -> Option<Self> {
        Self::new((value.get() >> 25) as u8)
    }

    /// The value that carries this count: the count in the top 7 bits, the
    /// other 25 zero.
    #[must_use]
    pub const fn to_value(self) -> ControlValue {
        ControlValue::new((self.0 as u32) << 25)
    }

    /// The count in a MIDI 1.0 Control Change 126's data byte, or `None` for a
    /// count past 16.
    #[must_use]
    pub const fn from_data_byte(byte: U7) -> Option<Self> {
        Self::new(byte.get())
    }

    /// The data byte of a MIDI 1.0 Control Change 126 that carries this count.
    #[must_use]
    pub const fn to_data_byte(self) -> U7 {
        U7::masked(self.0)
    }
}

tutti_units::ranged! {
    /// Sharps (positive) or flats (negative) in a key signature, `-7..=7`.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "i8", into = "i8"))]
    SharpsFlats(i8), SharpsFlatsOutOfRange, "within -7..=7", |raw| raw >= -7 && raw <= 7
}

/// An unsigned fixed-point pitch: a MIDI note number with a fraction of a
/// semitone, stored as Q7.25 (7 integer bits, 25 fractional bits). Note On
/// Attribute #3 carries it as Q7.9 (§7.4.15.3) and Registered Per-Note
/// Controller #3 as Q7.25 (§7.4.15.2). Q7.9 widens by zero extension, which is
/// exact (M2-115 §4), so one type holds both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
pub struct Pitch(u32);

// Every `u32` is a Pitch, so the serde form (the bare number) reads back
// through a conversion that cannot fail.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<raw>`: a value enters through `new` or `wire`; this is the serde form"
)]
impl TryFrom<u32> for Pitch {
    type Error = core::convert::Infallible;

    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        Ok(Self(raw))
    }
}

#[cfg(feature = "serde")]
impl From<Pitch> for u32 {
    fn from(pitch: Pitch) -> u32 {
        pitch.0
    }
}

/// The pitch of a whole note number: `note << 25` in Q7.25.
impl From<NoteNumber> for Pitch {
    #[inline]
    fn from(note: NoteNumber) -> Self {
        Self(u32::from(note.get()) << 25)
    }
}

impl Pitch {
    /// The pitch of a Q7.9 word.
    #[inline]
    #[must_use]
    pub const fn from_q7_9(bits: u16) -> Self {
        Self((bits as u32) << 16)
    }

    /// The pitch of a Q7.25 word.
    #[inline]
    #[must_use]
    pub const fn from_q7_25(bits: u32) -> Self {
        Self(bits)
    }

    /// The Q7.25 word.
    #[inline]
    #[must_use]
    pub const fn q7_25(self) -> u32 {
        self.0
    }

    /// The Q7.9 word: the fraction below 9 bits is dropped.
    #[inline]
    #[must_use]
    pub const fn q7_9(self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// The fractional MIDI note number: 69.0 is A4, and 0.5 above is a
    /// quarter tone up.
    #[inline]
    #[must_use]
    pub fn note_number(self) -> f64 {
        f64::from(self.0) / f64::from(1u32 << 25)
    }
}

#[cfg(test)]
mod tests {
    use super::{Pitch, U14, U28, first_non_seven_bit};

    #[test]
    fn a_u14_is_two_seven_bit_bytes_least_significant_first() {
        assert_eq!(U14::of(0x2001).to_le7(), [0x01, 0x40]);
        assert_eq!(U14::of(0x3FFF).to_le7(), [0x7F, 0x7F]);
        assert_eq!(U14::from_le7([0x01, 0x40]), Some(U14::of(0x2001)));
        assert_eq!(U14::from_le7([0x80, 0]), None);
        assert_eq!(U14::from_le7([0, 0x80]), None);
    }

    #[test]
    fn a_u28_is_four_seven_bit_bytes_least_significant_first() {
        assert_eq!(U28::of(0x0FFF_FFFF).to_le7(), [0x7F; 4]);
        assert_eq!(U28::of(1 << 21).to_le7(), [0, 0, 0, 1]);
        assert_eq!(
            U28::from_le7([1, 2, 3, 4]),
            Some(U28::of(1 | 2 << 7 | 3 << 14 | 4 << 21))
        );
        assert_eq!(U28::new(0x1000_0000), None);
        for at in 0..4 {
            let mut bytes = [0u8; 4];
            bytes[at] = 0x80;
            assert_eq!(U28::from_le7(bytes), None);
        }
    }

    #[test]
    fn a_length_is_refused_above_the_width() {
        assert_eq!(U14::from_len(0x3FFF), Some(U14::MAX));
        assert_eq!(U14::from_len(0x4000), None);
        assert_eq!(U14::saturating_from_len(0x3FFF), U14::MAX);
        assert_eq!(U14::saturating_from_len(0x4000), U14::MAX);
        assert_eq!(U14::saturating_from_len(5).get(), 5);
        assert_eq!(U28::from_len(0x0FFF_FFFF), Some(U28::MAX));
        assert_eq!(U28::from_len(0x1000_0000), None);
    }

    #[test]
    fn the_first_byte_with_its_top_bit_set_is_found() {
        assert_eq!(first_non_seven_bit(&[0, 0x7F, 0x80, 0xFF]), Some(2));
        assert_eq!(first_non_seven_bit(&[0, 0x7F]), None);
        assert_eq!(first_non_seven_bit(&[]), None);
    }

    use crate::Attribute;

    #[test]
    fn q7_9_is_widened_by_zero_extension() {
        for bits in [0u16, 1, 0x0200, 0x8C30, 0xFE00, 0xFFFF] {
            let p = Pitch::from_q7_9(bits);
            assert_eq!(p.q7_25(), u32::from(bits) << 16);
            assert_eq!(p.q7_9(), bits);
        }
    }

    #[test]
    fn the_note_number_is_the_integer_part_plus_the_fraction() {
        assert_eq!(Pitch::from_q7_9(60 << 9).note_number(), 60.0);
        assert_eq!(Pitch::from_q7_9((60 << 9) | 256).note_number(), 60.5);
        assert_eq!(Pitch::from_q7_25(69 << 25).note_number(), 69.0);
        assert_eq!(Pitch::from_q7_25(1 << 24).note_number(), 0.5);
        assert_eq!(Pitch::from_q7_25(127 << 25).note_number(), 127.0);
        assert_eq!(
            Pitch::from(crate::NoteNumber::of(127)),
            Pitch::from_q7_25(127 << 25)
        );
    }

    #[test]
    fn only_the_pitch_attribute_has_a_pitch() {
        assert_eq!(
            Attribute::Pitch(61 << 9).pitch(),
            Some(Pitch::from_q7_25(61 << 25))
        );
        assert_eq!(Attribute::None.pitch(), None);
        assert_eq!(Attribute::Manufacturer(61 << 9).pitch(), None);
        assert_eq!(
            Attribute::Reserved(crate::ReservedAttribute::wire(4, 1)).pitch(),
            None
        );
    }
}
