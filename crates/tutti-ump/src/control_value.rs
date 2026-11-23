//! The 32-bit data word of a MIDI 2.0 controller, pressure or per-note message.

use tutti_units::Normalized;

use crate::{U7, U14, scale};

/// The 32-bit value of a MIDI 2.0 Control Change, Channel or Polyphonic
/// Pressure, Registered or Assignable Controller, or Per-Note Controller
/// (M2-104 section 7.4).
///
/// Every `u32` is a value, so there is no refusing constructor, as for
/// [`NoteVelocity`](crate::NoteVelocity). The word is the wire's integer, not
/// a MIDI 1.0 value: `127` as a raw word is nearly zero.
///
/// # Examples
///
/// ```
/// use tutti_ump::{CCNumber, ControlValue, Message, MidiChannel, Voice2};
///
/// let half = ControlValue::new(0x8000_0000);
/// let cc = Message::Midi2(
///     MidiChannel::FIRST.into(),
///     Voice2::ControlChange { controller: CCNumber::VOLUME, value: half },
/// );
/// assert!(matches!(cc, Message::Midi2(..)));
/// assert_eq!(ControlValue::MAX.get(), u32::MAX);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
pub struct ControlValue(u32);

// Every `u32` is a ControlValue, so the serde form (the bare number) reads back
// through a conversion that cannot fail.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<raw>`: a value enters through `new` or `wire`; this is the serde form"
)]
impl TryFrom<u32> for ControlValue {
    type Error = core::convert::Infallible;

    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        Ok(Self(raw))
    }
}

impl ControlValue {
    /// Value 0.
    pub const MIN: Self = Self(0);

    /// The centre of the range, `0x8000_0000`: where a 7-bit 64 upscales to.
    pub const CENTER: Self = Self(0x8000_0000);

    /// Value `0xFFFF_FFFF`.
    pub const MAX: Self = Self(u32::MAX);

    /// The value whose word is `raw`.
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

    /// The value as a position on `0..=1`: `get() / 0xFFFF_FFFF`.
    #[inline]
    #[must_use]
    pub fn normalized(self) -> Normalized {
        Normalized::clamped(f64::from(self.0) / f64::from(u32::MAX))
    }

    /// The value at position `n` of the 32-bit range: `round(n * 0xFFFF_FFFF)`.
    /// Inverse of [`normalized`](Self::normalized).
    #[inline]
    #[must_use]
    pub fn from_normalized(n: Normalized) -> Self {
        // `n` is on `0..=1`, so the rounded product fits the 32-bit wire word.
        Self(libm::round(n.get() * f64::from(u32::MAX)) as u32)
    }
}

impl ControlValue {
    /// The data of Registered Controller `index` from its 14-bit MIDI 1.0 form
    /// (M2-115 section 4.1): zero-extended for an index of 0 to 31 in every
    /// bank, scaled Min-Center-Max otherwise.
    #[inline]
    #[must_use]
    pub fn from_registered(index: U7, data: U14) -> Self {
        if scale::zero_extended(index) {
            Self(u32::from(data.get()) << 18)
        } else {
            Self::from(data)
        }
    }

    /// The value as 7 bits: the top 7 bits, the downscale of M2-115 section 3.4
    /// as a plain right shift.
    #[inline]
    #[must_use]
    pub fn to_u7(self) -> U7 {
        // `>> 25` leaves 7 bits: the mask removes nothing.
        U7::masked((self.0 >> 25) as u8)
    }
}

/// A 7-bit controller or pressure value as the 32-bit word of a MIDI 2.0
/// Control Change or pressure message, scaled Min-Center-Max (M2-115 section
/// 3): 0 stays 0, 64 is [`ControlValue::CENTER`] and 127 is
/// [`ControlValue::MAX`]. [`to_u7`](ControlValue::to_u7) is its right shift
/// back.
///
/// # Examples
///
/// ```
/// use tutti_ump::{ControlValue, U7};
///
/// assert_eq!(ControlValue::from(U7::of(127)), ControlValue::MAX);
/// ```
impl From<U7> for ControlValue {
    #[inline]
    fn from(value: U7) -> Self {
        Self(scale::upscale(u32::from(value.get()), 7, 32))
    }
}

/// The 14-bit data of a MIDI 1.0 Registered or Assignable Controller as the
/// 32-bit word, scaled Min-Center-Max (M2-115 section 3.1).
impl From<U14> for ControlValue {
    #[inline]
    fn from(value: U14) -> Self {
        Self(scale::upscale(u32::from(value.get()), 14, 32))
    }
}

impl From<ControlValue> for u32 {
    #[inline]
    fn from(value: ControlValue) -> Self {
        value.0
    }
}
