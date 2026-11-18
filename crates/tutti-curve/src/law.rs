use tutti_units::{Normalized, Real, Unit};

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
    /// the polynomial and exponential forms. It is [`eval_in`](Self::eval_in)
    /// at `f64`.
    #[inline]
    #[must_use]
    pub fn eval(self, t: Normalized) -> Normalized {
        Normalized::clamped(self.eval_in(t.get()))
    }

    /// The law over data: a lane or an LFO sample at the graph's width.
    ///
    /// `t` is pinned onto `0..=1` (a NaN is 0) and so is the result, which
    /// trims the rounding overshoot of the polynomial and exponential forms.
    /// This is the one copy of the maths; [`eval`](Self::eval) is its `f64`
    /// instantiation, and an `f32` lane computes in `f32` with no round trip
    /// through `f64`.
    ///
    /// ```
    /// use tutti_curve::{Exponent, Law};
    ///
    /// assert_eq!(Law::PowerIn(Exponent::of(2.0)).eval_in(0.5_f32), 0.25);
    /// assert_eq!(Law::Linear.eval_in(f32::NAN), 0.0);
    /// ```
    #[inline]
    #[must_use]
    pub fn eval_in<R: Real>(self, t: R) -> R {
        let zero = R::ZERO;
        let one = R::ONE;
        let t = if t.is_nan() { zero } else { t.clamp(zero, one) };
        let g = match self {
            Self::Linear => t,
            Self::Hold => {
                if t < one {
                    zero
                } else {
                    one
                }
            }
            Self::PowerIn(p) => pow(t, param(p.get())),
            Self::PowerOut(p) => one - pow(one - t, param(p.get())),
            Self::PowerInOut(p) => {
                let p: R = param(p.get());
                let two = R::lit(2.0);
                let half = R::lit(0.5);
                // `2^(p-1) · t^p` is `(2t)^p / 2`, which keeps the integer
                // exponents on multiplications.
                if t < half {
                    pow(two * t, p) / two
                } else {
                    one - pow(two - two * t, p) / two
                }
            }
            Self::Smootherstep => t * t * t * (t * (t * R::lit(6.0) - R::lit(15.0)) + R::lit(10.0)),
            Self::Exp(k) => {
                let k: R = param(k.get());
                // `expm1` is accurate down to the smallest `k`; only 0 is `0 / 0`.
                if k == zero {
                    t
                } else {
                    (k * t).exp_m1() / k.exp_m1()
                }
            }
        };
        if g.is_nan() { zero } else { g.clamp(zero, one) }
    }
}

/// A checked law parameter (an `Exponent` or `Curvature` raw) as a quantity, so it
/// crosses into the lane's float with `R::from_unit`, saturating like any
/// quantity. `from_raw` is the identity: the value was checked when the
/// `Exponent` or `Curvature` was built.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Shape(f64);

impl Unit for Shape {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.0
    }
}

#[inline]
fn param<R: Real>(raw: f64) -> R {
    R::from_unit(Shape(raw))
}

/// `x^p` for `x` in `[0, 1]`: the integer exponents up to 5 and the square
/// root by IEEE-exact operations, anything else through `Real::powf`.
#[inline]
fn pow<R: Real>(x: R, p: R) -> R {
    if p == R::lit(1.0) {
        x
    } else if p == R::lit(2.0) {
        x * x
    } else if p == R::lit(3.0) {
        x * x * x
    } else if p == R::lit(4.0) {
        let x2 = x * x;
        x2 * x2
    } else if p == R::lit(5.0) {
        let x2 = x * x;
        x2 * x2 * x
    } else if p == R::lit(0.5) {
        x.sqrt()
    } else {
        x.powf(p)
    }
}

#[cfg(test)]
mod tests;
