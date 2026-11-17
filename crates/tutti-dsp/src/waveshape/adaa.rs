//! The antiderivative forms of the curves: each kind's unit curve `g`, its first antiderivative
//! `F1` and, where it is elementary, its second `F2`, and the two orders of the quotient that
//! replaces the curve at one sample.

use core::f64::consts::{FRAC_PI_2, LN_2};

use tutti_buffers::{Float, Sample};

use super::{Antialiasing, ShapeKind, smooth9};

/// Below this, relative to the larger of 1 and the two inputs' magnitudes, a divided difference
/// of the input is ill-conditioned (`f64`, in the units of the curve's own input): the quotient
/// it would form is replaced by its limit. The antiderivatives grow with the input, so the
/// quotient's cancellation does too, and the threshold with it.
const EPS: f64 = 1e-4;

/// Whether the inputs `p` and `q` are too close for a quotient of their antiderivatives.
#[inline(always)]
fn close(p: f64, q: f64) -> bool {
    (p - q).abs() < EPS * p.abs().max(q.abs()).max(1.0)
}

impl ShapeKind {
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
    fn pre(self) -> f64 {
        match self {
            Self::Atan => FRAC_PI_2,
            _ => 1.0,
        }
    }

    /// The factor from the unit curve to the output.
    #[inline(always)]
    fn post(self, hardness: f64) -> f64 {
        match self {
            Self::Atan => 1.0 / FRAC_PI_2,
            Self::Crush | Self::SoftCrush => 1.0 / hardness,
            _ => 1.0,
        }
    }

    /// The unit curve.
    #[inline(always)]
    fn g(self, u: f64) -> f64 {
        match self {
            Self::Tanh => libm::tanh(u),
            Self::Atan => libm::atan(u),
            Self::Softsign => u / (1.0 + u.abs()),
            Self::HardClip => u.clamp(-1.0, 1.0),
            Self::Crush => libm::round(u),
            Self::SoftCrush => {
                let n = libm::floor(u);
                n + smooth9(u - n)
            }
        }
    }

    /// The antiderivative of [`g`](Self::g) that is 0 at 0.
    #[inline(always)]
    fn f1(self, u: f64) -> f64 {
        match self {
            // ln cosh u = |u| + ln(1 + e^(-2|u|)) - ln 2, which never overflows.
            Self::Tanh => u.abs() + libm::log1p(libm::exp(-2.0 * u.abs())) - LN_2,
            Self::Atan => u * libm::atan(u) - 0.5 * libm::log1p(u * u),
            Self::Softsign => u.abs() - libm::log1p(u.abs()),
            Self::HardClip => {
                if u.abs() <= 1.0 {
                    0.5 * u * u
                } else {
                    u.abs() - 0.5
                }
            }
            Self::Crush => {
                let k = libm::round(u);
                k * u - 0.5 * k * k
            }
            Self::SoftCrush => {
                let n = libm::floor(u);
                let f = u - n;
                0.5 * n * n + n * f + smooth9_integral(f)
            }
        }
    }

    /// The antiderivative of [`f1`](Self::f1) that is 0 at 0 (odd, `F1` being even), for the kinds whose
    /// [`max_antialiasing`](Self::max_antialiasing) is the second order; the first order's
    /// kinds never ask.
    #[inline(always)]
    fn f2(self, u: f64) -> f64 {
        let a = u.abs();
        match self {
            Self::Softsign => (0.5 * a * a + a - (1.0 + a) * libm::log1p(a)).copysign(u),
            Self::HardClip => {
                let magnitude = if a <= 1.0 {
                    a * a * a / 6.0
                } else {
                    1.0 / 6.0 + 0.5 * (a * a - a)
                };
                magnitude.copysign(u)
            }
            _ => unreachable!(
                "only Softsign and HardClip have an elementary F2: `step` asks no other"
            ),
        }
    }

    /// First-order ADAA: the mean of the curve over the segment from `b` to `a`,
    /// `(F1(a) - F1(b)) / (a - b)`, or `g((a + b) / 2)` where [`close`] says `a` and `b` are.
    #[inline(always)]
    fn first(self, a: f64, b: f64) -> f64 {
        if close(a, b) {
            self.g(0.5 * (a + b))
        } else {
            (self.f1(a) - self.f1(b)) / (a - b)
        }
    }

    /// The mean of `F1` over the segment from `q` to `p`: `(F2(p) - F2(q)) / (p - q)`, or
    /// `F1((p + q) / 2)` where [`close`] says `p` and `q` are.
    #[inline(always)]
    fn mean_f1(self, p: f64, q: f64) -> f64 {
        if close(p, q) {
            self.f1(0.5 * (p + q))
        } else {
            (self.f2(p) - self.f2(q)) / (p - q)
        }
    }

    /// Second-order ADAA over the samples `a`, `b`, `c` (newest first):
    /// `2 (D(a, b) - D(b, c)) / (a - c)` with `D` [`mean_f1`](Self::mean_f1); where
    /// [`close`] says `a` and `c` are, with `m = (a + c) / 2`, the limit
    /// `2 (F1(m) - D(m, b)) / (m - b)`; and `g(m)` where it says `m` and `b` are too.
    #[inline(always)]
    fn second(self, a: f64, b: f64, c: f64) -> f64 {
        if !close(a, c) {
            return 2.0 * (self.mean_f1(a, b) - self.mean_f1(b, c)) / (a - c);
        }
        let m = 0.5 * (a + c);
        if close(m, b) {
            self.g(m)
        } else {
            let d = m - b;
            2.0 * (self.f1(m) - (self.f2(m) - self.f2(b)) / d) / d
        }
    }

    /// The antialiased output at one frame: `x` the undriven input, `history` the two before
    /// it (newest first), at `hardness`. The drive scales all three, so the history stays
    /// undriven.
    #[inline(always)]
    fn step(self, order: Antialiasing, hardness: f64, x: f64, history: [f64; 2]) -> f64 {
        let k = hardness * self.pre();
        let [a, b, c] = [x * k, history[0] * k, history[1] * k];
        let y = match order.min(self.max_antialiasing()) {
            Antialiasing::SecondOrder => self.second(a, b, c),
            _ => self.first(a, b),
        };
        y * self.post(hardness)
    }

    /// `input` through the antialiased curve of `order` at a hardness per frame, into `output`;
    /// `history` is the channel's two previous inputs, newest first, and holds the last two
    /// of this block's after. A non-finite sample is shaped as it is and enters the history
    /// as 0: it is heard at its own frame only, and the next frame (the next two at second
    /// order) reads its segment from 0.
    #[inline(always)]
    pub(super) fn adaa_each<S: Float>(
        self,
        order: Antialiasing,
        hardness: &[f32],
        input: &[S],
        output: &mut [S],
        history: &mut [f64; 2],
    ) {
        with_kind!(self, |K| {
            for ((o, x), &h) in output.iter_mut().zip(input).zip(hardness) {
                let x = x.convert_to::<f64>();
                let y = K.step(order, f64::from(h), x, *history);
                *o = y.convert_to::<S>();
                *history = [if x.is_finite() { x } else { 0.0 }, history[0]];
            }
        })
    }
}

/// The integral of [`smooth9`] from 0 to `x`; `1/2` at 1.
#[inline(always)]
fn smooth9_integral(x: f64) -> f64 {
    let x3 = x * x * x;
    ((((7.0 * x - 35.0) * x + 67.5) * x - 60.0) * x + 21.0) * x3 * x3
}
