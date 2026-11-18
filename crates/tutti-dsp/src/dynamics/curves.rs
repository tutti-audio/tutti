//! The gain curves and conversions the dynamics processors share.
//!
//! The gain-computation curves, shared by the compressor, the gate and the
//! limiter, and the amplitude-to-decibel conversion that floors silence.

use tutti_units::{Amplitude, CompressionRatio, Db};

/// Amplitude as decibels, floored: an amplitude of zero or below is `Db::FLOOR`
/// (`-144`, about the 24-bit noise floor) rather than `-inf`.
///
/// Every detector reads its level through this function, so they all pin silence
/// at the one floor and a level can be compared, clamped and displayed without
/// special-casing infinity. The gain curves also accept `-inf`:
/// [`compute_compressor_gain_reduction`] sends it down the `input_db <= below`
/// branch and returns `Db::UNITY`.
#[inline]
pub fn amplitude_to_db(amp: impl Into<Amplitude>) -> Db {
    let amp = amp.into();
    if amp.get() <= 0.0 {
        Db::FLOOR
    } else {
        Db::from_amplitude(amp)
    }
}

/// Pure compressor gain reduction (hard/soft knee).
#[inline]
pub fn compute_compressor_gain_reduction(
    input_db: impl Into<Db>,
    threshold: impl Into<Db>,
    ratio: impl Into<CompressionRatio>,
    knee: impl Into<Db>,
) -> Db {
    let input_db = input_db.into().get();
    let threshold = threshold.into().get();
    // The ratio is an `f32` unit; the curve is `f64`.
    let ratio = f64::from(ratio.into().get());
    let knee = knee.into().get();
    if knee <= 0.0 {
        let over_db = (input_db - threshold).max(0.0);
        Db(over_db * (1.0 - 1.0 / ratio))
    } else {
        let half_knee = knee / 2.0;
        let below = threshold - half_knee;
        let above = threshold + half_knee;

        if input_db <= below {
            Db::UNITY
        } else if input_db >= above {
            let over_db = input_db - threshold;
            Db(over_db * (1.0 - 1.0 / ratio))
        } else {
            let x = input_db - below;
            let slope = (1.0 - 1.0 / ratio) / (2.0 * knee);
            Db(slope * x * x)
        }
    }
}

/// The upward counterpart of [`compute_compressor_gain_reduction`]: the lift, in dB and `>= 0`,
/// a level under the threshold is given.
///
/// It is the reduction curve mirrored about the threshold: a level `x` dB under it, through
/// the same knee and ratio, is lifted by what a level `x` dB over it is reduced by, so a
/// ratio of 1 lifts nothing and the lift is continuous at the knee's edges.
#[inline]
pub(super) fn compute_upward_gain_lift(
    input_db: Db,
    threshold: Db,
    ratio: CompressionRatio,
    knee: Db,
) -> Db {
    let mirrored = Db(2.0 * threshold.get() - input_db.get());
    compute_compressor_gain_reduction(mirrored, threshold, ratio, knee)
}

/// The gate's gain: `range` closed (`gate_level` 0) to 1 open (`gate_level` 1),
/// from the smoothed open fraction (an amplitude, 0 closed to 1 open) and the range.
#[inline]
pub(crate) fn gate_gain_at(gate_level: Amplitude, range: Amplitude) -> Amplitude {
    let range = range.get();
    Amplitude(range + gate_level.get() * (1.0 - range))
}

/// Pure limiter gain computation: the transfer curve of [`LimiterCurve`] in
/// dB, for a threshold and ceiling that may change every frame.
///
/// The threshold drives the input: the gain is `drive = ceiling - threshold`
/// up to the threshold, and `ceiling / peak` past it, so the output is
/// `min(peak * drive, ceiling)`. Returns the linear gain to apply given the
/// input peak level, threshold, and ceiling (all in dB).
#[inline]
pub fn compute_limiter_gain(
    peak_db: impl Into<Db>,
    threshold: impl Into<Db>,
    ceiling: impl Into<Db>,
) -> Amplitude {
    let peak_db = peak_db.into().get();
    let threshold = threshold.into().get();
    let ceiling = ceiling.into().get();
    if peak_db > threshold {
        // The gain that puts the peak at the ceiling, taken as the difference of the two
        // levels: it does not lose the ceiling to cancellation when the threshold is far
        // below it.
        Db(ceiling - peak_db).to_amplitude()
    } else {
        Db(ceiling - threshold).to_amplitude()
    }
}

/// [`compute_limiter_gain`] for a threshold and ceiling that hold across a
/// block, evaluated on the linear peak: what depends on only those two is
/// computed once, and a frame is a compare and a divide.
///
/// # The curve
///
/// The threshold drives the input. With `drive = ceiling - threshold` (a
/// gain, in dB a difference) the gain at a linked peak is
/// `min(drive, ceiling / peak)`, so the output peak is
/// `min(peak * drive, ceiling)`: a signal under the threshold is moved by
/// the drive (a boost when the ceiling is above the threshold, a cut when
/// below), and a louder one settles at the ceiling. The two pieces meet at
/// `peak == threshold`, so the curve is continuous and monotonic whatever
/// the ordering of ceiling and threshold.
///
/// Over the threshold the gain is `ceiling / peak`, the ceiling's amplitude over the peak
/// (which is `10^(drive/20) * threshold_lin / peak`); the comparison
/// `peak_db > threshold` is `peak > threshold_lin` (the log is monotonic).
///
/// **Silence and the extremes.** A zero peak is under any finite threshold, so exact digital
/// silence takes the `below` gain `10^((ceiling - threshold)/20)`. To keep that boost
/// finite (`0 * inf` is NaN) the gap is clamped to `MAX_GAP`, the threshold amplitude
/// to `f64::MIN_POSITIVE` and the ceiling's amplitude to `MAX_OVER` (the output is a sample
/// of at least `f32` width, so a boost stays far from `f32::MAX`) and to
/// `f64::MAX * threshold` (so `over / peak`, with a peak over the threshold, never
/// overflows: where the threshold is clamped a ceiling above 4 holds at 4); and `below` is
/// held to `over / threshold`, so a peak under the threshold is never raised past the
/// ceiling where the clamps bite (a threshold under -6165 dB, or `-inf`). None of this moves
/// a curve whose ceiling is under 740 dB (1e37) and no more than about 6160 dB over its
/// threshold (`f64::MAX` times the threshold's amplitude).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterCurve {
    /// The threshold as an amplitude.
    threshold: Amplitude,
    /// The ceiling as an amplitude: the gain over the threshold is this over the peak.
    over: Amplitude,
    /// The gain at or under the threshold.
    below: Amplitude,
}

impl LimiterCurve {
    /// The largest `ceiling - threshold` gap used: `10^(6000/20)` is 1e300, finite.
    const MAX_GAP: Db = Db(6000.0);

    /// The largest ceiling amplitude used: the output is a sample of at least `f32` width, so a
    /// boost's output stays far from `f32::MAX`.
    const MAX_OVER: Amplitude = Amplitude(1.0e37);

    /// The curve for `threshold` and `ceiling`, held across a block.
    #[must_use]
    pub fn new(threshold: Db, ceiling: Db) -> Self {
        let gap = (ceiling - threshold).min(Self::MAX_GAP);
        let threshold_lin = threshold.to_amplitude().max(Amplitude(f64::MIN_POSITIVE));
        // `over / peak` for a peak over the threshold is at most `over / threshold`: held
        // to `f64::MAX`, so the gain is finite whatever the peak.
        let over = ceiling
            .to_amplitude()
            .min(Self::MAX_OVER)
            .min(threshold_lin * f64::MAX);
        Self {
            threshold: threshold_lin,
            over,
            below: gap
                .to_amplitude()
                .min(Amplitude(over.get() / threshold_lin.get())),
        }
    }

    /// The gain for a linked peak amplitude (`>= 0`; a NaN takes `below`).
    /// It divides by `peak` only when `peak > threshold_lin`, and relies on
    /// `threshold_lin > 0` (the `MIN_POSITIVE` clamp in `new`) so that the
    /// divisor is never zero.
    #[inline]
    pub fn gain(&self, peak: Amplitude) -> Amplitude {
        let peak = peak.get();
        Amplitude(if peak > self.threshold.get() {
            self.over.get() / peak
        } else {
            self.below.get()
        })
    }
}

#[cfg(test)]
mod tests;
