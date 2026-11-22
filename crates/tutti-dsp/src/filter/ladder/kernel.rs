//! The Moog-style four-stage ladder: coefficients and the cheap per-sample step
//! ([`ladder_step`]).
//!
//! Unlike the SVF this is a *nonlinear* filter: the resonance feedback runs
//! through a `tanh` saturator, so pushing resonance or drive grits and
//! compresses rather than blowing up. That saturation is the character, not a
//! safety measure. Where the saturators sit is a [`LadderSaturation`]: [`ladder_step`] is the
//! cheap form, with one saturator on the solved loop input, and
//! [`nonlinear_ladder_step`](crate::nonlinear_ladder_step) puts one at every stage. Both are
//! written once over [`SimdLanes`] with the saturator passed in, so a caller running SIMD lanes
//! passes its lane `tanh`; [`LadderBank`](crate::LadderBank) runs either with the float's own.
//!
//! The resonance loop is closed with **no unit delay** (zero-delay feedback,
//! Zavalishin, *The Art of VA Filter Design*, the ladder chapter): the loop
//! `u = x - k * y4` is solved for `u` within the sample, using that each trapezoidal
//! one-pole is affine in its input, so the filter realises the continuous ladder
//! `1 / ((1 + s/wc)^4 + k)` under the bilinear map prewarped at the cutoff, with
//! the resonance where the continuous model puts it.

use tutti_buffers::Float;

use crate::SimdLanes;
use crate::design::prewarp;
use tutti_units::{Hz, Resonance, SampleRate};

/// Which tap of the four-stage ladder is taken as the output, and so what
/// response and slope the filter presents.
///
/// All four run the same four stages and the same feedback; they differ only in
/// which tap is read, so the type costs nothing to switch. The 24 dB variants
/// are the classic Moog sound.
///
/// There is no default type: the tap is what the filter is, so a filter names it.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
#[non_exhaustive]
pub enum LadderType {
    /// Low-pass at 12 dB/octave — the second stage tap.
    LP12,
    /// Low-pass at 24 dB/octave — the fourth stage tap, the classic Moog
    /// response.
    LP24,
    /// High-pass at 12 dB/octave: the stage mix `u - 2*lp1 + lp2`, which is
    /// `(s / (1 + s))^2` for the stages' `1 / (1 + s)`.
    HP12,
    /// High-pass at 24 dB/octave: the binomial stage mix
    /// `u - 4*lp1 + 6*lp2 - 4*lp3 + lp4`, which is `(s / (1 + s))^4`.
    HP24,
}

/// Where a ladder's `tanh` saturators sit: the two models of its nonlinear zero-delay loop.
///
/// Both close the loop within the sample and have the same small-signal response, the
/// continuous ladder `1 / ((1 + s/wc)^4 + k)` under the bilinear map prewarped at the
/// cutoff; they differ in how a loud signal is saturated and in what a sample costs. There is
/// no default: each configuration states its own.
///
/// ```
/// use tutti_dsp::LadderSaturation;
///
/// // The cheap model for a voice engine, the full one for a filter node.
/// let _ = (LadderSaturation::Input, LadderSaturation::PerStage);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LadderSaturation {
    /// One saturator, on the loop's input: the loop is solved linearly in closed form
    /// ([`ladder_step`]) and its solution goes through one `tanh`, leaving the four stages
    /// linear. Per sample, one closed-form solve and one `tanh`. A constant input at
    /// resonance 0 settles to `tanh` applied once.
    Input,
    /// A saturator at each stage's input and in the loop: the implicit loop equation is
    /// solved by a capped Newton iteration ([`nonlinear_ladder_step`](crate::nonlinear_ladder_step)). Per sample, three
    /// evaluations of the four-stage chain, about 12 `tanh`, an order of magnitude
    /// more than [`Input`](Self::Input). A constant input at resonance 0 settles to `tanh`
    /// applied four times.
    PerStage,
}

/// The ladder design: its two coefficients solved in `f64` from its parameters, before they are
/// narrowed to the [`LadderCoefficients`] a step runs.
///
/// Public for the same reason as [`SvfDesign`](crate::SvfDesign): a voice
/// engine running this topology across SIMD lanes derives its coefficients
/// from this function rather than a copy of it. The cheap recurrence they feed is
/// four one-pole stages each `v = G*(in - s); lp = v + s;
/// s = lp + v` with `G = g / (1 + g)`, whose input is `u = tanh(u_lin)` for the
/// delay-free loop's solution `u_lin = (x - k*S) / (1 + k*G^4)` ([`ladder_step`]); the
/// bank's recurrence has the same small-signal response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderDesign {
    /// Prewarped integrator gain, `tan(pi * fc / sr)`.
    pub g: f64,
    /// Feedback gain, `4 * resonance`. At `4` the ladder self-oscillates.
    pub k: f64,
}

impl LadderDesign {
    /// Computes ladder coefficients (pure function, no state).
    ///
    /// The cutoff is clamped to `1.0..=0.998 * Nyquist`, since `tan` diverges at
    /// Nyquist itself, and the resonance to `0.0..=1.0`.
    pub fn new(
        freq: impl Into<Hz>,
        resonance: impl Into<Resonance>,
        sample_rate: impl Into<SampleRate>,
    ) -> LadderDesign {
        let (freq, resonance, sample_rate) = (freq.into(), resonance.into(), sample_rate.into());
        let fc = freq.clamp(
            Hz::new(1.0),
            sample_rate.nyquist_scaled(crate::filter::NYQUIST_MARGIN),
        );
        LadderDesign {
            g: prewarp(fc, sample_rate),
            k: 4.0 * resonance.get().clamp(0.0, 1.0),
        }
    }
}

/// The ladder's two coefficients [`ladder_step`] reads, in the arithmetic `L` it runs
/// in (the graph's float, shared by every channel, or the lane bank's `f32x8`).
/// The fields are named, so a set built by hand cannot swap them.
///
/// `g1` is the one-pole stage gain `G = g / (1 + g)` with `g = tan(π·fc/sr)`, solved
/// once with the `tan` rather than divided out every sample. The powers of `G` the
/// zero-delay loop needs are derived in the step from `g1` and `k`, so a ramp
/// interpolates these two and every blended set is a valid ladder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderCoefficients<L: SimdLanes> {
    /// The one-pole stage gain `G = g / (1 + g)`.
    pub g1: L,
    /// The loop gain, `4 * resonance`: the ladder self-oscillates from `k = 4`.
    pub k: L,
}

impl<F: Float + SimdLanes> From<&LadderDesign> for LadderCoefficients<F> {
    /// The solve's `f64` set, the stage gain `g / (1 + g)` divided out in `f64`, narrowed once
    /// to `F`: the one crossing of a ladder design.
    fn from(c: &LadderDesign) -> Self {
        Self {
            g1: F::from_design(c.g / (1.0 + c.g)),
            k: F::from_design(c.k),
        }
    }
}

impl<F: Float + SimdLanes> LadderCoefficients<F> {
    /// Linear blend toward `to` by `t` in `0..=1`: an interpolated set is only
    /// ever used for one sample and never stored.
    #[inline]
    #[must_use]
    pub(crate) fn lerp(&self, to: &Self, t: F) -> Self {
        Self {
            g1: crate::lerp(self.g1, to.g1, t),
            k: crate::lerp(self.k, to.k, t),
        }
    }
}

/// One channel's four-stage step over its stage words `s`, the resonance loop
/// closed without a delay.
///
/// Each trapezoidal one-pole (`v = G*(in - s); y = v + s; s' = y + v`) is affine in
/// its input, `y = G*in + (1 - G)*s`, so four in a chain give
/// `y4 = G^4 * u + S` with
/// `S = (1 - G) * (G^3*s0 + G^2*s1 + G*s2 + s3)`. The linear loop
/// `u = x - k*y4` then solves to `u_lin = (x - k*S) / (1 + k*G^4)`. The saturator
/// is applied to that solution, `u = tanh(u_lin)`: the standard cheap nonlinear
/// zero-delay form, exact for small signals (where `tanh x = x`, so the response
/// is the continuous ladder's bilinear image) and a saturating limit past the knee. It is
/// one `tanh` a sample against the bank's capped Newton solve with a `tanh` per stage.
///
/// `x` is the driven input (`input * drive`); `saturate` is the nonlinearity on the
/// solved loop input: the lane bank's rational approximation of `tanh` for the polysynth.
/// Written once over [`SimdLanes`].
///
/// `ty` is matched every frame; a caller with a constant type (the lane bank's
/// `LP24`) has the match folded away by the inlining, so it costs nothing there.
#[inline(always)]
pub fn ladder_step<L: SimdLanes>(
    c: &LadderCoefficients<L>,
    ty: LadderType,
    s: &mut [L; 4],
    x: L,
    saturate: impl Fn(L) -> L,
) -> L {
    let (g1, k) = (c.g1, c.k);
    let g2 = g1 * g1;
    let g4 = g2 * g2;
    let one = L::lit(1.0);
    let tail = (one - g1) * (g1 * (g1 * (g1 * s[0] + s[1]) + s[2]) + s[3]);
    let u = saturate((x - k * tail) / (one + k * g4));

    let v1 = g1 * (u - s[0]);
    let lp1 = v1 + s[0];
    s[0] = lp1 + v1;

    let v2 = g1 * (lp1 - s[1]);
    let lp2 = v2 + s[1];
    s[1] = lp2 + v2;

    let v3 = g1 * (lp2 - s[2]);
    let lp3 = v3 + s[2];
    s[2] = lp3 + v3;

    let v4 = g1 * (lp3 - s[3]);
    let lp4 = v4 + s[3];
    s[3] = lp4 + v4;

    match ty {
        LadderType::LP12 => lp2,
        LadderType::LP24 => lp4,
        LadderType::HP12 => u - lp1 - lp1 + lp2,
        LadderType::HP24 => u - lp1 * L::lit(4.0) + lp2 * L::lit(6.0) - lp3 * L::lit(4.0) + lp4,
    }
}
