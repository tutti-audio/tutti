//! The antiderivative forms of the curves: each kind's unit curve `g` and the two orders of the
//! quotient of its antiderivatives that replaces the curve at one sample.
//!
//! A quotient `(F(a) - F(b)) / (a - b)` of two antiderivatives cancels when the ends are close,
//! so each curve's quotient is written as a difference that does not: the mean of `g` over the
//! segment from `b` to `a` is formed from the ends' differences (`a - b`, `|a| - |b|`,
//! `clamp(a) - clamp(b)`, `ln_1p` of a ratio of differences), each exact or to the relative
//! precision of the float, so the result is within a few `epsilon` of the closed form at any
//! gap and no guard is needed. What remains is the curves with no such form, which take a
//! [`Guard`].

use tutti_buffers::Float;

use super::{Antialiasing, AntialiasingError, ShapeKind, fold_phase, smooth9, triangle_fold};

/// How close two inputs are before a quotient whose differences cannot be conditioned is
/// replaced by its limit, relative to the larger of 1 and the two inputs' magnitudes (in the
/// units of the curve's own input), for the float `F`.
///
/// Such a quotient's rounding error is about `3 epsilon` over the distance of its ends `d`, and
/// the limit it is replaced by, the curve at the mean of the nodes, is its Taylor form, off by
/// `d^2 / 12` for a curve of second derivative 2. The guard is where the two meet,
/// `(36 epsilon)^(1/3)`: 0.016 in `f32`, 2.0e-5 in `f64`, and the error there `epsilon^(2/3)` up
/// to a factor of 3.
#[derive(Clone, Copy, Debug)]
pub(super) struct Guard<F: Float>(F);

impl<F: Float> Guard<F> {
    /// The guard of the float `F`, solved from its epsilon in `f64`.
    pub(super) fn new() -> Self {
        let epsilon: f64 = F::EPSILON.into();
        Self(F::from_design(libm::cbrt(36.0 * epsilon)))
    }

    /// Whether `p` and `q` are within the guard of each other, relative to the larger of 1
    /// and their magnitudes: the antiderivatives grow with the input, so the quotient's
    /// cancellation does too, and the threshold with it.
    #[inline(always)]
    fn close(self, p: F, q: F) -> bool {
        (p - q).abs() < self.0 * p.abs().max(q.abs()).max(F::ONE)
    }
}

/// The second divided difference of `t^3` for `t > 0` and `0` below (`(t)+^3`) over three nodes:
/// the quantity the second antiderivative of the hard clip is built from. Every form is a sum
/// or quotient of terms of one sign, so it does not cancel.
#[inline(always)]
fn truncated_cube_dd2<F: Float>(x: F, y: F, z: F) -> F {
    // Sorted: `s0 <= s1 <= s2`.
    let (s0, s1) = (x.min(y), x.max(y));
    let (s0, s2) = (s0.min(z), s0.max(z));
    let (s1, s2) = (s1.min(s2), s1.max(s2));
    if s2 <= F::ZERO {
        F::ZERO
    } else if s0 > F::ZERO {
        s0 + s1 + s2
    } else if s1 <= F::ZERO {
        // Only `s2` is on the cube: the one Lagrange term.
        s2 * (s2 / (s2 - s0)) * (s2 / (s2 - s1))
    } else {
        // `s1` and `s2` are: `f[s1, s2] = s2^2 + s1 s2 + s1^2` and `f[s0, s1] = s1^3 / (s1 - s0)`.
        ((s1 * s1 + s1 * s2 + s2 * s2) - s1 * s1 * (s1 / (s1 - s0))) / (s2 - s0)
    }
}

/// `ln(top / bottom)` for positive `top` and `bottom` whose difference `delta` is given: `ln_1p`
/// of the relative difference where it is small, so a ratio near 1 keeps its precision, and the
/// logarithm of the quotient where it is not, so a ratio far from 1 does not round to 0.
#[inline(always)]
fn ln_ratio<F: Float>(delta: F, top: F, bottom: F) -> F {
    let t = delta / bottom;
    if t.abs() < F::lit(0.5) {
        t.ln_1p()
    } else {
        (top / bottom).ln()
    }
}

/// `F1` of `Softsign`'s unit curve, `|u| - ln(1 + |u|)`.
#[inline(always)]
fn softsign_f1<F: Float>(u: F) -> F {
    u.abs() - u.abs().ln_1p()
}

/// `F2` of `Softsign`'s unit curve for `w >= 0` (it is odd): `w^2 / 2 + w - (1 + w) ln(1 + w)`.
#[inline(always)]
fn softsign_f2_magnitude<F: Float>(w: F) -> F {
    F::lit(0.5) * w * w + w - (F::ONE + w) * w.ln_1p()
}

/// The mean of `Softsign`'s `F1` over the segment from `q` to `p`: `(F2(p) - F2(q)) / (p - q)`.
///
/// Where the ends have one sign, with `P = |p|` and `Q = |q|`, it is
/// `(P + Q) / 2 + 1 - ln(1 + Q) - (1 + P) ln_1p((P - Q) / (1 + Q)) / (P - Q)`, the logarithms
/// taken as a ratio of differences; where they straddle zero the two `F2` add, `(F2(P) + F2(Q)) / (P + Q)`, which has no
/// subtraction.
#[inline(always)]
fn softsign_mean_f1<F: Float>(p: F, q: F) -> F {
    let (big_p, big_q) = (p.abs(), q.abs());
    if p * q < F::ZERO {
        (softsign_f2_magnitude(big_p) + softsign_f2_magnitude(big_q)) / (big_p + big_q)
    } else if big_p == big_q {
        softsign_f1(p)
    } else {
        let one = F::ONE;
        F::lit(0.5) * (big_p + big_q) + one
            - big_q.ln_1p()
            - (one + big_p) * ln_ratio(big_p - big_q, one + big_p, one + big_q) / (big_p - big_q)
    }
}

/// `F1` of the triangle fold's unit curve: the integral of the fold from 0, periodic (the fold
/// is odd and has a mean of 0 over its period of 4) and even, `a^2 / 2` for `a = |w| <= 1` and
/// `1 - (2 - a)^2 / 2` from 1 to 2, `w` the argument reduced to `-2..=2`.
#[inline(always)]
fn triangle_fold_f1<F: Float>(u: F) -> F {
    let a = fold_phase(u).abs();
    if a <= F::ONE {
        F::lit(0.5) * a * a
    } else {
        let r = F::lit(2.0) - a;
        F::ONE - F::lit(0.5) * r * r
    }
}

/// `F1` of `SoftCrush`'s unit curve: the integral of `n + smooth9(u - n)`.
#[inline(always)]
fn soft_crush_f1<F: Float>(u: F) -> F {
    let half = F::lit(0.5);
    let n = u.floor();
    let f = u - n;
    half * n * n + n * f + smooth9_integral(f)
}

impl ShapeKind {
    /// `order`, when the kind has it.
    ///
    /// # Errors
    ///
    /// [`AntialiasingError`] when `order` is above [`max_antialiasing`](Self::max_antialiasing).
    pub fn antialiasing(self, order: Antialiasing) -> Result<Antialiasing, AntialiasingError> {
        let max = self.max_antialiasing();
        if order > max {
            return Err(AntialiasingError {
                kind: self,
                order,
                max,
            });
        }
        Ok(order)
    }

    /// The highest order of antiderivative antialiasing the kind has: the order its
    /// antiderivatives are elementary to.
    ///
    /// `Softsign` and `HardClip` have an elementary second antiderivative; the others stop at
    /// the first: `Tanh` (`ln cosh`), `Atan`, `Crush` and `SoftCrush` (a polynomial per step).
    #[must_use]
    pub fn max_antialiasing(self) -> Antialiasing {
        match self {
            Self::Softsign | Self::HardClip => Antialiasing::SecondOrder,
            _ => Antialiasing::FirstOrder,
        }
    }

    /// The factor from `x * hardness` to the argument of the unit curve.
    #[inline(always)]
    fn pre<F: Float>(self) -> F {
        match self {
            Self::Atan => F::PI * F::lit(0.5),
            _ => F::ONE,
        }
    }

    /// The factor from the unit curve to the output.
    #[inline(always)]
    fn post<F: Float>(self, hardness: F) -> F {
        match self {
            Self::Atan => F::lit(2.0) / F::PI,
            Self::Crush | Self::SoftCrush => F::ONE / hardness,
            _ => F::ONE,
        }
    }

    /// The unit curve.
    #[inline(always)]
    fn g<F: Float>(self, u: F) -> F {
        match self {
            Self::Tanh => u.tanh(),
            Self::Atan => u.atan(),
            Self::Softsign => u / (F::ONE + u.abs()),
            Self::HardClip => u.clamp(-F::ONE, F::ONE),
            Self::TriangleFold => triangle_fold(u),
            Self::Crush => u.round(),
            Self::SoftCrush => {
                let n = u.floor();
                n + smooth9(u - n)
            }
        }
    }

    /// First-order ADAA: the mean of the curve over the segment from `b` to `a`, the quotient
    /// `(F1(a) - F1(b)) / (a - b)` of the curve's antiderivative formed from differences that do
    /// not cancel, or `g(a)` where the ends are the same; `SoftCrush` has no such form and
    /// takes `g` at the midpoint where the ends are within the guard.
    #[inline(always)]
    fn first<F: Float>(self, guard: Guard<F>, a: F, b: F) -> F {
        if a == b {
            return self.g(a);
        }
        let (half, one) = (F::lit(0.5), F::ONE);
        let gap = a - b;
        match self {
            Self::Tanh => {
                // `ln cosh a - ln cosh b = ln(cosh a / cosh b)` and `cosh a / cosh b = cosh d +
                // tanh(b) sinh d` for `d = a - b`; a gap of one or more takes `|u| - ln 2 +
                // ln(1 + e^(-2|u|))`, whose terms do not cancel there.
                let num = if gap.abs() < one {
                    let sinh = |x: F| (x.exp_m1() - (-x).exp_m1()) * half;
                    let h = sinh(half * gap);
                    (F::lit(2.0) * h * h + b.tanh() * sinh(gap)).ln_1p()
                } else {
                    let tail = |u: F| (-F::lit(2.0) * u.abs()).exp().ln_1p();
                    (a.abs() - b.abs()) + tail(a) - tail(b)
                };
                num / gap
            }
            Self::Atan => {
                // `F1(u) = u atan u - ln(1 + u^2) / 2`; `atan a - atan b = atan2(a - b, 1 + ab)`
                // and `ln((1 + a^2) / (1 + b^2)) = ln_1p((a - b)(a + b) / (1 + b^2))`.
                a.atan() + b * gap.atan2(one + a * b) / gap
                    - half * ln_ratio(gap * (a + b), one + a * a, one + b * b) / gap
            }
            Self::Softsign => {
                // `F1` is even, so with `d = |a| - |b|` the difference is
                // `d - ln((1 + |a|) / (1 + |b|))`.
                let d = a.abs() - b.abs();
                (d - ln_ratio(d, one + a.abs(), one + b.abs())) / gap
            }
            Self::HardClip => {
                // `F1(u) = c u - c^2 / 2` with `c = clamp(u)`: the difference is
                // `(ca^2 - cb^2) / 2 + ca (a - ca) - cb (b - cb)`.
                let (ca, cb) = (a.clamp(-one, one), b.clamp(-one, one));
                (half * (ca - cb) * (ca + cb) + ca * (a - ca) - cb * (b - cb)) / gap
            }
            Self::TriangleFold => {
                // The fold is linear between its kinks, at the odd integers. A segment under
                // 1 long holds at most one: the mean is the two linear pieces' (the sum of
                // their end values times their lengths over the segment's, every term of
                // order 1, so no cancellation grows), or the curve at the midpoint where it
                // holds none. A longer segment has the antiderivative's difference, over a
                // gap of at least 1.
                if gap.abs() < one {
                    let (lo, hi) = if a > b { (b, a) } else { (a, b) };
                    let kink = F::lit(2.0) * ((lo + one) * half).floor() + one;
                    if kink < hi {
                        let at = self.g(kink);
                        ((kink - lo) * (self.g(lo) + at) + (hi - kink) * (at + self.g(hi))) * half
                            / (hi - lo)
                    } else {
                        self.g(half * (a + b))
                    }
                } else {
                    (triangle_fold_f1(a) - triangle_fold_f1(b)) / gap
                }
            }
            Self::Crush => {
                // `F1(u) = k u - k^2 / 2` with `k = round(u)`: the mean is `ka + (ka - kb)(b -
                // (ka + kb) / 2) / (a - b)`, whose offset from the steps' boundary is exact.
                let (ka, kb) = (a.round(), b.round());
                ka + (ka - kb) * (b - half * (ka + kb)) / gap
            }
            Self::SoftCrush => {
                if guard.close(a, b) {
                    self.g(half * (a + b))
                } else {
                    (soft_crush_f1(a) - soft_crush_f1(b)) / gap
                }
            }
        }
    }

    /// Second-order ADAA over the samples `a`, `b`, `c` (newest first): twice the second divided
    /// difference of the curve's second antiderivative `F2`, `2 (D(a, b) - D(b, c)) / (a - c)`
    /// with `D` the mean of `F1`.
    ///
    /// The hard clip's `F2` is `(u^3 - (u - 1)+^3 + (-u - 1)+^3) / 6`, so its quotient is
    /// `(a + b + c) / 3` less and plus the truncated cube's divided differences, exact. For
    /// `Softsign`, where `a` and `c` are within the guard, with `m = (a + c) / 2` it is the limit
    /// `2 (F1(m) - D(m, b)) / (m - b)`, and where `m` and `b` are too, the curve at the mean of
    /// the three, the Taylor form to `O(d^2)`.
    #[inline(always)]
    fn second<F: Float>(self, guard: Guard<F>, a: F, b: F, c: F) -> F {
        let one = F::ONE;
        if self == Self::HardClip {
            let third = F::ONE / F::lit(3.0);
            return (a + b + c - truncated_cube_dd2(a - one, b - one, c - one)
                + truncated_cube_dd2(-a - one, -b - one, -c - one))
                * third;
        }
        let two = F::lit(2.0);
        if !guard.close(a, c) {
            return two * (softsign_mean_f1(a, b) - softsign_mean_f1(b, c)) / (a - c);
        }
        let m = F::lit(0.5) * (a + c);
        if guard.close(m, b) {
            self.g((a + b + c) / F::lit(3.0))
        } else {
            let d = m - b;
            two * (softsign_f1(m) - softsign_mean_f1(m, b)) / d
        }
    }

    /// The antialiased output at one frame: `x` the undriven input, `history` the two before
    /// it (newest first), at `hardness`. The drive scales all three, so the history stays
    /// undriven. The argument is held to `±sqrt(MAX) / 4` so a square of it is finite.
    #[inline(always)]
    fn step<F: Float>(
        self,
        order: Antialiasing,
        guard: Guard<F>,
        hardness: F,
        x: F,
        history: [F; 2],
    ) -> F {
        let k = hardness * self.pre::<F>();
        let reach = F::MAX.sqrt() / F::lit(4.0);
        let arg = |v: F| (v * k).clamp(-reach, reach);
        let [a, b, c] = [arg(x), arg(history[0]), arg(history[1])];
        let y = match order {
            Antialiasing::SecondOrder => self.second(guard, a, b, c),
            _ => self.first(guard, a, b),
        };
        y * self.post(hardness)
    }

    /// `input` through the antialiased curve of `order` at a hardness per frame, into `output`;
    /// `history` is the channel's two previous inputs, newest first, and holds the last two
    /// of this block's after. A non-finite sample is shaped as it is and enters the history
    /// as 0: it is heard at its own frame only, and the next frame (the next two at second
    /// order) reads its segment from 0.
    #[inline(always)]
    pub(super) fn adaa_each<F: Float>(
        self,
        order: Antialiasing,
        guard: Guard<F>,
        hardness: &[F],
        input: &[F],
        output: &mut [F],
        history: &mut [F; 2],
    ) {
        with_kind!(self, |K| {
            for ((o, &x), &h) in output.iter_mut().zip(input).zip(hardness) {
                *o = K.step(order, guard, h, x, *history);
                *history = [if x.is_finite() { x } else { F::ZERO }, history[0]];
            }
        })
    }
}

/// The integral of [`smooth9`] from 0 to `x`; `1/2` at 1.
#[inline(always)]
fn smooth9_integral<F: Float>(x: F) -> F {
    let x3 = x * x * x;
    ((((F::lit(7.0) * x - F::lit(35.0)) * x + F::lit(67.5)) * x - F::lit(60.0)) * x + F::lit(21.0))
        * x3
        * x3
}
