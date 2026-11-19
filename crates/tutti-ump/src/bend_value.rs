//! The 32-bit pitch bend word of a MIDI 2.0 Pitch Bend or Per-Note Pitch Bend.

use tutti_units::Bipolar;

use crate::{U14, scale};

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

    /// The bend as a position on `-1..=1`, centre-exact and piecewise.
    ///
    /// M2-104 section 7.4.13 gives "an equal range of pitch bend up" as down,
    /// with `0x8000_0000..=0xFFFF_FFFF` the whole up range. With
    /// `k = get() - 2^31`: below the centre the position is `k / 2^31`, from
    /// the centre up it is `k / (2^31 - 1)`. So `0` is `-1.0`,
    /// [`CENTER`](Self::CENTER) is `0.0` and `u32::MAX` is `+1.0`, each exact.
    #[inline]
    #[must_use]
    pub fn bipolar(self) -> Bipolar {
        let k = f64::from(self.0) - f64::from(Self::CENTER.0);
        let span = if k < 0.0 {
            f64::from(Self::CENTER.0)
        } else {
            f64::from(Self::MAX.0 - Self::CENTER.0)
        };
        // `k` is on `-2^31..=2^31 - 1` and `span` is its side's extent, so the
        // quotient is on `-1..=1`.
        Bipolar::clamped(k / span)
    }

    /// The bend at position `b`: `2^31 + round(b * 2^31)` below the centre and
    /// `2^31 + round(b * (2^31 - 1))` from it up (M2-104 section 7.4.13, "an
    /// equal range of pitch bend up"), rounding half away from zero. Inverse of
    /// [`bipolar`](Self::bipolar); `+1.0` is `u32::MAX`.
    #[inline]
    #[must_use]
    pub fn from_bipolar(b: Bipolar) -> Self {
        let b = b.get();
        let span = if b < 0.0 {
            f64::from(Self::CENTER.0)
        } else {
            f64::from(Self::MAX.0 - Self::CENTER.0)
        };
        // The word is on `0..=u32::MAX`; the cast is the wire's 32-bit width.
        Self((f64::from(Self::CENTER.0) + libm::round(b * span)) as u32)
    }
}

impl BendValue {
    /// The bend as 14 bits: the top 14 bits, the downscale of M2-115 section
    /// 3.4 as a plain right shift.
    #[inline]
    #[must_use]
    pub fn to_u14(self) -> U14 {
        // `>> 18` leaves 14 bits: the mask removes nothing.
        U14::masked((self.0 >> 18) as u16)
    }
}

/// A 14-bit pitch bend as the 32-bit word of a MIDI 2.0 Pitch Bend, scaled
/// Min-Center-Max (M2-115 section 3): the 14-bit centre `0x2000` is
/// [`BendValue::CENTER`]. [`to_u14`](BendValue::to_u14) is its right shift
/// back.
///
/// # Examples
///
/// ```
/// use tutti_ump::{BendValue, U14};
///
/// assert_eq!(BendValue::from(U14::of(0x2000)), BendValue::CENTER);
/// ```
impl From<U14> for BendValue {
    #[inline]
    fn from(bend: U14) -> Self {
        Self(scale::upscale(u32::from(bend.get()), 14, 32))
    }
}

impl From<BendValue> for u32 {
    #[inline]
    fn from(bend: BendValue) -> Self {
        bend.0
    }
}
