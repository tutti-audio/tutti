//! The velocity field of a MIDI 2.0 Note On or Note Off.

use tutti_units::Normalized;

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

    /// Velocity `0x8000`, the midpoint of the range: M2-115 section 3 widens a
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

    /// The velocity as a position on `0..=1`: `get() / 0xFFFF`.
    #[inline]
    #[must_use]
    pub fn normalized(self) -> Normalized {
        Normalized::clamped(f64::from(self.0) / f64::from(u16::MAX))
    }

    /// The velocity at position `n` of the 16-bit range: `round(n * 0xFFFF)`.
    /// Inverse of [`normalized`](Self::normalized).
    #[inline]
    #[must_use]
    pub fn from_normalized(n: Normalized) -> Self {
        // `n` is on `0..=1`, so the rounded product fits the 16-bit wire field.
        Self(libm::round(n.get() * f64::from(u16::MAX)) as u16)
    }
}

impl From<NoteVelocity> for u16 {
    #[inline]
    fn from(velocity: NoteVelocity) -> Self {
        velocity.0
    }
}
