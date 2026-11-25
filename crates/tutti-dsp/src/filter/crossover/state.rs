//! The state of a Linkwitz-Riley crossover tree: [`CrossoverState`], the sections' words of one
//! channel in the graph's float `F`, run with
//! [`CrossoverBlend`](super::CrossoverBlend) narrowed to it (a
//! [`CrossoverBank`](super::CrossoverBank) holds one per channel).
//!
//! Each split is a low-pass and a high-pass of two cascaded 2nd-order Butterworth sections
//! (Q = 1/sqrt(2)) at the crossover frequency (Linkwitz and Riley, 1976). The two branches
//! are -6.02 dB and in phase at the crossover, so they sum to unity magnitude: the sum is the
//! 2nd-order Butterworth all-pass of that frequency.
//!
//! N bands are N-1 splits in a tree: split 0 divides the input into band 0 and the rest,
//! split 1 divides the rest into band 1 and the rest, and so on. A band split off before the
//! last crossover has not been through the later all-passes its siblings share, so it runs
//! through the all-pass of each later crossover. Every band then has the same phase
//! response: for a crossover with denominator `D`, `LP^2`, `HP^2` and the all-pass all have
//! phase `-2 * arg D`. The bands are phase-aligned at every frequency, so they sum without
//! cancelling, and the sum has flat magnitude.
//!
//! # Sections
//!
//! Each section is the topology-preserving (trapezoidal) state-variable filter of its
//! split, with the damping `k = sqrt 2` of a Butterworth section: from its input `x`,
//! `bp = s1 - d s1 + a2 (x - s2)` and `lp = s2 + a2 s1 + a3 (x - s2)`, then the state
//! `s <- 2 (bp, lp) - s`, with `a1 = 1 / (1 + g (g + k))`, `d = 1 - a1`, `a2 = g a1`,
//! `a3 = g a2` and `g = tan(pi fc / fs)`. Its low-pass is `lp`, its high-pass `x - k bp - lp`
//! and its all-pass `x - 2 k bp`, each the bilinear Butterworth response of `fc`. A split's two
//! cascaded low-passes and two cascaded high-passes share the state of their first stage
//! (the same input and coefficients, so the same state): per split one shared first
//! section, a second low-pass section, a second high-pass section, and the all-pass
//! section each band split off earlier runs for it.
//!
//! # Stability under a moving split
//!
//! The state is bounded for any sequence of gains `g` in `[0, G]`, a split moving every
//! sample included. Write `v = (bp, lp)`, `A = [[-k, -1], [1, 0]]`, `b = (1, 0)` and
//! `u = A v + b x`; the integrators give `v = s + g u` and `s' = s + 2 g u = v + g u`. For
//! a symmetric positive definite `P` and `V(s) = s^T P s`,
//!
//! ```text
//! V(s') - V(s) = (v + g u)^T P (v + g u) - (v - g u)^T P (v - g u) = 4 g v^T P u
//!              = -2 g v^T Q v + 4 g (P v)_1 x,      Q = -(P A + A^T P).
//! ```
//!
//! This holds at every sample for its own `g`: the state's coordinates do not depend on
//! `g`, so no coefficient change moves `V`.
//!
//! - **Zero input.** `P = I` gives `Q = diag(2k, 0)` and `|s'|^2 = |s|^2 - 4 g k bp^2`: the
//!   state's Euclidean norm never grows, whatever `g` does.
//! - **Bounded input, `|x| <= M`.** Take `P = [[1, e], [e, 1 + k e]]` and
//!   `Q = diag(2k - 2e, 2e)` with `e = 3/8`, so `Q` is positive definite. With `p = P b`,
//!   `c = p^T Q^-1 p`, `q` the least eigenvalue of `P^-1 Q`, `alpha = |A|_P`
//!   (the operator norm in `|.|_P`) and `beta = |b|_P = 1`:
//!   1. `4 (P v)_1 x <= v^T Q v + 4 c x^2` (Cauchy-Schwarz, then `4 a b <= a^2 + 4 b^2`),
//!      so `V(s') <= V(s) - g v^T Q v + 4 g c M^2`.
//!   2. `s = v - g (A v + b x)` gives `|s|_P <= (1 + g alpha) |v|_P + g beta M`, and
//!      `v^T Q v >= q |v|_P^2`, so `v^T Q v >= q ((|s|_P - g M)_+ / (1 + g alpha))^2`.
//!   3. `f(V) = V - g q ((sqrt V - g M)_+)^2 / (1 + g alpha)^2 + 4 g c M^2` is nondecreasing
//!      (its slope is at least `1 - g q / (1 + g alpha)^2 >= 0`, as `(1 + g alpha)^2 >=
//!      4 g alpha >= g q`), and `f(R^2) <= R^2` once `R >= g M + 2 (1 + g alpha) sqrt(c / q) M`.
//!      So `V <= R^2` is invariant for every `g <= G` with
//!      `R = (G + 2 (1 + G alpha) sqrt(c / q)) M`, and the state starts at 0.
//!   4. `|(I - g A) w|_P^2 = |w|_P^2 + g w^T Q w + g^2 |A w|_P^2 >= |w|_P^2`, so
//!      `|v|_P = |(I - g A)^-1 (s + g b x)|_P <= |s|_P + g M`; the taps are within
//!      `|v|_2 <= |v|_P / sqrt(lambda)` (`lambda` the least eigenvalue of `P`), and the
//!      high-pass and all-pass within `M + 2 k |v|_2`.
//!
//!   At `G = tan(0.499 pi)` (`SOLVE_CEILING`): `|s|_2 <= 1.7e3 M` and a tap `<= 5.8e3 M`,
//!   per section and for every sequence of `g`. They grow with `G` because a section tuned
//!   near Nyquist has a pole near -1; a numerical search over gains and inputs of `+-M`
//!   finds `|s|_2` up to `1.1 G M`. A band of a tree is at most `2 * splits` sections in
//!   series, each fed by the one before, so its bound is that many factors of the tap's.
//!
//! The filters are IIR, so the response is **not linear-phase**: the shared phase rotates
//! with frequency (the group delay is not constant), and the sum is an all-pass, not a
//! delayed copy of the input. They add no latency.

use tutti_buffers::Float;

use super::bands::{MAX_BANDS, MAX_SPLITS};
use super::blend::{Gains, Narrowed};
use crate::SimdLanes;
use crate::filter::svf_taps;

/// A section's two state words, `[s1, s2]`: the integrators' trapezoidal memories.
#[derive(Clone, Copy, Debug)]
struct Section<F>([F; 2]);

impl<F: Float> Default for Section<F> {
    fn default() -> Self {
        Self([F::ZERO; 2])
    }
}

/// What a section's step yields: its input and the band-pass and low-pass states.
#[derive(Clone, Copy)]
struct Taps<F> {
    x: F,
    bp: F,
    lp: F,
    /// The section's damping and its double.
    k: F,
    two_k: F,
}

impl<F: Float> Taps<F> {
    #[inline]
    fn high_pass(self) -> F {
        self.x - self.k * self.bp - self.lp
    }

    #[inline]
    fn all_pass(self) -> F {
        self.x - self.two_k * self.bp
    }
}

impl<F: Float + SimdLanes> Section<F> {
    /// One sample at the coefficients `c`: the integrator step of
    /// [`svf_step`](crate::svf_step), with the taps a split's low-pass, high-pass and all-pass
    /// are mixed from.
    #[inline]
    fn tick(&mut self, c: &Gains<F>, x: F) -> Taps<F> {
        let [bp, lp] = svf_taps(c.a, &mut self.0, x);
        Taps {
            x,
            bp,
            lp,
            k: c.k,
            two_k: c.two_k,
        }
    }
}

/// The state of one channel of a crossover tree: the sections' words, apart from the
/// coefficients that run them ([`CrossoverBlend`](super::CrossoverBlend)).
#[derive(Clone, Copy, Debug)]
pub(crate) struct CrossoverState<F> {
    /// Per split, the first stage of its low-pass and high-pass, which share the state.
    first: [Section<F>; MAX_SPLITS],
    /// Per split, the second stage of its low-pass and of its high-pass.
    low: [Section<F>; MAX_SPLITS],
    high: [Section<F>; MAX_SPLITS],
    /// `all_pass[j][k]`: band `j`'s all-pass for the later split `k > j`.
    all_pass: [[Section<F>; MAX_SPLITS]; MAX_SPLITS],
}

impl<F: Float> Default for CrossoverState<F> {
    fn default() -> Self {
        Self {
            first: [Section::default(); MAX_SPLITS],
            low: [Section::default(); MAX_SPLITS],
            high: [Section::default(); MAX_SPLITS],
            all_pass: [[Section::default(); MAX_SPLITS]; MAX_SPLITS],
        }
    }
}

impl<F: Float + SimdLanes> CrossoverState<F> {
    /// One sample through the tree `c` describes: its bands, lowest first, in the first
    /// entries of the result (the rest are 0).
    #[inline]
    pub(super) fn tick(&mut self, c: &Narrowed<F>, x: F) -> [F; MAX_BANDS] {
        let mut out = [F::ZERO; MAX_BANDS];
        let splits = c.bands - 1;
        let mut rest = x;
        for (j, slot) in out.iter_mut().enumerate().take(splits) {
            let first = self.first[j].tick(&c.split[j], rest);
            let mut lo = self.low[j].tick(&c.split[j], first.lp).lp;
            let hi = self.high[j].tick(&c.split[j], first.high_pass());
            for k in j + 1..splits {
                lo = self.all_pass[j][k].tick(&c.split[k], lo).all_pass();
            }
            *slot = lo;
            rest = hi.high_pass();
        }
        out[splits] = rest;
        out
    }
}

#[cfg(test)]
mod tests;
