//! The gain curves and conversions the dynamics processors share.
//!
//! The gain-computation curves, shared by the compressor, the gate and the
//! limiter, and the amplitude-to-decibel conversion that floors silence.

use tutti_buffers::Float;
use tutti_units::{Amplitude, CompressionRatio, Db};

/// The decibels of a level at the working precision `F`, floored at [`Db::FLOOR`]: a level of
/// zero or below is the floor. The maths is [`Db::from_amplitude`]'s, `20 log10`, in `F`: a unit
/// is `f64`, not generic over `F`, so a per-sample level in `F` is computed here and
/// [`amplitude_to_db`] at `f64` gives the same bits.
#[inline]
pub(crate) fn level_db<F: Float>(level: F) -> F {
    if level <= F::ZERO {
        F::from_f64(Db::FLOOR.get())
    } else {
        F::lit(20.0) * level.log10()
    }
}

/// The amplitude ratio of `db` decibels at the working precision `F`, `10^(db / 20)` in `F` as
/// [`Db::to_amplitude`] has it at `f64`: `-inf` is 0 and `+inf` is `+inf`.
#[inline]
pub(crate) fn db_gain<F: Float>(db: F) -> F {
    F::lit(10.0).powf(db / F::lit(20.0))
}

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
    Db(gain_reduction(
        input_db.into().get(),
        threshold.into().get(),
        // The ratio is an `f32` unit; the curve is `f64`.
        f64::from(ratio.into().get()),
        knee.into().get(),
    ))
}

/// [`compute_compressor_gain_reduction`] over the bare floats at the working precision `F`,
/// all in dB but the ratio.
#[inline]
pub(crate) fn gain_reduction<F: Float>(input_db: F, threshold: F, ratio: F, knee: F) -> F {
    let slope = F::ONE - F::ONE / ratio;
    if knee <= F::ZERO {
        let over_db = (input_db - threshold).max(F::ZERO);
        over_db * slope
    } else {
        let two = F::lit(2.0);
        let half_knee = knee / two;
        let below = threshold - half_knee;
        let above = threshold + half_knee;

        if input_db <= below {
            F::ZERO
        } else if input_db >= above {
            (input_db - threshold) * slope
        } else {
            let x = input_db - below;
            slope / (two * knee) * x * x
        }
    }
}

/// The upward counterpart of [`gain_reduction`], over the bare floats at the working precision
/// `F`: the lift, in dB and `>= 0`, a level under the threshold is given.
///
/// It is the reduction curve mirrored about the threshold: a level `x` dB under it, through
/// the same knee and ratio, is lifted by what a level `x` dB over it is reduced by, so a
/// ratio of 1 lifts nothing and the lift is continuous at the knee's edges.
#[inline]
pub(super) fn upward_gain_lift<F: Float>(input_db: F, threshold: F, ratio: F, knee: F) -> F {
    let mirrored = F::lit(2.0) * threshold - input_db;
    gain_reduction(mirrored, threshold, ratio, knee)
}

/// The gate's gain: `range` closed (`gate_level` 0) to 1 open (`gate_level` 1),
/// from the open fraction (unitless, 0..1) and the range as an amplitude.
#[inline]
pub(crate) fn gate_gain_at<F: Float>(gate_level: F, range: F) -> F {
    range + gate_level * (F::ONE - range)
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
    Amplitude(limiter_gain(
        peak_db.into().get(),
        threshold.into().get(),
        ceiling.into().get(),
    ))
}

/// [`compute_limiter_gain`] over the bare floats at the working precision `F`, all in dB.
#[inline]
pub(crate) fn limiter_gain<F: Float>(peak_db: F, threshold: F, ceiling: F) -> F {
    if peak_db > threshold {
        // The gain that puts the peak at the ceiling, taken as the difference of the two
        // levels: it does not lose the ceiling to cancellation when the threshold is far
        // below it.
        db_gain(ceiling - peak_db)
    } else {
        db_gain(ceiling - threshold)
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
/// to `f32::MIN_POSITIVE` and the ceiling's amplitude to `MAX_OVER` and to
/// `f32::MAX * threshold` (so `over / peak`, with a peak over the threshold, never
/// overflows: where the threshold is clamped a ceiling above 4 holds at 4); and `below` is
/// held to `over / threshold`, so a peak under the threshold is never raised past the
/// ceiling where the clamps bite (a threshold under -758 dB, or `-inf`). None of this moves
/// a curve whose ceiling is under 1e37 (740 dB) and no more than about 770 dB over its
/// threshold (`f32::MAX` times the threshold's amplitude): a threshold at or above -144 dB
/// keeps any ceiling up to about +626 dB. The limits are `f32`'s because `f32` is the narrowest
/// width a bank runs in: the curve is `f64`, and its gains narrow to the bank's float.
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
    /// The largest `ceiling - threshold` gap used: `10^(760/20)` is 1e38, finite in `f32`
    /// (the limit is about 770 dB). The limits are `f32`-sized because the curve is narrowed
    /// into a bank of `F`, and `f32` is the narrowest `F`: at that instantiation a larger gap
    /// is an infinite gain (`a_threshold_past_the_clamp_never_overshoots_the_ceiling`).
    const MAX_GAP: Db = Db(760.0);

    /// The largest ceiling amplitude used: a boost's output stays far from `f32::MAX`, the
    /// same `f32` boundary as [`MAX_GAP`](Self::MAX_GAP).
    const MAX_OVER: Amplitude = Amplitude(1.0e37);

    /// The curve for `threshold` and `ceiling`, held across a block.
    #[must_use]
    pub fn new(threshold: Db, ceiling: Db) -> Self {
        let gap = (ceiling - threshold).min(Self::MAX_GAP);
        let threshold_lin = threshold
            .to_amplitude()
            .max(Amplitude(f64::from(f32::MIN_POSITIVE)));
        // `over / peak` for a peak over the threshold is at most `over / threshold`: held
        // to `f32::MAX`, so the gain is finite whatever the peak.
        let over = ceiling
            .to_amplitude()
            .min(Self::MAX_OVER)
            .min(threshold_lin * f64::from(f32::MAX));
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
        Amplitude(self.widened::<f64>().gain(peak.get()))
    }

    /// This curve's figures widened to `F`, once, for a per-sample [`WideCurve::gain`].
    pub(crate) fn widened<F: Float>(&self) -> WideCurve<F> {
        WideCurve {
            threshold: F::from_f64(self.threshold.get()),
            over: F::from_f64(self.over.get()),
            below: F::from_f64(self.below.get()),
        }
    }
}

/// A [`LimiterCurve`] widened to `F`: the figures as bare amplitudes, so a per-sample gain
/// widens nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WideCurve<F> {
    threshold: F,
    over: F,
    below: F,
}

impl<F: Float> WideCurve<F> {
    /// The gain for a linked peak held in `F`, as [`LimiterCurve::gain`].
    #[inline]
    pub(crate) fn gain(&self, peak: F) -> F {
        if peak > self.threshold {
            self.over / peak
        } else {
            self.below
        }
    }
}

#[cfg(test)]
mod tests;
