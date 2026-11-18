//! The gain curves and conversions the dynamics processors share.
//!
//! The gain-computation curves live here rather than in any one processor, so
//! a compressor and a gate pin silence at the same floor.

use tutti_units::{Amplitude, CompressionRatio, Db};

/// Amplitude as decibels, for the dynamics detectors.
///
/// **This is where the dynamics floor is applied.** `Db::from_amplitude` gives
/// silence `-inf`, because what to substitute for it is the consumer's choice;
/// this is the consumer that chooses, so every detector pins at one floor:
/// `Db::FLOOR` is `-144`, roughly the 24-bit noise floor. Per-site floors are
/// what let this diverge — a detector at `-96` against `tutti-export`'s `-144`
/// against a third site with none. The exact value is inaudible in a
/// compressor, since anything near either floor is far below any usable
/// threshold; the agreement is the point.
///
/// The guard buys agreement, not `NaN` containment: the gain curves tolerate
/// `-inf` on their own ([`compute_compressor_gain_reduction`] sends it down the
/// `input_db <= below` branch and returns `Db(0.0)`). What it does buy is a
/// level every caller can compare, clamp and display without special-casing
/// infinity.
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
    let ratio = ratio.into().get();
    let knee = knee.into().get();
    if knee <= 0.0 {
        let over_db = (input_db - threshold).max(0.0);
        Db(over_db * (1.0 - 1.0 / ratio))
    } else {
        let half_knee = knee / 2.0;
        let below = threshold - half_knee;
        let above = threshold + half_knee;

        if input_db <= below {
            Db(0.0)
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

/// The gate's gain: `range_linear` closed (`gate_level` 0) to 1 open (`gate_level` 1),
/// from the open fraction (unitless, 0..1) and the range already as an amplitude.
#[inline]
pub(crate) fn gate_gain_at(gate_level: f32, range_linear: f32) -> Amplitude {
    Amplitude(range_linear + gate_level * (1.0 - range_linear))
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
/// finite (`0 * inf` is NaN) the gap is clamped to `MAX_GAP_DB`, the threshold amplitude
/// to `f32::MIN_POSITIVE` and the ceiling's amplitude to `MAX_OVER` and to
/// `f32::MAX * threshold` (so `over / peak`, with a peak over the threshold, never
/// overflows: where the threshold is clamped a ceiling above 4 holds at 4); and `below` is
/// held to `over / threshold`, so a peak under the threshold is never raised past the
/// ceiling where the clamps bite (a threshold under -758 dB, or `-inf`). None of this moves
/// a curve whose ceiling is under 1e37 (740 dB) and no more than about 770 dB over its
/// threshold (`f32::MAX` times the threshold's amplitude): a threshold at or above -144 dB
/// keeps any ceiling up to about +626 dB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterCurve {
    /// The threshold as an amplitude.
    threshold: f32,
    /// The ceiling as an amplitude: the gain over the threshold is this over the peak.
    over: f32,
    /// The gain at or under the threshold.
    below: f32,
}

impl LimiterCurve {
    /// The largest `ceiling - threshold` gap used, in dB: `10^(760/20)` is
    /// 1e38, finite in `f32` (the limit is about 770 dB).
    const MAX_GAP_DB: f32 = 760.0;

    /// The largest ceiling amplitude used: a boost's output stays far from `f32::MAX`.
    const MAX_OVER: f32 = 1.0e37;

    /// The curve for `threshold` and `ceiling`, held across a block.
    #[must_use]
    pub fn new(threshold: Db, ceiling: Db) -> Self {
        let (threshold, ceiling) = (threshold.get(), ceiling.get());
        let gap = (ceiling - threshold).min(Self::MAX_GAP_DB);
        let threshold_lin = Db(threshold).to_amplitude().get().max(f32::MIN_POSITIVE);
        // `over / peak` for a peak over the threshold is at most `over / threshold`: held
        // to `f32::MAX`, so the gain is finite whatever the peak.
        let over = Db(ceiling)
            .to_amplitude()
            .get()
            .min(Self::MAX_OVER)
            .min(f32::MAX * threshold_lin);
        Self {
            threshold: threshold_lin,
            over,
            below: Db(gap).to_amplitude().get().min(over / threshold_lin),
        }
    }

    /// The gain for a linked peak amplitude (`>= 0`; a NaN takes `below`).
    /// It divides by `peak` only when `peak > threshold_lin`, and relies on
    /// `threshold_lin > 0` (the `MIN_POSITIVE` clamp in `new`) so that the
    /// divisor is never zero.
    #[inline]
    pub fn gain(&self, peak: Amplitude) -> Amplitude {
        let peak = peak.get();
        Amplitude(if peak > self.threshold {
            self.over / peak
        } else {
            self.below
        })
    }
}

#[cfg(test)]
mod tests;
