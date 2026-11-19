//! The velocity field of a MIDI 2.0 Note On or Note Off.

use tutti_units::Velocity;

use crate::{U7, U14, scale};

/// The 16-bit velocity of a MIDI 2.0 Note On or Note Off (M2-104 section 7.4.2).
///
/// This is the wire's 16-bit field, `0` to `0xFFFF`, and not a MIDI 1.0
/// velocity: `NoteVelocity::new(100)` is a velocity of 100 out of 65535, which
/// is nearly silent, and a MIDI 1.0 velocity of 100 is `51492` here. Every
/// `u16` is a velocity, so there is no refusing constructor. Zero is a Note On
/// here, not a Note Off.
///
/// # Examples
///
/// ```
/// use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};
///
/// // Half of the 16-bit range, which is a MIDI 1.0 velocity of 64.
/// let on = Message::Midi2(
///     MidiChannel::FIRST.into(),
///     Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
/// );
/// assert!(matches!(on, Message::Midi2(..)));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct NoteVelocity(u16);

impl NoteVelocity {
    /// Velocity 0.
    pub const MIN: Self = Self(0);

    /// Velocity `0x8000`, the midpoint of the range: M2-115 section 3 upscales a
    /// MIDI 1.0 velocity of 64 to it.
    pub const CENTER: Self = Self(0x8000);

    /// Velocity `0xFFFF`.
    pub const MAX: Self = Self(u16::MAX);

    /// The velocity whose field is `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The field's 16 bits.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The velocity as a [`Velocity`] on `0..=1`: `get() / 0xFFFF`.
    #[inline]
    #[must_use]
    pub fn velocity(self) -> Velocity {
        Velocity::new_clamped(f64::from(self.0) / f64::from(u16::MAX))
    }

    /// The field at `velocity` of the 16-bit range: `round(velocity * 0xFFFF)`.
    /// Inverse of [`velocity`](Self::velocity).
    #[inline]
    #[must_use]
    pub fn from_velocity(velocity: Velocity) -> Self {
        // The float-to-integer cast saturates, so a velocity outside `0..=1`
        // lands on `MIN` or `MAX`.
        Self(libm::round(velocity.get() * f64::from(u16::MAX)) as u16)
    }
}

impl NoteVelocity {
    /// The velocity as 7 bits: the top 7 bits, the downscale of M2-115 section
    /// 3.4 as a plain right shift.
    ///
    /// A nonzero velocity below `0x0200` is 0, which MIDI 1.0 reads as a Note
    /// Off. A Note On goes through [`to_note_on_u7`](Self::to_note_on_u7)
    /// instead.
    #[inline]
    #[must_use]
    pub fn to_u7(self) -> U7 {
        // `>> 9` leaves 7 bits: the mask removes nothing.
        U7::masked((self.0 >> 9) as u8)
    }

    /// The velocity as 7 bits, never 0: a MIDI 1.0 Note On with velocity 0 is a
    /// Note Off, so a nonzero velocity below `0x0200` is 1, the lowest audible
    /// Note On velocity.
    #[inline]
    #[must_use]
    pub fn to_note_on_u7(self) -> U7 {
        self.to_u7().max(U7::of(1))
    }
}

/// A 7-bit velocity as the 16-bit field of a Note On or Note Off, scaled
/// Min-Center-Max (M2-115 section 3.4): 0 stays 0, 64 is `0x8000` and 127 is
/// `0xFFFF`. [`to_u7`](NoteVelocity::to_u7) is its right shift back.
///
/// This is the named way from a MIDI 1.0 velocity to a MIDI 2.0 one:
/// `NoteVelocity::new(100)` is a velocity of 100 out of 65535, not a MIDI 1.0
/// 100.
///
/// # Examples
///
/// ```
/// use tutti_ump::{NoteVelocity, U7};
///
/// assert_eq!(NoteVelocity::from(U7::of(100)).get(), 51492);
/// ```
impl From<U7> for NoteVelocity {
    #[inline]
    fn from(velocity: U7) -> Self {
        Self(scale::upscale(u32::from(velocity.get()), 7, 16) as u16)
    }
}

/// A 14-bit velocity (a MIDI 1.0 High Resolution Velocity Prefix and velocity)
/// as the 16-bit field, scaled Min-Center-Max.
impl From<U14> for NoteVelocity {
    #[inline]
    fn from(velocity: U14) -> Self {
        Self(scale::upscale(u32::from(velocity.get()), 14, 16) as u16)
    }
}

impl From<NoteVelocity> for u16 {
    #[inline]
    fn from(velocity: NoteVelocity) -> Self {
        velocity.0
    }
}
