//! The 32-bit pitch bend word of a MIDI 2.0 Pitch Bend or Per-Note Pitch Bend.

/// The 32-bit value of a MIDI 2.0 Pitch Bend or Per-Note Pitch Bend (M2-104
/// sections 7.4.11 and 7.4.7), centred on [`CENTER`](Self::CENTER).
///
/// Every `u32` is a bend, so there is no refusing constructor. The word is the
/// wire's integer, not a MIDI 1.0 14-bit bend: the raw `0x2000` of a 14-bit
/// centre is far below the centre here.
///
/// # Examples
///
/// ```
/// use tutti_ump::{BendValue, Message, MidiChannel, Voice2};
///
/// let bend = Message::Midi2(MidiChannel::FIRST.into(), Voice2::PitchBend(BendValue::CENTER));
/// assert!(matches!(bend, Message::Midi2(..)));
/// assert_eq!(BendValue::CENTER.get(), 0x8000_0000);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
pub struct BendValue(u32);

// Every `u32` is a BendValue, so the serde form (the bare number) reads back
// through a conversion that cannot fail.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<raw>`: a value enters through `new` or `wire`; this is the serde form"
)]
impl TryFrom<u32> for BendValue {
    type Error = core::convert::Infallible;

    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        Ok(Self(raw))
    }
}

impl BendValue {
    /// The lowest bend, `0`.
    pub const MIN: Self = Self(0);

    /// No bend, `0x8000_0000`.
    pub const CENTER: Self = Self(0x8000_0000);

    /// The highest bend, `0xFFFF_FFFF`.
    pub const MAX: Self = Self(u32::MAX);

    /// The bend whose word is `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The word's 32 bits.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<BendValue> for u32 {
    #[inline]
    fn from(bend: BendValue) -> Self {
        bend.0
    }
}
