// A fractional length in frames.

use super::*;

// `SampleSpan` is a `Unit` (below), which is already a `Quantity`, so it takes
// the type and its error without the macro's own `Quantity` impl.
crate::ranged! {
    @items []
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
    /// and serde. [`of`](Self::of) is for a literal. [`clamped`](Self::clamped)
    /// is the one saturating constructor; [`Unit::from_raw`] is it too, since a
    /// parameter cell holds any float.
    ///
    /// ```
    /// use tutti_units::{SamplePosition, SampleRate, SampleSpan, Seconds};
    ///
    /// let rate = SampleRate::of(48_000.0);
    /// let delay = SampleSpan::from_seconds(Seconds::new(0.25), rate).unwrap();
    /// assert_eq!(delay.get(), 12_000.0);
    /// assert_eq!(SamplePosition::new(20_000.0) - delay, SamplePosition::new(8_000.0));
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
        Seconds::new(self.get() / rate.into().get())
    }

    /// Whole frames within the span, rounded **down**: the integer index an
    /// interpolating read starts from. Saturates past `usize::MAX`.
    #[inline]
    #[must_use]
    pub fn to_samples_floor(self) -> Samples {
        Samples::new(libm::floor(self.0) as usize)
    }

    /// Whole frames that hold the span, rounded **up**: the size of a buffer
    /// that holds a delay of this length. Saturates past `usize::MAX`.
    #[inline]
    #[must_use]
    pub fn to_samples_ceil(self) -> Samples {
        Samples::new(libm::ceil(self.0) as usize)
    }

    /// The interpolation fraction between [`to_samples_floor`](Self::to_samples_floor)
    /// and the next frame, in `[0, 1)`.
    #[inline]
    #[must_use]
    pub fn fract(self) -> f64 {
        self.0 - libm::floor(self.0)
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
        SamplePosition::new(self.get() + rhs.get())
    }
}

impl core::ops::Sub<SampleSpan> for SamplePosition {
    type Output = SamplePosition;
    #[inline]
    fn sub(self, rhs: SampleSpan) -> SamplePosition {
        SamplePosition::new(self.get() - rhs.get())
    }
}

impl core::ops::AddAssign<SampleSpan> for SamplePosition {
    #[inline]
    fn add_assign(&mut self, rhs: SampleSpan) {
        *self = SamplePosition::new(self.get() + rhs.get());
    }
}

impl core::ops::SubAssign<SampleSpan> for SamplePosition {
    #[inline]
    fn sub_assign(&mut self, rhs: SampleSpan) {
        *self = SamplePosition::new(self.get() - rhs.get());
    }
}

impl SamplePosition {
    /// The span from `earlier` to this position, or `None` when `earlier` is
    /// the later of the two (or either is NaN): the delay between a read and a
    /// write position.
    #[inline]
    #[must_use]
    pub fn span_since(self, earlier: SamplePosition) -> Option<SampleSpan> {
        SampleSpan::new(self.get() - earlier.get())
    }
}

/// A read position in frames: a `u64` Q32.32 count, whole frames in the high 32 bits and the
/// fraction in the low 32.
///
/// A delay's or a resampler's read head is a position, never a float: a float holds a frame
/// count to its own precision, so an `f32` at 10 s and 192 kHz (1.92 million frames) has
/// steps of 1/8 frame and none at all past 2^23, and a modulated read snaps. This one holds
/// every count to `2^-32` frame up to its range of `2^32` frames (6.2 hours at 192 kHz, far
/// past a delay's longest line). Past the range a position saturates at [`MAX`](Self::MAX),
/// and so does every operation on it, so a position never wraps.
///
/// A [`SampleSpan`] or a [`Seconds`] at a rate is a setting; [`of`](Self::of) and
/// [`from_frames`](Self::from_frames) turn one into a position once, and a processor reads
/// a position as a whole frame and a float fraction through one crossing in its own crate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadPosition(u64);

impl ReadPosition {
    /// The bits of the fraction.
    pub const FRACTION_BITS: u32 = 32;
    /// The counts in one frame, `2^FRACTION_BITS`: the one scale between a position's count
    /// and a frame count held as a float.
    pub const COUNTS_PER_FRAME: f64 = (1_u64 << Self::FRACTION_BITS) as f64;
    /// No frames back.
    pub const START: ReadPosition = ReadPosition(0);
    /// The last position, just under `2^32` frames: where every operation saturates.
    pub const MAX: ReadPosition = ReadPosition(u64::MAX);

    /// The position `count` times `2^-32` frames on.
    #[inline]
    #[must_use]
    pub const fn from_count(count: u64) -> ReadPosition {
        ReadPosition(count)
    }

    /// The position as a count of `2^-32` frames.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// A whole number of frames, saturating at [`MAX`](Self::MAX).
    #[inline]
    #[must_use]
    pub const fn from_frames(frames: Samples) -> ReadPosition {
        let n = frames.get() as u64;
        if n > u32::MAX as u64 {
            ReadPosition::MAX
        } else {
            ReadPosition(n << Self::FRACTION_BITS)
        }
    }

    /// The frames `time` occupies at `rate`, rounded to the nearest `2^-32` frame (the
    /// product is one `f64` multiply, within `2^-53` of itself). NaN and a negative time are
    /// [`START`](Self::START); a time past the range is [`MAX`](Self::MAX).
    #[inline]
    #[must_use]
    pub fn of(time: Seconds, rate: SampleRate) -> ReadPosition {
        // Scaling by 2^32 is exact; `as` saturates at both ends and maps NaN to 0.
        ReadPosition((time.get() * rate.get() * Self::COUNTS_PER_FRAME + 0.5) as u64)
    }

    /// The position of `span` frames, rounded to the nearest `2^-32` frame and saturating at
    /// [`MAX`](Self::MAX): [`of`](Self::of) for a span already in frames.
    #[inline]
    #[must_use]
    pub fn from_span(span: SampleSpan) -> ReadPosition {
        ReadPosition((span.get() * Self::COUNTS_PER_FRAME + 0.5) as u64)
    }

    /// The position as a span of frames, exact to `f64` (a position has 64 bits and the
    /// span 53, so it rounds a position past `2^21` frames).
    #[inline]
    #[must_use]
    pub fn span(self) -> SampleSpan {
        SampleSpan::clamped(self.0 as f64 / Self::COUNTS_PER_FRAME)
    }

    /// The whole frames: the position truncated.
    #[inline]
    #[must_use]
    pub const fn frames(self) -> Samples {
        Samples::new((self.0 >> Self::FRACTION_BITS) as usize)
    }

    /// The position `frames` frames nearer the start, [`START`](Self::START) at most.
    #[inline]
    #[must_use]
    pub const fn saturating_sub(self, frames: Samples) -> ReadPosition {
        ReadPosition(self.0.saturating_sub(ReadPosition::from_frames(frames).0))
    }

    /// This position moved on by `n` steps of `step` (a per-frame advance,
    /// [`ReadRate::step`]), in exact integer counts: `n` steps land where `n`
    /// single advances would, with no drift. Saturates at [`MAX`](Self::MAX), as
    /// every operation on a position does.
    #[inline]
    #[must_use]
    pub const fn advanced(self, step: ReadPosition, n: Samples) -> ReadPosition {
        ReadPosition(self.0.saturating_add(step.0.saturating_mul(n.get() as u64)))
    }
}

#[cfg(test)]
mod tests;
