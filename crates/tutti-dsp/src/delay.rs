//! The fractional-read delay ring and its interpolation: a delay whose time is
//! modulated must interpolate between taps or it steps audibly.

mod bank;
mod layout;
mod max_delay;
mod notes;
mod position;
mod ring;
mod routing;
mod taps;
mod times;

pub use bank::{DelayBank, DelayRamps, DelaySignals};
pub use layout::{
    DELAY_SLEW, DelayControls, DelayLayout, DelayLayoutBuilder, DelayLayoutError, DelayTiming,
    MAX_DELAY_CHANNELS,
};
pub(crate) use max_delay::MAX_FRAMES;
pub use max_delay::{MaxDelay, MaxDelayError};
pub use notes::{NoteTimes, NoteTimesError};
pub use position::{DelayError, LinePosition};
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
/// lookahead. Capacity is fixed at construction and the line never grows, so it never
/// allocates on the audio thread: a read takes a [`LinePosition`], a delay the line holds,
/// which [`position`](Self::position) refuses past the line and [`held`](Self::held) holds
/// at its end for a delay driven per frame.
///
/// [`read_sample`]: Self::read_sample
/// [`read_whole`]: Self::read_whole
#[derive(Clone)]
pub struct DelayLine<F> {
    ring: DelayRing<F>,
    max_delay_samples: usize,
}

/// The frames the ring holds past the longest delay: the taps an interpolation reads beyond
/// a delay at the end of the line (a linear read one, a cubic read two).
const SPARE: usize = 3;

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
    /// The backing buffer is `SPARE` frames longer than the maximum delay, so that
    /// an interpolating read at exactly `capacity` still has the taps it blends toward.
    /// Allocates — build before going live.
    #[must_use]
    pub fn new(capacity: Samples) -> Self {
        let max_delay_samples = capacity.get();
        Self {
            ring: DelayRing::new(max_delay_samples + SPARE),
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
    /// `delay` is a [`LinePosition`]: a fractional frame count held to `2^-32` frame at any
    /// distance, inside the line. Build it once ([`position`](Self::position)) for a fixed
    /// delay and per frame ([`held`](Self::held)) for a driven one, from a time
    /// ([`ReadPosition::of`]) moved per sample by [`Float::offset_position`]. `0` is the
    /// sample just pushed. The interpolator takes the position's whole frames as the index and
    /// its fraction as an `F` weight ([`Float::split_position`]); the taps it blends toward
    /// past the line's end are the history older than the line, which the ring holds.
    ///
    /// Does not allocate. A position of a longer line is a logic error, caught by a debug
    /// assertion.
    #[inline]
    pub fn read_sample(&self, delay: LinePosition, mode: InterpolationMode) -> F {
        debug_assert!(
            delay.get() <= self.end().get(),
            "a delay past the line it reads"
        );
        let delay = delay.get();
        match mode {
            InterpolationMode::None => {
                // The nearest whole slot: half a frame on, truncated.
                let half = ReadPosition::from_count(delay.get().saturating_add(1 << 31));
                self.tap(half.frames().get())
            }
            InterpolationMode::Linear => {
                let (floor, frac) = F::split_position(delay);
                let floor = floor.get();
                let s0 = self.tap(floor);
                let s1 = self.tap(floor + 1);
                crate::lerp(s0, s1, frac)
            }
            InterpolationMode::CubicHermite => {
                let (floor, frac) = F::split_position(delay);
                let floor = floor.get();
                let sm1 = self.tap(floor.saturating_sub(1));
                let s0 = self.tap(floor);
                let s1 = self.tap(floor + 1);
                let s2 = self.tap(floor + 2);
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

    /// Reads the line `at`'s whole frames back from the newest write, no interpolation: `0`
    /// is the sample just pushed. Does not allocate.
    #[inline]
    #[must_use]
    pub fn read_whole(&self, at: LinePosition) -> F {
        debug_assert!(
            at.get() <= self.end().get(),
            "a delay past the line it reads"
        );
        self.tap(at.get().frames().get())
    }

    /// The frame `frames` back, up to the line's capacity and the spare taps past it.
    #[inline]
    fn tap(&self, frames: usize) -> F {
        // The newest write is one slot behind the cursor, and a read is at most
        // `max_delay_samples + SPARE - 1` back, so the ring (`SPARE` frames longer) holds it.
        self.ring.back(frames + 1)
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
    // The delay is driven per frame: it is held at the line's end where it is read.
    let d = line.held(d);
    let tap = line.read_sample(d.saturating_sub(Samples::new(1)), interp);
    line.push_sample(x + tap * fb);
    line.read_sample(d, interp)
}

#[cfg(test)]
mod tests;
