//! The nonlinear zero-delay ladder: a `tanh` at each stage's input and in the loop, the
//! implicit loop equation solved per sample by [`capped_newton`](crate::newton).
//!
//! # The model
//!
//! Four stages, each a leaky integrator driven through a saturator (Huovilainen 2004,
//! "Non-linear digital implementation of the Moog ladder filter"; Zavalishin, *The Art of VA
//! Filter Design*, the ladder chapter):
//!
//! ```text
//! y1' = wc (tanh(u)  - y1),      u = x - k y4
//! yn' = wc (tanh(y(n-1)) - yn),  n = 2, 3, 4
//! ```
//!
//! The saturator sits at each integrator's input and not on its leak, so a stage is affine in
//! its input once the input is known. Small signals see `tanh v = v`: the continuous ladder
//! `1 / ((1 + s/wc)^4 + k)` of the model, with self-oscillation at `k = 4`.
//!
//! # The discretization
//!
//! Each stage is the trapezoidal rule at the prewarped gain `G = g / (1 + g)`,
//! `g = tan(pi fc / fs)`, state `s`:
//!
//! ```text
//! yn = G tanh(y(n-1)) + (1 - G) s(n),     s'(n) = 2 yn - s(n)
//! ```
//!
//! With `y0 = u`, the stage outputs are explicit functions of the loop variable `u`, and the
//! loop closes on one scalar equation
//!
//! ```text
//! F(u) = u - x + k y4(u) = 0,    F'(u) = 1 + k G^4 prod_{i=0..3} (1 - tanh^2 y_i)
//! ```
//!
//! # The cap, the seed and the residual
//!
//! `F' >= 1`, so `F` has one root and a start at `F(u0)` is within `|F(u0)|` of it. The seed is
//! the cheap form's solution, the loop solved with the saturators removed,
//! `u_lin = (x - k S) / (1 + k G^4)` with `S = (1 - G)(G^3 s1 + G^2 s2 + G s3 + s4)`;
//! [`capped_newton`](crate::newton) takes the better of it and the previous sample's solution.
//!
//! `|F''| <= M = k G^4 (4 / (3 sqrt 3)) (1 + G + G^2 + G^3)`: each factor `1 - tanh^2 y_i`
//! has derivative at most `4 / (3 sqrt 3)` in `y_i`, and `dy_i / du` is at most `G^i`. Newton's
//! recurrence `e' <= (M / 2) e^2` (`F' >= 1`) over [`NEWTON_CAP`](crate::newton) = 2 steps from
//! `e0 <= |F(start)|` leaves
//!
//! ```text
//! |F(u)| <= (1 + k G^4) (M / 2)^3 F(start)^4        (see `residual_bound`)
//! ```
//!
//! plus the rounding of the terms of `F` (`8 eps` of their sum of magnitudes).

use tutti_buffers::Float;

use super::kernel::{LadderCoefficients, LadderType};
use crate::SimdLanes;
use crate::newton::capped_newton;

/// One channel's nonlinear ladder: the four stage words and the previous sample's loop
/// solution, the warm start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LadderState<F> {
    /// The trapezoidal integrator words of the four stages.
    pub(crate) s: [F; 4],
    /// The loop variable `u` of the last sample.
    pub(crate) u: F,
}

impl<F: Float> LadderState<F> {
    /// A silent channel.
    pub(crate) fn silent() -> Self {
        Self {
            s: [F::ZERO; 4],
            u: F::ZERO,
        }
    }
}

/// The chain from the loop variable `u`: the saturated stage inputs `a0..a3`, the stage
/// outputs `y1..y4` and the stage increments `v1..v4` (`yn = s(n) + vn`, the integrator
/// moving by `vn` each half step).
struct Chain<F> {
    a: [F; 4],
    y: [F; 4],
    v: [F; 4],
}

impl<F: Float + SimdLanes> Chain<F> {
    /// The stages in the increment form `v = G (a - s)`, which keeps a small `G` exact where
    /// `G a + (1 - G) s` would round `1 - G`.
    #[inline(always)]
    fn at(g1: F, s: &[F; 4], u: F) -> Self {
        let mut a = [u.tanh(); 4];
        let mut y = [F::ZERO; 4];
        let mut v = [F::ZERO; 4];
        for n in 0..4 {
            v[n] = g1 * (a[n] - s[n]);
            y[n] = s[n] + v[n];
            if n < 3 {
                a[n + 1] = y[n].tanh();
            }
        }
        Self { a, y, v }
    }
}

/// `F(u)` and `F'(u)` of the loop equation at `u`, for the stage words `s` and the driven
/// input `x`.
#[inline(always)]
pub(crate) fn loop_residual<F: Float + SimdLanes>(
    c: &LadderCoefficients<F>,
    s: &[F; 4],
    x: F,
    u: F,
) -> (F, F) {
    let chain = Chain::at(c.g1, s, u);
    let one = F::ONE;
    let g2 = c.g1 * c.g1;
    let mut gain = g2 * g2;
    for a in chain.a {
        gain *= one - a * a;
    }
    (u - x + c.k * chain.y[3], one + c.k * gain)
}

/// The loop solved with the saturators removed: the closed form every sample starts from.
#[inline(always)]
pub(crate) fn linear_seed<F: Float + SimdLanes>(c: &LadderCoefficients<F>, s: &[F; 4], x: F) -> F {
    let g1 = c.g1;
    let g2 = g1 * g1;
    let tail = (F::ONE - g1) * (g1 * (g1 * (g1 * s[0] + s[1]) + s[2]) + s[3]);
    (x - c.k * tail) / (F::ONE + c.k * g2 * g2)
}

/// The loop variable of the sample: [`capped_newton`] on [`loop_residual`] from
/// [`linear_seed`], warm-started at `warm`.
#[inline(always)]
pub(crate) fn solve_loop<F: Float + SimdLanes>(
    c: &LadderCoefficients<F>,
    s: &[F; 4],
    x: F,
    warm: F,
) -> F {
    capped_newton(|u| loop_residual(c, s, x, u), linear_seed(c, s, x), warm)
}

/// One sample: the loop solved, the stages advanced, the tap `ty` returned. `x` is the driven
/// input (`input * drive`).
#[inline(always)]
pub(crate) fn nonlinear_ladder_step<F: Float + SimdLanes>(
    c: &LadderCoefficients<F>,
    ty: LadderType,
    state: &mut LadderState<F>,
    x: F,
) -> F {
    let u = solve_loop(c, &state.s, x, state.u);
    let chain = Chain::at(c.g1, &state.s, u);
    for ((s, y), v) in state.s.iter_mut().zip(chain.y).zip(chain.v) {
        *s = y + v;
    }
    state.u = u;
    let two = F::ONE + F::ONE;
    let (four, six) = (two + two, two + two + two);
    let [a0, ..] = chain.a;
    let [y1, y2, y3, y4] = chain.y;
    match ty {
        LadderType::LP12 => y2,
        LadderType::LP24 => y4,
        LadderType::HP12 => a0 - y1 - y1 + y2,
        LadderType::HP24 => a0 - y1 * four + y2 * six - y3 * four + y4,
    }
}

#[cfg(test)]
mod tests;
