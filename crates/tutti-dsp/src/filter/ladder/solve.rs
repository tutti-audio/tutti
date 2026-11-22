//! The nonlinear zero-delay ladder: a `tanh` at each stage's input and in the loop, the
//! implicit loop equation solved per sample by a capped Newton iteration
//! ([`NEWTON_CAP`](crate::newton) steps, no tolerance test).
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
//! the previous sample's solution is no better a start: over the residual test's range the
//! worst residual after the cap is 2.8e-7 from the seed (its worst start has `|F|` 1.7) and
//! 6e-3 from the previous solution (worst start 17), and the better of the two gives the
//! seed's figure. The solve starts from the seed alone, so it evaluates the chain less per
//! sample and keeps no warm state.
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

use super::kernel::{LadderCoefficients, LadderType};
use crate::SimdLanes;
use crate::newton::{NEWTON_CAP, newton_steps};

/// The chain from the loop variable `u`: the saturated stage inputs `a0..a3`, the stage
/// outputs `y1..y4` and the stage increments `v1..v4` (`yn = s(n) + vn`, the integrator
/// moving by `vn` each half step).
struct Chain<F> {
    a: [F; 4],
    y: [F; 4],
    v: [F; 4],
}

impl<F: SimdLanes> Chain<F> {
    /// The stages in the increment form `v = G (a - s)`, which keeps a small `G` exact where
    /// `G a + (1 - G) s` would round `1 - G`.
    #[inline(always)]
    fn at(g1: F, s: &[F; 4], u: F, tanh: impl Fn(F) -> F) -> Self {
        let mut a = [tanh(u); 4];
        let mut y = [F::lit(0.0); 4];
        let mut v = [F::lit(0.0); 4];
        for n in 0..4 {
            v[n] = g1 * (a[n] - s[n]);
            y[n] = s[n] + v[n];
            if n < 3 {
                a[n + 1] = tanh(y[n]);
            }
        }
        Self { a, y, v }
    }
}

/// `F(u)` and `F'(u)` of the loop equation at `u`, for the stage words `s` and the driven
/// input `x`.
#[inline(always)]
pub(crate) fn loop_residual<F: SimdLanes>(
    c: &LadderCoefficients<F>,
    s: &[F; 4],
    x: F,
    u: F,
    tanh: impl Fn(F) -> F,
) -> (F, F) {
    let chain = Chain::at(c.g1, s, u, tanh);
    let one = F::lit(1.0);
    let g2 = c.g1 * c.g1;
    let mut gain = g2 * g2;
    for a in chain.a {
        gain = gain * (one - a * a);
    }
    (u - x + c.k * chain.y[3], one + c.k * gain)
}

/// The loop solved with the saturators removed: the closed form every sample starts from.
#[inline(always)]
pub(crate) fn linear_seed<F: SimdLanes>(c: &LadderCoefficients<F>, s: &[F; 4], x: F) -> F {
    let g1 = c.g1;
    let g2 = g1 * g1;
    let one = F::lit(1.0);
    let tail = (one - g1) * (g1 * (g1 * (g1 * s[0] + s[1]) + s[2]) + s[3]);
    (x - c.k * tail) / (one + c.k * g2 * g2)
}

/// The loop variable of the sample: [`NEWTON_CAP`] Newton steps on [`loop_residual`] from
/// [`linear_seed`], with no tolerance test (the cost per sample is fixed, the output a
/// function of the inputs alone).
#[inline(always)]
pub(crate) fn solve_loop<F: SimdLanes>(
    c: &LadderCoefficients<F>,
    s: &[F; 4],
    x: F,
    tanh: impl Fn(F) -> F + Copy,
) -> F {
    newton_steps::<F, NEWTON_CAP>(|u| loop_residual(c, s, x, u, tanh), linear_seed(c, s, x))
}

/// One sample of the per-stage model ([`LadderSaturation::PerStage`](super::LadderSaturation)):
/// the loop solved, the stages advanced, the tap `ty` returned. `x` is the driven input
/// (`input * drive`).
///
/// Written once over [`SimdLanes`], scalar or SIMD lanes: the solve is a fixed count of Newton
/// steps with no tolerance test and no branch. `tanh` is the saturator, at every stage and in
/// the loop, and the loop's slope is read from it as `1 - tanh^2`. A scalar caller passes the
/// float's own `tanh`. A lane caller may pass an approximation: the residual after the cap is
/// then the bound of the module's derivation for the *approximated* equation (its `F` built
/// from the approximation), and the slope `1 - a^2` is the Newton slope of the true `tanh`
/// taken at the approximated value, within the approximation's own error of the
/// slope of that equation, which a Newton step tolerates as a quasi-Newton step.
#[inline(always)]
pub fn nonlinear_ladder_step<F: SimdLanes>(
    c: &LadderCoefficients<F>,
    ty: LadderType,
    s: &mut [F; 4],
    x: F,
    tanh: impl Fn(F) -> F + Copy,
) -> F {
    let u = solve_loop(c, s, x, tanh);
    let chain = Chain::at(c.g1, s, u, tanh);
    for ((s, y), v) in s.iter_mut().zip(chain.y).zip(chain.v) {
        *s = y + v;
    }
    let (four, six) = (F::lit(4.0), F::lit(6.0));
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
