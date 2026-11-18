use tutti_units::Normalized;

use crate::{Curvature, Exponent};

/// How a piece moves from its start value to its end value: a function `g` of
/// its progress `t` in `[0, 1]`, with `g(0) = 0` and `g(1) = 1`; the value is
/// `from + (to - from) · g(t)`.
///
/// A **closed set**, so a segment stays a `Copy` value a few words long on the
/// audio thread rather than an `Arc<dyn Curve>`. Every law is monotone between
/// its endpoints, so a piece never leaves `[from, to]`.
///
/// A variant's parameter is a checked type ([`Exponent`], [`Curvature`]) that
/// refuses what the law cannot evaluate, so every `Law` is evaluable and the
/// audio thread never computes a NaN from one.
///
/// **Platform figures.** `Linear`, `Hold`, `Smootherstep` and the power laws
/// at exponents 0.5 and 1–5 evaluate with IEEE-exact operations
/// (multiplication, `sqrt`). Other exponents and [`Exp`](Self::Exp) use
/// `libm`'s pure-Rust `pow` and `expm1`, not the platform's C math library,
/// so every law gives the same bits on every target.
///
/// ```
/// use tutti_curve::Law;
/// use tutti_curve::Exponent;
/// use tutti_units::Normalized;
/// let at = |t| Normalized::of(t);
/// assert_eq!(Law::Linear.eval(at(0.25)), at(0.25));
/// assert_eq!(Law::PowerIn(Exponent::of(2.0)).eval(at(0.5)), at(0.25));
/// assert_eq!(Law::PowerOut(Exponent::of(2.0)).eval(at(0.5)), at(0.75));
/// assert_eq!(Law::Hold.eval(at(0.99)), at(0.0));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Law {
    /// `t`. The default.
    #[default]
    Linear,
    /// `0` until the end, then `1`: the start value held, and a step onto the
    /// end value on the piece's last frame.
    Hold,
    /// `t^p`: an ease-in. `p > 1` starts slow (`2` is a quadratic),
    /// `p < 1` starts fast (`0.5` is a square root).
    PowerIn(Exponent),
    /// `1 - (1 - t)^p`: the ease-out mirror of
    /// [`PowerIn`](Self::PowerIn).
    PowerOut(Exponent),
    /// [`PowerIn`](Self::PowerIn) over the first half and
    /// [`PowerOut`](Self::PowerOut) over the second, `p > 0`: an S-curve (`2`
    /// is the quadratic S-curve).
    PowerInOut(Exponent),
    /// `(e^(k·t) - 1) / (e^k - 1)`: a true exponential with curvature `k`
    /// (`|k| <= 64`, see [`Curvature`]). `k > 0` starts slow, `k < 0` starts fast;
    /// `k` near 0 is linear. `k = ln(to / from)` makes the value geometric (a
    /// frequency sweep linear in pitch).
    Exp(Curvature),
    /// `t³(6t² - 15t + 10)`: Perlin's smootherstep, the fifth-order polynomial
    /// with zero first and second derivatives at both ends, an S-curve that
    /// meets its neighbours smoothly. (Named for the theory: "smoothstep"
    /// is the third-order `3t² - 2t³`, and this is its successor.) It is
    /// the amplitude law of the graph's equal-amplitude crossfade.
    ///
    /// Evaluated in a Horner form and pinned to `[0, 1]`.
    Smootherstep,
}

impl Law {
    /// `g(t)`. Allocation-free: what a param port calls once per frame.
    ///
    /// The progress and the result are [`Normalized`]: a progress is a
    /// position on `0..=1`, so an out-of-range or NaN progress is pinned where
    /// it is built (`Normalized::clamped`), never inside the law. The result
    /// is pinned onto `0..=1` once more, which trims the rounding overshoot of
    /// the polynomial and exponential forms.
    #[inline]
    #[must_use]
    pub fn eval(self, t: Normalized) -> Normalized {
        let t = t.get();
        let g = match self {
            Self::Linear => t,
            Self::Hold => {
                if t < 1.0 {
                    0.0
                } else {
                    1.0
                }
            }
            Self::PowerIn(p) => pow(t, p.get()),
            Self::PowerOut(p) => 1.0 - pow(1.0 - t, p.get()),
            Self::PowerInOut(p) => {
                let p = p.get();
                // `2^(p-1) · t^p` is `(2t)^p / 2`, which keeps the integer
                // exponents on multiplications.
                if t < 0.5 {
                    pow(2.0 * t, p) / 2.0
                } else {
                    1.0 - pow(2.0 - 2.0 * t, p) / 2.0
                }
            }
            Self::Smootherstep => t * t * t * (t * (t * 6.0 - 15.0) + 10.0),
            Self::Exp(k) => {
                let k = k.get();
                // `expm1` is accurate down to the smallest `k`; only 0 is `0 / 0`.
                if k == 0.0 {
                    t
                } else {
                    libm::expm1(k * t) / libm::expm1(k)
                }
            }
        };
        Normalized::clamped(g)
    }
}

/// `x^p` for `x` in `[0, 1]`: the integer exponents up to 5 and the square
/// root by IEEE-exact operations, anything else through `libm::pow`.
#[inline]
fn pow(x: f64, p: f64) -> f64 {
    if p == 1.0 {
        x
    } else if p == 2.0 {
        x * x
    } else if p == 3.0 {
        x * x * x
    } else if p == 4.0 {
        let x2 = x * x;
        x2 * x2
    } else if p == 5.0 {
        let x2 = x * x;
        x2 * x2 * x
    } else if p == 0.5 {
        libm::sqrt(x)
    } else {
        libm::pow(x, p)
    }
}

#[cfg(test)]
mod tests;
