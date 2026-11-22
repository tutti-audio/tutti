//! The Wright omega function: the real solution `w` of `w + ln w = x`.
//!
//! For real `x`, `omega(x) = W0(e^x)`, the principal Lambert W of `e^x`, but it is evaluated
//! without forming `e^x` for large `x`, so it is finite wherever the result is. It turns a
//! balance of a linear term against an exponential into a closed form: `a u + b e^u = q` has
//! the solution `u = q / a - omega(q / a + ln(b / a))`.
//!
//! The method is Lawrence, Corless and Jeffrey's (2012, "Algorithm 917: Complex Double-Precision
//! Evaluation of the Wright omega Function"), cut to the real line: a piecewise initial
//! approximation, then [`FRITSCH`] steps of Fritsch's iteration, which converges at the fourth
//! order.
//!
//! # Initial approximation
//!
//! - `x >= 1`: the first terms of the large-`x` series `omega = x - ln x + ln x / x + ...`;
//!   its relative error is at most 8% at `x = 1.5` (largest on `1..=3`) and falls as `(ln x / x)^2`.
//!   At `x = 1` it is exactly 1.
//! - `x < 1`: Barry et al.'s approximation `W(t) ~ L (1 - ln(1 + L) / (2 + L))`, `L = ln(1 + t)`,
//!   at `t = e^x`, within 2% relative error for `t` in `0..=e`.
//! - `e^x <= eps / 2`: `omega = e^x (1 - e^x + ...)` is `e^x` to the float's rounding, so it is
//!   returned as it is (and an `x` too negative for `e^x` returns 0).
//!
//! # Accuracy
//!
//! A step is `w <- w (1 + e)` with `e` of Fritsch's form from the residual `r = x - w - ln w`.
//! From a relative error `d` it leaves `K d^4` with `K` of order 1, so two steps take the
//! approximation's worst 8% to about `3e-18`: below the resolution of `f64`. The result then
//! satisfies `|omega + ln omega - x| <= 4 eps max(1, |x|)` (measured 1.5 at `f64` and 1.3 at
//! `f32`): the rounding of the sum and of the logarithm, and `(1 + omega)` times the result's
//! relative error of about `eps`.

use tutti_buffers::Float;

/// The Fritsch steps after the initial approximation: two take the approximation's worst
/// relative error (8%) to below the resolution of `f64`.
const FRITSCH: usize = 2;

/// `omega(x)` in the float `F`, after Lawrence, Corless and Jeffrey (2012).
///
/// `omega(1) = 1`, `omega(0) = W(1) = 0.567143290409783873...`, `omega(x) ~ e^x` as `x`
/// goes to minus infinity and `omega(x) ~ x - ln x` as it goes to infinity. NaN gives NaN,
/// minus infinity 0 and infinity infinity. No allocation.
#[inline]
#[must_use]
pub(crate) fn wright_omega<F: Float>(x: F) -> F {
    let w = if x >= F::ONE {
        if x > F::MAX {
            return x;
        }
        let ln_x = x.ln();
        x - ln_x + ln_x / x
    } else {
        let t = x.exp();
        if t <= F::EPSILON * F::lit(0.5) {
            return t;
        }
        let l = t.ln_1p();
        l * (F::ONE - l.ln_1p() / (F::lit(2.0) + l))
    };
    let mut w = w;
    for _ in 0..FRITSCH {
        w = fritsch(x, w);
    }
    w
}

/// One step of Fritsch's iteration from `w`, a positive estimate of `omega(x)`.
#[inline]
fn fritsch<F: Float>(x: F, w: F) -> F {
    let r = x - w - w.ln();
    let w1 = F::ONE + w;
    let q = F::lit(2.0) * w1 * (w1 + F::lit(0.666_666_666_666_666_7) * r);
    let e = (r / w1) * (q - r) / (q - F::lit(2.0) * r);
    w * (F::ONE + e)
}

#[cfg(test)]
mod tests;
