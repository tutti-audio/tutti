//! The fractional-read delay ring and its interpolation: a delay whose time is
//! modulated must interpolate between taps or it steps audibly.

mod bank;
mod layout;
mod max_delay;
mod ring;
mod routing;
mod taps;
mod times;

pub use bank::{DelayBank, DelayGlides, DelayLanes};
pub use layout::{DELAY_SLEW, DelayControls, DelayLayout, MAX_DELAY_CHANNELS};
pub use max_delay::{MaxDelay, MaxDelayError};
pub(crate) use ring::DelayRing;
pub use routing::{Routing, RoutingError};
pub(crate) use taps::{ring_seconds, trips};
pub use times::{ChannelTimes, ChannelTimesError};

use tutti_types::Samples;

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

/// A fractional-delay ring buffer: push a sample per frame, read back at any
/// real-valued delay.
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
pub struct DelayLine {
    ring: DelayRing,
    max_delay_samples: usize,
}

impl std::fmt::Debug for DelayLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelayLine")
            .field("max_delay_samples", &self.max_delay_samples)
            .finish_non_exhaustive()
    }
}

impl DelayLine {
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
    pub fn push_sample(&mut self, sample: f32) {
        self.ring.push(sample);
    }

    /// Reads the line `delay_samples` frames back from the newest write,
    /// interpolating per `mode`.
    ///
    /// `delay_samples` is a **fractional frame count**, not seconds: the caller
    /// converts ([`SampleSpan::from_seconds`](tutti_types::SampleSpan::from_seconds)),
    /// because keeping the fraction is the whole point of an interpolated read.
    /// `0.0` is the sample just pushed. It is a bare `f32` because this is the
    /// per-sample kernel read: a caller lowers its span to the raw once per block
    /// (or once per frame for a driven delay) and reads at it.
    ///
    /// The delay is clamped to `0.0..=max_delay_samples`, so an over-long
    /// request shortens silently rather than panicking. Does not allocate.
    #[inline]
    pub fn read_sample(&self, delay_samples: f32, mode: InterpolationMode) -> f32 {
        // Defensive only: a caller holds its delay inside the line, so this clamp is
        // reached by a caller that does not.
        let delay = delay_samples.clamp(0.0, self.max_delay_samples as f32);
        match mode {
            InterpolationMode::None => {
                let idx = delay.round() as usize;
                self.read_whole(idx)
            }
            InterpolationMode::Linear => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let s0 = self.read_whole(floor);
                let s1 = self.read_whole(floor + 1);
                s0 + frac * (s1 - s0)
            }
            InterpolationMode::CubicHermite => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
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
    pub fn read_whole(&self, frames: usize) -> f32 {
        // The newest write is one slot behind the cursor, and the clamped read is at
        // most `max_delay_samples`, so the ring (one frame longer) holds it.
        self.ring.back(frames.min(self.max_delay_samples) + 1)
    }
}

/// One sample of one uncoupled channel: read the feedback tap, write the line,
/// read the output tap (the wet sample).
///
/// The delay `d` and the feedback `fb` are the raw `f32` of a
/// [`SampleSpan`](tutti_types::SampleSpan) of frames and a
/// [`Feedback`](tutti_types::Feedback): this is the per-frame read, called once per sample
/// per channel, and its caller lowers its span to the position once per frame after the
/// clamp that makes it a span (a delay glide's value is already an `f32` of frames).
#[inline(always)]
pub fn delay_step(line: &mut DelayLine, x: f32, d: f32, fb: f32, interp: InterpolationMode) -> f32 {
    let tap = line.read_sample((d.max(1.0) - 1.0).max(0.0), interp);
    line.push_sample(x + tap * fb);
    line.read_sample(d, interp)
}

#[cfg(test)]
mod tests;
