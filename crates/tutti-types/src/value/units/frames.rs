// A fractional length in frames.

use super::*;

crate::ranged! {
    /// A non-integer length in frames: a delay time, a delay line's length, an
    /// all-pass's delay.
    ///
    /// Distinct from [`Samples`], an integer count of frames, and from
    /// [`SamplePosition`], an absolute offset: a span is how far apart two
    /// positions are, as [`BeatDuration`] is to [`Beat`]. It stays in frames
    /// because the read interpolates between two of them, and `f64`-backed like
    /// [`SamplePosition`] and [`Seconds`], so a span survives being added to a
    /// position far into a long buffer.
    ///
    /// **Range-checked**: [`new`](Self::new) refuses NaN, the infinities and
    /// anything below zero (negative zero included), and so do `TryFrom<f64>`
    /// and serde. [`of`](Self::of) is for a literal. [`clamped`](Self::clamped) is the one saturating
    /// constructor; [`Unit::from_raw`] is it too, since a parameter cell holds
    /// any float.
    ///
    /// ```
    /// use tutti_types::{SamplePosition, SampleRate, SampleSpan, Seconds};
    ///
    /// let rate = SampleRate::of(48_000.0);
    /// let delay = SampleSpan::from_seconds(Seconds(0.25), rate).unwrap();
    /// assert_eq!(delay.get(), 12_000.0);
    /// assert_eq!(SamplePosition(20_000.0) - delay, SamplePosition(8_000.0));
    /// assert_eq!(SampleSpan::new(-1.0), None);
    /// ```
    #[repr(transparent)]
    #[derive(PartialOrd, Default)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    SampleSpan(f64), SampleSpanOutOfRange, "a span of frames, finite and not below zero",
    |raw| raw.is_sign_positive() && raw <= f64::MAX
}

// NOT `Sub`, `Neg`, `Mul<f64>`, `Div<f64>`: each can leave the range. The
// replacements are `checked_sub`, `saturating_sub` and `scaled`.
impl SampleSpan {
    /// No frames.
    pub const ZERO: SampleSpan = SampleSpan(0.0);

    /// `raw` constrained into `0..=f64::MAX`; NaN and negative zero are [`ZERO`](Self::ZERO).
    #[inline]
    #[must_use]
    pub const fn clamped(raw: f64) -> SampleSpan {
        if raw.is_nan() || !raw.is_sign_positive() {
            SampleSpan::ZERO
        } else if raw > f64::MAX {
            SampleSpan(f64::MAX)
        } else {
            SampleSpan(raw)
        }
    }

    /// The span of a whole number of frames, exact up to 2^53 frames.
    #[inline]
    #[must_use]
    pub const fn from_samples(frames: Samples) -> SampleSpan {
        SampleSpan(frames.get() as f64)
    }

    /// The frames `time` occupies at `rate`, kept fractional; the inverse of
    /// [`to_seconds`](Self::to_seconds).
    ///
    /// Negative zero is [`ZERO`](Self::ZERO). `None` for a negative or NaN
    /// time, and for a product past `f64::MAX`.
    #[inline]
    #[must_use]
    pub fn from_seconds(time: Seconds, rate: impl Into<SampleRate>) -> Option<SampleSpan> {
        SampleSpan::new(time.get() * rate.into().get() + 0.0)
    }

    /// The time this span takes at `rate`.
    #[inline]
    #[must_use]
    pub fn to_seconds(self, rate: impl Into<SampleRate>) -> Seconds {
        Seconds(self.0 / rate.into().get())
    }

    /// Whole frames within the span, rounded **down**: the integer index an
    /// interpolating read starts from. Saturates past `usize::MAX`.
    #[inline]
    #[must_use]
    pub fn to_samples_floor(self) -> Samples {
        Samples::new(self.0.floor() as usize)
    }

    /// Whole frames that hold the span, rounded **up**: the size of a buffer
    /// that holds a delay of this length. Saturates past `usize::MAX`.
    #[inline]
    #[must_use]
    pub fn to_samples_ceil(self) -> Samples {
        Samples::new(self.0.ceil() as usize)
    }

    /// The interpolation fraction between [`to_samples_floor`](Self::to_samples_floor)
    /// and the next frame, in `[0, 1)`.
    #[inline]
    #[must_use]
    pub fn fract(self) -> f64 {
        self.0 - self.0.floor()
    }

    /// `self - rhs`, or `None` when `rhs` is longer.
    #[inline]
    #[must_use]
    pub fn checked_sub(self, rhs: SampleSpan) -> Option<SampleSpan> {
        SampleSpan::new(self.0 - rhs.0)
    }

    /// `self - rhs`, [`ZERO`](Self::ZERO) when `rhs` is longer.
    #[inline]
    #[must_use]
    pub fn saturating_sub(self, rhs: SampleSpan) -> SampleSpan {
        SampleSpan::clamped(self.0 - rhs.0)
    }

    /// This span times `factor`, or `None` when `factor` is negative or NaN, or
    /// the product leaves the range. A modulated delay time is its centre
    /// scaled by a non-negative factor.
    #[inline]
    #[must_use]
    pub fn scaled(self, factor: f64) -> Option<SampleSpan> {
        // `0 * negative` is negative zero: a span of nothing, not a refusal.
        SampleSpan::new(self.0 * factor + 0.0)
    }

    /// Returns the shorter of the two.
    #[inline]
    #[must_use]
    pub fn min(self, other: SampleSpan) -> SampleSpan {
        SampleSpan(self.0.min(other.0))
    }

    /// Returns the longer of the two.
    #[inline]
    #[must_use]
    pub fn max(self, other: SampleSpan) -> SampleSpan {
        SampleSpan(self.0.max(other.0))
    }

    /// Clamps into `lo..=hi`.
    ///
    /// # Panics
    ///
    /// Panics if `lo > hi`, matching `f64::clamp`.
    #[inline]
    #[must_use]
    pub fn clamp(self, lo: SampleSpan, hi: SampleSpan) -> SampleSpan {
        SampleSpan(self.0.clamp(lo.0, hi.0))
    }
}

/// Spans sum, saturating at `f64::MAX` so the sum stays in range.
impl core::ops::Add for SampleSpan {
    type Output = SampleSpan;
    #[inline]
    fn add(self, rhs: SampleSpan) -> SampleSpan {
        SampleSpan::clamped(self.0 + rhs.0)
    }
}

impl core::ops::AddAssign for SampleSpan {
    #[inline]
    fn add_assign(&mut self, rhs: SampleSpan) {
        *self = *self + rhs;
    }
}

/// How many times one span holds another, a bare number. A zero divisor gives
/// infinity, or NaN for `0 / 0`.
impl core::ops::Div for SampleSpan {
    type Output = f64;
    #[inline]
    fn div(self, rhs: SampleSpan) -> f64 {
        self.0 / rhs.0
    }
}

impl core::fmt::Display for SampleSpan {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// `from_raw` saturates ([`SampleSpan::clamped`]): a parameter cell holds any
/// float, and a unit's `from_raw` cannot refuse.
impl Unit for SampleSpan {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self::clamped(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.0
    }
}

// The affine pairing with `SamplePosition`: a position moved by a span is a
// position, and the span between two positions is `span_since`. There is no
// `pos - pos -> SampleSpan` operator: a later position minus an earlier one is
// a span, the reverse is not, and an operator cannot refuse.
impl core::ops::Add<SampleSpan> for SamplePosition {
    type Output = SamplePosition;
    #[inline]
    fn add(self, rhs: SampleSpan) -> SamplePosition {
        SamplePosition(self.0 + rhs.0)
    }
}

impl core::ops::Sub<SampleSpan> for SamplePosition {
    type Output = SamplePosition;
    #[inline]
    fn sub(self, rhs: SampleSpan) -> SamplePosition {
        SamplePosition(self.0 - rhs.0)
    }
}

impl core::ops::AddAssign<SampleSpan> for SamplePosition {
    #[inline]
    fn add_assign(&mut self, rhs: SampleSpan) {
        self.0 += rhs.0;
    }
}

impl core::ops::SubAssign<SampleSpan> for SamplePosition {
    #[inline]
    fn sub_assign(&mut self, rhs: SampleSpan) {
        self.0 -= rhs.0;
    }
}

impl SamplePosition {
    /// The span from `earlier` to this position, or `None` when `earlier` is
    /// the later of the two (or either is NaN): the delay between a read and a
    /// write position.
    #[inline]
    #[must_use]
    pub fn span_since(self, earlier: SamplePosition) -> Option<SampleSpan> {
        SampleSpan::new(self.0 - earlier.0)
    }
}

#[cfg(test)]
mod tests;
