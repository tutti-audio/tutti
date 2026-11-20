//! The fractional-read delay ring and its interpolation: a delay whose time is
//! modulated must interpolate between taps or it steps audibly.

mod bank;
mod layout;
mod max_delay;
mod ring;
mod routing;
mod taps;
mod times;

pub use bank::{DelayBank, DelayRamps, DelaySignals};
pub use layout::{
    DELAY_SLEW, DelayControls, DelayLayout, DelayLayoutBuilder, DelayLayoutError,
    MAX_DELAY_CHANNELS,
};
pub use max_delay::{MaxDelay, MaxDelayError};
pub(crate) use ring::DelayRing;
pub use routing::{FeedbackRouting, FeedbackRoutingBuilder, FeedbackRoutingError};
pub(crate) use taps::{ring_seconds, trips};
pub use times::{ChannelTimes, ChannelTimesError};

use tutti_buffers::Float;
use tutti_units::{ReadPosition, Samples};

/// How a fractional delay position is turned into a sample.
///
/// The cost/quality ladder for a *modulated* delay: a moving delay time lands
/// between samples, and how that gap is filled is what separates a clean chorus
/// from a gritty one. At a fixed, integral delay all three agree.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum InterpolationMode {
    /// Round to the nearest whole sample. Cheapest, and it quantises the delay
    /// to frame boundaries — a swept delay steps audibly ("zipper"). Fine for a
    /// static echo, wrong for chorus or flanger.
    None,
    /// Linear blend between the two neighbouring samples. The default: no
    /// stepping under modulation, at the cost of slight high-frequency damping
    /// that worsens as the fraction approaches 0.5.
    #[default]
    Linear,
    /// Four-point cubic Hermite. Keeps the highs that [`Linear`](Self::Linear)
    /// damps, for roughly four reads per sample instead of two. Reach for it on
    /// audibly-swept delays; it is wasted on a static one.
    CubicHermite,
}

/// A fractional-delay ring buffer over frames of the float `F` (`f32` or `f64`): push a
/// sample per frame, read back at any real-valued delay.
///
/// One ring, two readers. [`read_sample`] reads a fractional delay, interpolated
/// ([`InterpolationMode`]): the core of a modulated delay. [`read_whole`] reads a whole
/// number of frames back with no interpolation: a fixed delay, such as the limiter's
/// lookahead. Capacity is fixed at construction: both clamp rather than grow, so the line
/// never allocates on the audio thread.
///
/// [`read_sample`]: Self::read_sample
/// [`read_whole`]: Self::read_whole
#[derive(Clone)]
pub struct DelayLine<F> {
    ring: DelayRing<F>,
    max_delay_samples: usize,
}

impl<F> std::fmt::Debug for DelayLine<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelayLine")
            .field("max_delay_samples", &self.max_delay_samples)
            .finish_non_exhaustive()
    }
}

impl<F: Float> DelayLine<F> {
    /// Allocates a line holding up to `capacity` frames of history.
    ///
    /// The backing buffer is one frame longer than the maximum delay, so that
    /// an interpolating read at exactly `capacity` still has a second
    /// tap to blend toward. Allocates — build before going live.
    #[must_use]
    pub fn new(capacity: Samples) -> Self {
        let max_delay_samples = capacity.get();
        Self {
            ring: DelayRing::new(max_delay_samples + 1),
            max_delay_samples,
        }
    }

    /// Writes one frame at the cursor and advances, overwriting the oldest
    /// sample.
    ///
    /// Call exactly once per frame: the cursor *is* the line's clock, so a
    /// skipped or doubled push shifts every subsequent read by that much.
    #[inline]
    pub fn push_sample(&mut self, sample: F) {
        self.ring.push(sample);
    }

    /// Reads the line `delay` frames back from the newest write, interpolating per `mode`.
    ///
    /// `delay` is a [`ReadPosition`]: a fractional frame count held to `2^-32` frame at any
    /// distance, built once per block from a time ([`ReadPosition::of`]) and moved per sample
    /// by [`Float::offset_position`]. `0` is the sample just pushed. The
    /// interpolator takes the position's whole frames as the index and its fraction as an
    /// `F` weight ([`Float::split_position`]).
    ///
    /// The delay is clamped to `0..=max_delay_samples`, so an over-long
    /// request shortens silently rather than panicking. Does not allocate.
    #[inline]
    pub fn read_sample(&self, delay: ReadPosition, mode: InterpolationMode) -> F {
        // Defensive only: a caller holds its delay inside the line, so this clamp is
        // reached by a caller that does not.
        let delay = delay.min(ReadPosition::from_frames(Samples::new(
            self.max_delay_samples,
        )));
        match mode {
            InterpolationMode::None => {
                // The nearest whole slot: half a frame on, truncated.
                let half = ReadPosition::from_count(delay.get().saturating_add(1 << 31));
                self.read_whole(half.frames().get())
            }
            InterpolationMode::Linear => {
                let (floor, frac) = F::split_position(delay);
                let floor = floor.get();
                let s0 = self.read_whole(floor);
                let s1 = self.read_whole(floor + 1);
                crate::lerp(s0, s1, frac)
            }
            InterpolationMode::CubicHermite => {
                let (floor, frac) = F::split_position(delay);
                let floor = floor.get();
                let sm1 = self.read_whole(floor.saturating_sub(1));
                let s0 = self.read_whole(floor);
                let s1 = self.read_whole(floor + 1);
                let s2 = self.read_whole(floor + 2);
                crate::cubic_hermite(sm1, s0, s1, s2, frac)
            }
        }
    }

    /// Zeroes the line and returns the cursor to 0, dropping any tail still in
    /// flight.
    ///
    /// Does not reallocate, so it is safe on the audio thread.
    pub fn reset(&mut self) {
        self.ring.clear();
    }

    /// Reads the line `frames` frames back from the newest write, a whole number and no
    /// interpolation: `0` is the sample just pushed, and a `frames` past the line's
    /// capacity reads the oldest it holds. Does not allocate.
    #[inline]
    #[must_use]
    pub fn read_whole(&self, frames: usize) -> F {
        // The newest write is one slot behind the cursor, and the clamped read is at
        // most `max_delay_samples`, so the ring (one frame longer) holds it.
        self.ring.back(frames.min(self.max_delay_samples) + 1)
    }
}

/// One sample of one uncoupled channel: read the feedback tap, write the line,
/// read the output tap (the wet sample).
///
/// The delay `d` is a [`ReadPosition`] and the feedback `fb` the raw `F` of a
/// [`Feedback`](tutti_units::Feedback): this is the per-frame read, called once per sample
/// per channel, and its caller holds the delay as a position and lowers the feedback once
/// per frame after the clamp that makes it one.
#[inline(always)]
pub(crate) fn delay_step<F: Float>(
    line: &mut DelayLine<F>,
    x: F,
    d: ReadPosition,
    fb: F,
    interp: InterpolationMode,
) -> F {
    let tap = line.read_sample(d.saturating_sub(Samples::new(1)), interp);
    line.push_sample(x + tap * fb);
    line.read_sample(d, interp)
}

#[cfg(test)]
mod tests;
