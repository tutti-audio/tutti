//! A PI controller from a buffer's fill to a resampler's read ratio.

use tutti_units::{Hz, SampleRate, Samples};

use super::VariableRate;

/// The most a block may span of the loop's time constant: `w B` is held to at most this,
/// so the integration step stays small against the loop's own time.
const MAX_STEP: f64 = 0.25;

tutti_units::ranged! {
    /// The natural frequency of a [`FillController`]'s loop: `0.001..=1` Hz.
    /// [`new`](Self::new) refuses NaN and anything outside the range.
    LoopBandwidth(Hz), LoopBandwidthError, "a bandwidth from 0.001 to 1 Hz",
    |raw| raw.get() >= 0.001 && raw.get() <= 1.0
}

impl LoopBandwidth {
    /// The slowest bandwidth, 0.001 Hz.
    pub const MIN: Self = Self(Hz::new(0.001));
    /// The fastest bandwidth, 1 Hz.
    pub const MAX: Self = Self(Hz::new(1.0));
    /// The bandwidth of a controller that is given none: 0.05 Hz, slow enough to follow the
    /// drift of a clock and not the jitter of a block.
    pub const DEFAULT: Self = Self(Hz::new(0.05));

    /// The bandwidth in hertz.
    #[must_use]
    pub const fn hz(self) -> Hz {
        self.0
    }
}

/// A PI loop from a buffer's fill to a read ratio.
///
/// # Model
///
/// Per output frame the fill `f` gains what the producer writes and loses what the
/// resampler reads, `r` input frames, so with `e = f - target` and a producer that writes
/// `1 + d` frames per output frame, `e' = (1 + d) - r` per frame. The controller reads
/// `r = 1 + u`, with
///
/// ```text
/// u = kp e + ki * (sum of e over the frames so far)
/// ```
///
/// which gives `e'' + kp e' + ki e = 0`: a second-order loop with natural frequency
/// `w = 2 pi bandwidth / rate` radians per frame and damping 1 (critically damped) for
/// `kp = 2 w` and `ki = w^2`. A block of `B` frames adds `ki B e` to the sum once. After the
/// ratio is steady the loop forgets its start: `e(t) = d t e^(-w t)` for a producer
/// that starts `d` fast (peak `d / (e w)` frames at `t = 1 / w`, 2.8 frames for 50 ppm at
/// 48 kHz and the default bandwidth), and the ratio approaches `1 + d` as
/// `1 + d (1 - (1 - w t) e^(-w t))`. The time constant is `1 / w`, 3.2 seconds by default,
/// and the error is under 1% of its peak after `8 / w`.
///
/// The fill is a whole number of frames, so the error is read to within a frame and the
/// ratio carries `kp` of dither (13 ppm at the default); the sum averages it out.
///
/// # Range
///
/// The ratio is held within the resampler's range, and the sum stops where the held ratio
/// stops so it does not wind up.
///
/// The ratio is a [`VariableRate`], input frames per output frame: the one quantity the fill
/// loop and the resampler share, with no sample rate or pitch in it. The rate in
/// [`new`](Self::new) turns the bandwidth in hertz into radians per frame; the block
/// cadence is the argument of [`observe`](Self::observe), so the controller holds no clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FillController {
    target: Samples,
    rate: SampleRate,
    bandwidth: LoopBandwidth,
    /// `ki` times the error summed over the frames so far: the ratio offset the loop has
    /// learned, dimensionless.
    integral: f64,
}

impl FillController {
    /// A controller holding `target` frames of fill, at the output `rate`, with the
    /// [`LoopBandwidth::DEFAULT`].
    #[must_use]
    pub const fn new(target: Samples, rate: SampleRate) -> Self {
        Self {
            target,
            rate,
            bandwidth: LoopBandwidth::DEFAULT,
            integral: 0.0,
        }
    }

    /// The loop bandwidth, its natural frequency.
    #[must_use]
    pub const fn bandwidth(mut self, bandwidth: LoopBandwidth) -> Self {
        self.bandwidth = bandwidth;
        self
    }

    /// The fill the loop holds.
    #[must_use]
    pub const fn target(&self) -> Samples {
        self.target
    }

    /// Forgets what the loop learned: the ratio is unity again on the next observation
    /// with the fill at target.
    pub fn reset(&mut self) {
        self.integral = 0.0;
    }

    /// The ratio to read the next block at, given the `fill` observed and the `block` of
    /// output frames the last ratio ran for. No allocation.
    ///
    /// The bandwidth is held under a quarter of the block rate (`w B <= 1/4`) so a long
    /// block does not make the loop unstable.
    #[must_use]
    pub fn observe(&mut self, fill: Samples, block: Samples) -> VariableRate {
        let error = fill.get() as f64 - self.target.get() as f64;
        let span = block.get() as f64;
        let omega = self
            .bandwidth
            .hz()
            .angle_per_frame(self.rate)
            .get()
            .min(MAX_STEP / span.max(1.0));
        let limit = VariableRate::MAX.get() - 1.0;
        self.integral = (self.integral + omega * omega * span * error).clamp(-limit, limit);
        let ratio = 1.0 + 2.0 * omega * error + self.integral;
        VariableRate::clamped(ratio)
    }
}

#[cfg(test)]
mod tests;
