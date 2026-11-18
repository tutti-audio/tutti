//! How a source gets to where it is aimed without zippering: the
//! [`AngleSmoother`] the binaural panner steps.

use tutti_units::{Azimuth, Direction, Elevation, SampleRate, Samples, Seconds};

/// The time constant of the position de-zipper, in audio time.
const SMOOTH_TIME: Seconds = Seconds(0.05);

/// One smoother per coordinate, each with its own arithmetic: the bearing takes
/// the short arc, the height is a plain ramp.
///
/// Shared because that asymmetry is easy to get wrong twice. Give the bearing
/// the linear form and a source crossing behind the listener (170° → -170°, a
/// 20° move) sweeps 340° the wrong way around the head.
///
/// Each coordinate runs the one-pole recurrence `y += c * (x - y)` in its own
/// unit: the bearing along the short arc ([`Azimuth::lerp_shortest`]), the
/// height along a line ([`Elevation::lerp`]). This type derives the coefficient
/// from the time constant and the stride.
#[derive(Clone, Debug)]
pub(crate) struct AngleSmoother {
    azimuth: Azimuth,
    elevation: Elevation,
    /// The fraction of the gap one step closes.
    coeff: f64,
    /// Samples of audio each step stands for: the frame length of a smoother
    /// stepped once per frame. Kept so
    /// [`set_sample_rate`](Self::set_sample_rate) can re-derive the coefficient.
    stride: Samples,
}

impl AngleSmoother {
    /// A smoother with a 50 ms time constant of audio at `sample_rate`, for
    /// a caller that steps once per `samples_per_step` samples: the convolution
    /// frame length for the binaural panner, which steps once per frame.
    pub(crate) fn new(sample_rate: SampleRate, samples_per_step: Samples) -> Self {
        Self {
            azimuth: Azimuth::FRONT,
            elevation: Elevation::LEVEL,
            coeff: Self::coeff(sample_rate, samples_per_step),
            stride: samples_per_step,
        }
    }

    /// The fraction of the gap one step closes: the one-pole coefficient at
    /// the rate the smoother is stepped, `sr / stride`
    /// ([`Seconds::one_pole_coeff`]).
    fn coeff(sample_rate: SampleRate, stride: Samples) -> f64 {
        // A step with no rate (a zero stride) is a smoother that jumps:
        // `one_pole_coeff`'s answer for a rate that is no rate.
        SampleRate::new(sample_rate.get() / stride.get() as f64)
            .map_or(1.0, |rate| SMOOTH_TIME.one_pole_coeff(rate))
    }

    /// Retune for a new sample rate, keeping the stride
    /// [`new`](Self::new) was given, so the 50 ms ramp holds at any rate.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.coeff = Self::coeff(sample_rate, self.stride);
    }

    /// Discard the in-flight ramp, seating both coordinates *on* the commanded
    /// position.
    ///
    /// This is what a panner's [`Node::reset`] clears here: the interpolation
    /// position is the smoother's only runtime state, and the commanded bearing
    /// and height are caller-set configuration a reset must leave alone. Seating
    /// on the target rather than at zero is the difference between resuming
    /// silently and sweeping the source in from front-centre over the ramp.
    ///
    /// [`Node::reset`]: tutti_graph::Node::reset
    pub(crate) fn reset_to_target(&mut self, target: Direction) {
        self.azimuth = target.azimuth.wrap();
        self.elevation = Elevation::new_clamped(target.elevation.get());
    }

    /// Where the ramp currently is: the direction the last step returned (or
    /// the seeded one), without advancing.
    ///
    /// The binaural panner reads this as the position it renders from and
    /// reseats on.
    #[inline]
    pub(crate) fn current(&self) -> Direction {
        Direction::new(self.azimuth, self.elevation)
    }

    /// Advance one step toward the target and return the smoothed direction.
    ///
    /// The bearing takes the short way around: a plain `target - value` is
    /// -340 degrees from 170 to -170, which is 20 degrees to travel.
    /// [`Azimuth::shortest_arc_to`] is the correct difference, and the running
    /// value is re-wrapped each step so it never drifts out of `-180..180`.
    #[inline]
    pub(crate) fn step(&mut self, target: Direction) -> Direction {
        self.azimuth = self
            .azimuth
            .wrap()
            .lerp_shortest(target.azimuth.wrap(), self.coeff);
        self.elevation = self.elevation.lerp(target.elevation, self.coeff);
        Direction::new(self.azimuth, self.elevation)
    }
}

#[cfg(test)]
mod tests;
