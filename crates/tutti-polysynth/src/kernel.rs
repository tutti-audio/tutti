//! Per-sample DSP primitives, each evaluated across a group of voice lanes at once.
//!
//! Everything here is a pure function of [`Lane`] groups (`f32x8` or `f64x4`): no state, no branches
//! on lane values (a lane-dependent branch is a `select`), no allocation. The
//! state these functions advance lives in `crate::bank`, which is the only
//! caller.
//!
//! These are *audio-rate* generators. `tutti-modulation`'s `LfoShape` waves are
//! control-rate (a control lane evaluates them) and not band-limited, which is why the synth does not reuse
//! them: a naive saw at 4 kHz folds most of its energy back below Nyquist as
//! inharmonic partials.

use crate::lane::Lane;
use tutti_units::{PhaseIncrement, Real};

/// The largest phase increment the band-limiting corrections support.
///
/// PolyBLEP and PolyBLAMP correct the samples within one increment either side
/// of a discontinuity. Past half a cycle per sample those two regions overlap
/// and the correction double-counts, so a pitch above Nyquist is clamped just
/// below it rather than rendered as garbage.
pub(crate) const MAX_INCREMENT: PhaseIncrement = PhaseIncrement::new(0.49);

/// The coefficients of `sin(y) / y` in `y²`: `(-1)^n / (2n + 1)!`.
const SIN_TERMS: [f64; 11] = {
    let mut terms = [1.0; 11];
    let mut factorial = 1.0;
    let mut n = 1;
    while n < 11 {
        factorial *= ((2 * n) * (2 * n + 1)) as f64;
        terms[n] = if n % 2 == 0 { 1.0 } else { -1.0 } / factorial;
        n += 1;
    }
    terms
};

/// `sin(2π·p)` for a phase `p` in turns, `0.0..1.0`.
///
/// Folds the phase into the quarter-wave around zero and evaluates an odd
/// Taylor polynomial there: to the 11th power at `f32`, where on `|x| <= π/2` the first
/// omitted term is `(π/2)^13 / 13! ≈ 5.7e-8`, below `f32` resolution at unity, and to
/// the 21st at `f64`, where it is `(π/2)^23 / 23! ≈ 1.2e-18`. So this is exact to
/// within rounding at either width. A vector `sin` from a libm would buy nothing but
/// a range reduction the phase representation makes unnecessary.
#[inline(always)]
pub(crate) fn sine<V: Lane>(p: V) -> V {
    let half = V::lit(0.5);
    let quarter = V::lit(0.25);
    // sin(2π·p) = -sin(2π·x) with x = p - 0.5 in [-0.5, 0.5).
    let x = p - half;
    // Mirror about ±0.25 so |x| <= 0.25: sin(π - a) = sin(a).
    let x = x.gt(quarter).pick(half - x, x);
    let x = x.lt(-quarter).pick(-half - x, x);
    let y = x * V::splat(V::Scalar::TAU);
    let y2 = y * y;
    // The polynomial's terms: `f32` stops at the 11th power, `f64` runs to the 21st.
    let terms = if V::Scalar::MANTISSA_DIGITS > 24 {
        SIN_TERMS.len()
    } else {
        6
    };
    let mut poly = V::splat(V::Scalar::from_design(SIN_TERMS[terms - 1]));
    for &c in SIN_TERMS[..terms - 1].iter().rev() {
        poly = poly.mul_add(y2, V::splat(V::Scalar::from_design(c)));
    }
    -(y * poly)
}

/// The two-sample polynomial band-limited step residual (Välimäki, 2007).
///
/// `t` is the phase in turns and `dt` the increment; the result is the
/// correction for a unit upward step at `t = 0`, scaled so that adding it
/// to a naive `-1 → +1` edge (a step of 2) rounds that edge. Nonzero only
/// within one increment of the wrap.
#[inline(always)]
pub(crate) fn poly_blep<V: Lane>(t: V, dt: V, inv_dt: V) -> V {
    // Just after the step: x in [0, 1), residual 2x - x² - 1.
    let x0 = t * inv_dt;
    let after = x0 + x0 - x0 * x0 - V::ONE;
    // Just before it: x in (-1, 0], residual x² + 2x + 1.
    let x1 = (t - V::ONE) * inv_dt;
    let before = x1 * x1 + x1 + x1 + V::ONE;
    let is_after = t.lt(dt);
    let is_before = t.gt(V::ONE - dt);
    is_after.pick(after, is_before.pick(before, V::ZERO))
}

/// The two-sample polynomial band-limited ramp residual — PolyBLEP's
/// integral, for a discontinuity in *slope* rather than value (Esqueda,
/// Välimäki & Bilbao, 2016).
///
/// Normalized like [`poly_blep`], which corrects a step of 2: this corrects a
/// slope change of 2 per sample, so the caller scales it by half the slope
/// change per sample.
#[inline(always)]
pub(crate) fn poly_blamp<V: Lane>(t: V, dt: V, inv_dt: V) -> V {
    let third = V::lit(0.333_333_333_333_333_3);
    let x0 = t * inv_dt - V::ONE;
    let after = -(x0 * x0 * x0) * third;
    let x1 = (t - V::ONE) * inv_dt + V::ONE;
    let before = x1 * x1 * x1 * third;
    let is_after = t.lt(dt);
    let is_before = t.gt(V::ONE - dt);
    is_after.pick(after, is_before.pick(before, V::ZERO))
}

/// Wrap a phase that is at most one cycle past `1.0` back into `0.0..1.0`.
///
/// Cheaper than `x - floor(x)` and exact for the only inputs it gets: a phase
/// in `0.0..1.0` plus an increment below one.
#[inline(always)]
pub(crate) fn wrap_once<V: Lane>(p: V) -> V {
    let wrapped = p - V::ONE;
    p.lt(V::ONE).pick(p, wrapped)
}

/// Band-limited sawtooth, rising from −1 to +1 over the cycle.
#[inline(always)]
pub(crate) fn saw<V: Lane>(p: V, dt: V, inv_dt: V) -> V {
    // The naive ramp falls by 2 at the wrap; subtracting the step residual
    // rounds that fall.
    (p + p - V::ONE) - poly_blep(p, dt, inv_dt)
}

/// Band-limited pulse, `+1` for the first `width` of the cycle and `-1` after.
#[inline(always)]
pub(crate) fn pulse<V: Lane>(p: V, width: V, dt: V, inv_dt: V) -> V {
    let naive = p.lt(width).pick(V::ONE, -V::ONE);
    // Rising edge at 0, falling edge at `width`.
    naive + poly_blep(p, dt, inv_dt) - poly_blep(wrap_once(p - width + V::ONE), dt, inv_dt)
}

/// Band-limited triangle, starting at 0 and rising, so a phase reset lands on
/// a zero crossing rather than a peak.
///
/// The naive triangle has continuous value but its slope flips between ±4 per
/// cycle at each corner — a change of 8 per cycle, `8·dt` per sample — so the
/// correction is a ramp residual, PolyBLAMP, scaled by half that change
/// (see [`poly_blamp`]): `4·dt`.
#[inline(always)]
pub(crate) fn triangle<V: Lane>(p: V, dt: V, inv_dt: V) -> V {
    let half = V::lit(0.5);
    // `q` is the phase measured from the trough, so the corners sit at q = 0
    // (trough, slope -4 → +4) and q = 0.5 (peak, +4 → -4).
    let q = wrap_once(p + V::lit(0.25));
    let naive = V::ONE - V::lit(4.0) * (q - half).abs();
    let scale = V::lit(4.0) * dt;
    naive + scale * (poly_blamp(q, dt, inv_dt) - poly_blamp(wrap_once(q + half), dt, inv_dt))
}

/// Pink noise from white, by Paul Kellet's three-pole "economy" filter.
///
/// Within about ±0.5 dB of a −3 dB/octave slope (Kellet's stated figure for the economy
/// form, above about 10 Hz at 44.1 kHz), and three multiply-adds per lane; the refined
/// seven-pole version is tighter than a synth voice's noise needs.
/// The output gain of `0.12` brings the RMS to about 0.2, near a sine at −14 dBFS, so the
/// noise oscillator sits at a level comparable to the pitched ones.
#[inline(always)]
pub(crate) fn pink<V: Lane>(white: V, b: &mut [V; 3]) -> V {
    b[0] = V::lit(0.997_65).mul_add(b[0], white * V::lit(0.099_046));
    b[1] = V::lit(0.963).mul_add(b[1], white * V::lit(0.296_516_4));
    b[2] = V::lit(0.57).mul_add(b[2], white * V::lit(1.052_691_3));
    (b[0] + b[1] + b[2] + white * V::lit(0.184_8)) * V::lit(0.12)
}

/// `tanh`, by the [7/6] Padé approximant clamped at its unity crossing.
///
/// The approximant reaches exactly `±1` near `|x| = 4.97` and is monotonic up
/// to there, so clamping the argument gives a saturator that never overshoots.
/// Worst error against `f64::tanh` is about `2e-4`, at the clamp; below
/// `|x| = 3` it is under `1e-6`. The ladder's feedback is the only caller, and
/// what it needs is a smooth, bounded, odd saturator — `wide`'s `tanh` is exact
/// to an ulp at roughly three times the cost, which the ladder pays per sample
/// per lane.
#[inline(always)]
pub(crate) fn tanh<V: Lane>(x: V) -> V {
    let x = x.clamp(V::lit(-4.97), V::lit(4.97));
    let x2 = x * x;
    let num = x * x2
        .mul_add(x2 + V::lit(378.0), V::lit(17_325.0))
        .mul_add(x2, V::lit(135_135.0));
    let den = V::lit(28.0)
        .mul_add(x2, V::lit(3_150.0))
        .mul_add(x2, V::lit(62_370.0))
        .mul_add(x2, V::lit(135_135.0));
    num / den
}

#[cfg(test)]
mod tests;
