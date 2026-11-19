//! [`Real`]: the arithmetic of `f32` and `f64`.

use crate::Quantity;
use core::fmt::{Debug, Display};
use core::iter::Sum;
use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Rem, Sub, SubAssign};

/// A real number type a law or a kernel computes in: `f32` or `f64`.
///
/// Its math goes through the `libm` crate, never the standard library's float
/// methods, so a result is the same on every platform. A concrete `f32` or
/// `f64` call (`x.sqrt()`) resolves to the standard library's inherent method;
/// generic code over `R: Real`, or the qualified `<f32 as Real>::sqrt(x)`,
/// reaches `libm`.
///
/// ```
/// use tutti_units::Real;
///
/// assert_eq!(<f32 as Real>::sqrt(4.0), 2.0);
/// assert_eq!(<f64 as Real>::exp(0.0), 1.0);
/// ```
///
/// Data of the width crosses from four places: [`from_unit`](Real::from_unit) (a quantity),
/// [`from_design`](Real::from_design) (a computed figure), [`lit`](Real::lit) (a literal) and
/// [`from_count`](Real::from_count) (a count).
pub trait Real:
    sealed::Sealed
    + Copy
    + Debug
    + Display
    + PartialEq
    + PartialOrd
    + Send
    + Sync
    + 'static
    + Into<f64>
    + Sum
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Rem<Output = Self>
    + Neg<Output = Self>
    + AddAssign
    + SubAssign
    + MulAssign
    + DivAssign
{
    /// The largest finite value.
    const MAX: Self;
    /// The significand digits in base 2: 24 for `f32`, 53 for `f64`.
    const MANTISSA_DIGITS: u32;
    /// Zero.
    const ZERO: Self;
    /// One.
    const ONE: Self;
    /// Archimedes' constant, `π`.
    const PI: Self;
    /// The full turn, `2π`.
    const TAU: Self;
    /// The difference between one and the next larger value.
    const EPSILON: Self;
    /// Positive infinity.
    const INFINITY: Self;
    /// Not a number.
    const NAN: Self;

    /// A [`Quantity`] as data of this width: the only spelling of quantity to
    /// data, `F::from_unit(cutoff)`.
    ///
    /// It saturates. A finite quantity narrows to a finite value and an
    /// infinite one to the nearest, both clamped to `-Self::MAX..=Self::MAX`;
    /// a NaN stays NaN. A bare `as` would turn a finite `f64` past `f32::MAX`
    /// into an infinity, which no node latches.
    /// The cost is one compare-and-select, paid at control rate. A `Quantity` is an `f64`
    /// value, which keeps counts and positions from crossing into data this way.
    ///
    /// ```
    /// use tutti_units::{Hz, Real};
    ///
    /// assert_eq!(f32::from_unit(Hz(440.0)), 440.0f32);
    /// assert_eq!(f32::from_unit(Hz(1e300)), f32::MAX);
    /// assert_eq!(f64::from_unit(Hz(440.0)), 440.0);
    /// ```
    #[must_use]
    fn from_unit<Q: Quantity>(q: Q) -> Self;
    /// A design figure as data: a coefficient, a pole or a gain (`g`, `k`, a
    /// biquad's `a2`) that a kernel's design computes in `f64` from its
    /// quantities, crossing once inside that design function. A figure is not a
    /// [`Quantity`], so it does not cross with [`from_unit`](Self::from_unit); it
    /// saturates the same way: a value past this width's range clamps to
    /// `±MAX`, an infinity included, and NaN stays NaN.
    ///
    /// ```
    /// use tutti_units::Real;
    ///
    /// let g = (core::f64::consts::PI * 1_000.0 / 48_000.0).tan();
    /// assert_eq!(f32::from_design(g), g as f32);
    /// assert_eq!(f32::from_design(1e300), f32::MAX);
    /// assert_eq!(f64::from_design(g), g);
    /// ```
    #[must_use]
    fn from_design(x: f64) -> Self;
    /// A numeric literal in a generic kernel: `F::lit(0.5)`. Only a literal
    /// argument; any other value crosses with [`from_unit`](Self::from_unit), a
    /// computed design figure with [`from_design`](Self::from_design), or `From`.
    ///
    /// ```
    /// use tutti_units::Real;
    ///
    /// assert_eq!(f32::lit(0.1), 0.1f32);
    /// assert_eq!(f64::lit(0.1), 0.1);
    /// ```
    #[must_use]
    fn lit(x: f64) -> Self;
    /// A count as data of this width: exact up to `2^MANTISSA_DIGITS` (`2^24` at `f32`, `2^53`
    /// at `f64`) and rounded to nearest above that. The one crossing from an integer count (a
    /// frame index, a block length) to data.
    ///
    /// ```
    /// use tutti_units::Real;
    ///
    /// assert_eq!(f32::from_count(1 << 24), 16_777_216.0);
    /// assert_eq!(f64::from_count(u32::MAX), 4_294_967_295.0);
    /// ```
    #[must_use]
    fn from_count(n: u32) -> Self;
    /// The absolute value.
    #[must_use]
    fn abs(self) -> Self;
    /// The largest integer not above the value.
    #[must_use]
    fn floor(self) -> Self;
    /// The smallest integer not below the value.
    #[must_use]
    fn ceil(self) -> Self;
    /// The nearest integer, a tie away from zero; a value that is already whole, NaN and the
    /// infinities are returned as they are, and a zero keeps its sign.
    #[must_use]
    fn round(self) -> Self;
    /// The nearest integer, a tie to the even one (IEEE 754 `roundToIntegralTiesToEven`); a
    /// value that is already whole, NaN and the infinities are returned as they are, and a
    /// result of zero keeps the value's sign.
    #[must_use]
    fn round_ties_even(self) -> Self;
    /// The square root.
    #[must_use]
    fn sqrt(self) -> Self;
    /// `e` to the value.
    #[must_use]
    fn exp(self) -> Self;
    /// `e` to the value, minus one: accurate near zero, where `exp(x) - 1`
    /// cancels.
    #[must_use]
    fn exp_m1(self) -> Self;
    /// The natural logarithm of one plus the value: accurate near zero, where
    /// `ln(1 + x)` cancels.
    #[must_use]
    fn ln_1p(self) -> Self;
    /// `2` to the value.
    #[must_use]
    fn exp2(self) -> Self;
    /// The natural logarithm.
    #[must_use]
    fn ln(self) -> Self;
    /// The base-10 logarithm.
    #[must_use]
    fn log10(self) -> Self;
    /// The value to the power `y`.
    #[must_use]
    fn powf(self, y: Self) -> Self;
    /// The sine, in radians.
    #[must_use]
    fn sin(self) -> Self;
    /// The cosine, in radians.
    #[must_use]
    fn cos(self) -> Self;
    /// The tangent, in radians.
    #[must_use]
    fn tan(self) -> Self;
    /// The hyperbolic tangent.
    #[must_use]
    fn tanh(self) -> Self;
    /// The arctangent, in radians, within `-π/2..=π/2`.
    #[must_use]
    fn atan(self) -> Self;
    /// The angle of the point `(other, self)` from the positive x axis, in
    /// radians: the four-quadrant arctangent of `self / other`.
    #[must_use]
    fn atan2(self, other: Self) -> Self;
    /// `self * a + b` with one rounding.
    ///
    /// This is libm's software `fma`, slow on x86: not for a per-sample loop
    /// unless the single rounding is needed.
    #[must_use]
    fn mul_add(self, a: Self, b: Self) -> Self;
    /// The smaller of the two; a NaN loses to a number.
    #[must_use]
    fn min(self, other: Self) -> Self;
    /// The larger of the two; a NaN loses to a number.
    #[must_use]
    fn max(self, other: Self) -> Self;
    /// The value limited to `min..=max`; a NaN is returned unchanged.
    ///
    /// The bounds are not checked: with `min > max` the result is `min` for a
    /// value below `min` and `max` otherwise. It never panics.
    #[must_use]
    fn clamp(self, min: Self, max: Self) -> Self;
    /// `1.0` for a positive value or `+0.0`, `-1.0` for a negative one or
    /// `-0.0`, and NaN for NaN.
    #[must_use]
    fn signum(self) -> Self;
    /// Whether the value is NaN.
    #[must_use]
    fn is_nan(self) -> bool;
    /// Whether the value is neither infinite nor NaN.
    #[must_use]
    fn is_finite(self) -> bool;
}

macro_rules! real_impl {
    (
        $t:ident,
        $abs:ident, $floor:ident, $ceil:ident, $round:ident, $roundeven:ident, $atan:ident,
        $sqrt:ident, $exp:ident, $expm1:ident, $log1p:ident, $exp2:ident,
        $ln:ident, $log10:ident, $pow:ident, $sin:ident, $cos:ident, $tan:ident,
        $tanh:ident, $atan2:ident, $fma:ident, $min:ident, $max:ident, $copysign:ident
    ) => {
        impl Real for $t {
            const MAX: Self = $t::MAX;
            const MANTISSA_DIGITS: u32 = $t::MANTISSA_DIGITS;
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            const PI: Self = core::$t::consts::PI;
            const TAU: Self = core::$t::consts::TAU;
            const EPSILON: Self = $t::EPSILON;
            const INFINITY: Self = $t::INFINITY;
            const NAN: Self = $t::NAN;
            #[inline]
            fn from_unit<Q: Quantity>(q: Q) -> Self {
                Self::from_design(q.value())
            }
            #[inline]
            fn from_design(x: f64) -> Self {
                if x.is_nan() {
                    Self::NAN
                } else {
                    let max = f64::from($t::MAX);
                    x.clamp(-max, max) as $t
                }
            }
            #[inline]
            fn lit(x: f64) -> Self {
                x as $t
            }
            #[inline]
            fn from_count(n: u32) -> Self {
                n as $t
            }
            #[inline]
            fn exp_m1(self) -> Self {
                libm::$expm1(self)
            }
            #[inline]
            fn ln_1p(self) -> Self {
                libm::$log1p(self)
            }
            #[inline]
            fn abs(self) -> Self {
                libm::$abs(self)
            }
            #[inline]
            fn floor(self) -> Self {
                libm::$floor(self)
            }
            #[inline]
            fn ceil(self) -> Self {
                libm::$ceil(self)
            }
            #[inline]
            fn round(self) -> Self {
                libm::$round(self)
            }
            #[inline]
            fn round_ties_even(self) -> Self {
                libm::$roundeven(self)
            }
            #[inline]
            fn atan(self) -> Self {
                libm::$atan(self)
            }
            #[inline]
            fn sqrt(self) -> Self {
                libm::$sqrt(self)
            }
            #[inline]
            fn exp(self) -> Self {
                libm::$exp(self)
            }
            #[inline]
            fn exp2(self) -> Self {
                libm::$exp2(self)
            }
            #[inline]
            fn ln(self) -> Self {
                libm::$ln(self)
            }
            #[inline]
            fn log10(self) -> Self {
                libm::$log10(self)
            }
            #[inline]
            fn powf(self, y: Self) -> Self {
                libm::$pow(self, y)
            }
            #[inline]
            fn sin(self) -> Self {
                libm::$sin(self)
            }
            #[inline]
            fn cos(self) -> Self {
                libm::$cos(self)
            }
            #[inline]
            fn tan(self) -> Self {
                libm::$tan(self)
            }
            #[inline]
            fn tanh(self) -> Self {
                libm::$tanh(self)
            }
            #[inline]
            fn atan2(self, other: Self) -> Self {
                libm::$atan2(self, other)
            }
            #[inline]
            fn mul_add(self, a: Self, b: Self) -> Self {
                libm::$fma(self, a, b)
            }
            #[inline]
            fn min(self, other: Self) -> Self {
                libm::$min(self, other)
            }
            #[inline]
            fn max(self, other: Self) -> Self {
                libm::$max(self, other)
            }
            #[inline]
            fn clamp(self, min: Self, max: Self) -> Self {
                if self < min {
                    min
                } else if self > max {
                    max
                } else {
                    self
                }
            }
            #[inline]
            fn signum(self) -> Self {
                if <$t>::is_nan(self) {
                    self
                } else {
                    libm::$copysign(1.0, self)
                }
            }
            #[inline]
            fn is_nan(self) -> bool {
                <$t>::is_nan(self)
            }
            #[inline]
            fn is_finite(self) -> bool {
                libm::$abs(self) < $t::INFINITY
            }
        }
    };
}

mod sealed {
    /// Closes [`Real`](super::Real) to `f32` and `f64`.
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

real_impl!(
    f32, fabsf, floorf, ceilf, roundf, roundevenf, atanf, sqrtf, expf, expm1f, log1pf, exp2f, logf,
    log10f, powf, sinf, cosf, tanf, tanhf, atan2f, fmaf, fminf, fmaxf, copysignf
);
real_impl!(
    f64, fabs, floor, ceil, round, roundeven, atan, sqrt, exp, expm1, log1p, exp2, log, log10, pow,
    sin, cos, tan, tanh, atan2, fma, fmin, fmax, copysign
);

#[cfg(test)]
mod tests {
    use super::Real;
    use crate::Hz;

    #[test]
    fn from_unit_saturates_a_finite_quantity_to_a_finite_f32() {
        assert_eq!(f32::from_unit(Hz(1e300)), f32::MAX);
        assert_eq!(f32::from_unit(Hz(-1e300)), -f32::MAX);
        assert!(f32::from_unit(Hz(f64::NAN)).is_nan());
        assert_eq!(f32::from_unit(Hz(440.0)), 440.0f32);
    }

    #[test]
    fn from_unit_at_f64_is_the_raw_value_when_finite() {
        for x in [0.1, 1e300, -1e-300, 48_000.0] {
            assert_eq!(f64::from_unit(Hz(x)), x);
        }
    }

    #[test]
    fn from_unit_saturates_an_infinite_quantity_at_both_widths() {
        assert_eq!(f32::from_unit(Hz(f64::INFINITY)), f32::MAX);
        assert_eq!(f32::from_unit(Hz(f64::NEG_INFINITY)), -f32::MAX);
        assert_eq!(f64::from_unit(Hz(f64::INFINITY)), f64::MAX);
        assert_eq!(f64::from_unit(Hz(f64::NEG_INFINITY)), -f64::MAX);
        assert!(f64::from_unit(Hz(f64::NAN)).is_nan());
    }

    #[test]
    fn from_design_saturates_a_figure_and_keeps_nan_at_both_widths() {
        assert_eq!(f32::from_design(1e300), f32::MAX);
        assert_eq!(f32::from_design(-1e300), -f32::MAX);
        assert_eq!(f32::from_design(f64::INFINITY), f32::MAX);
        assert_eq!(f64::from_design(f64::NEG_INFINITY), -f64::MAX);
        assert!(f32::from_design(f64::NAN).is_nan());
        assert!(f64::from_design(f64::NAN).is_nan());
        assert_eq!(f32::from_design(0.5), 0.5_f32);
        for x in [0.1, 1e300, -1e-300, 0.999_999_999] {
            assert_eq!(f64::from_design(x), x);
        }
    }

    #[test]
    fn a_ranged_f64_and_a_unit_are_both_quantities() {
        use crate::{Normalized, Quantity};
        let n = Normalized::of(0.25);
        assert_eq!(n.value(), n.get());
        assert_eq!(Hz(440.0).value(), 440.0);
        assert_eq!(f32::from_unit(n), 0.25_f32);
    }

    #[test]
    fn lit_is_the_literal_at_the_width() {
        assert_eq!(f32::lit(0.1), 0.1f32);
        assert_eq!(f64::lit(0.1), 0.1);
    }

    #[test]
    fn a_count_is_exact_to_the_mantissa_and_rounds_to_nearest_above() {
        assert_eq!(f32::from_count(0), 0.0);
        assert_eq!(f64::from_count(0), 0.0);
        assert_eq!(f32::from_count(1 << 24), 16_777_216.0);
        assert_eq!(f64::from_count(1 << 24), 16_777_216.0);
        assert_eq!(f32::from_count((1 << 24) + 1), 16_777_216.0, "ties to even");
        assert_eq!(f64::from_count((1 << 24) + 1), 16_777_217.0);
        assert_eq!(f64::from_count(u32::MAX), 4_294_967_295.0);
    }

    #[test]
    fn exp_m1_is_zero_at_zero_and_exact_near_zero() {
        assert_eq!(<f32 as Real>::exp_m1(0.0), 0.0);
        assert_eq!(<f64 as Real>::exp_m1(0.0), 0.0);
        // e^x - 1 = x + x^2/2 + ..., so at 1e-10 the result is x + 5e-21 to within
        // an ulp; `exp(x) - 1` is off by ~1e-16, a million ulps of the result.
        let x = 1e-10_f64;
        let exact = x + x * x / 2.0;
        assert!((<f64 as Real>::exp_m1(x) - exact).abs() <= exact * f64::EPSILON);
        assert!((<f64 as Real>::exp(x) - 1.0 - exact).abs() > exact * 1e-8);
        let y = 1e-10_f32;
        assert!((<f32 as Real>::exp_m1(y) - y).abs() <= y * f32::EPSILON);
    }

    #[test]
    fn ln_1p_is_zero_at_zero_and_exact_near_zero_at_both_widths() {
        assert_eq!(<f32 as Real>::ln_1p(0.0), 0.0);
        assert_eq!(<f64 as Real>::ln_1p(0.0), 0.0);
        // ln(1 + x) = x - x^2/2 + ..., so at 1e-10 the result is x - 5e-21 to within an
        // ulp; `ln(1 + x)` is off by ~1e-16, about 1e10 ulps of the result.
        let x = 1e-10_f64;
        let exact = x - x * x / 2.0;
        assert!((<f64 as Real>::ln_1p(x) - exact).abs() <= exact * f64::EPSILON);
        assert!((<f64 as Real>::ln(1.0 + x) - exact).abs() > exact * 1e-8);
        // At f32, `1 + 1e-6` rounds to a multiple of 2^-23, so the naive form is off by
        // up to 6% of the result.
        let y = 1e-6_f32;
        let exact = f64::from(y) - f64::from(y) * f64::from(y) / 2.0;
        let got = f64::from(<f32 as Real>::ln_1p(y));
        assert!((got - exact).abs() <= exact * f64::from(f32::EPSILON));
        let naive = f64::from(<f32 as Real>::ln(1.0 + y));
        assert!((naive - exact).abs() > exact * 1e-2);
    }

    #[test]
    fn ln_1p_of_one_is_ln_two() {
        assert!((<f64 as Real>::ln_1p(1.0) - core::f64::consts::LN_2).abs() <= f64::EPSILON);
        assert!((<f32 as Real>::ln_1p(1.0) - core::f32::consts::LN_2).abs() <= f32::EPSILON);
    }

    #[test]
    fn mantissa_digits_are_the_ieee_significand_widths() {
        assert_eq!(<f32 as Real>::MANTISSA_DIGITS, 24);
        assert_eq!(<f64 as Real>::MANTISSA_DIGITS, 53);
    }

    fn rounds<R: Real + From<f32>>() {
        let v = |x: f32| R::from(x);
        assert_eq!(v(0.5).round_ties_even(), v(0.0));
        assert_eq!(v(1.5).round_ties_even(), v(2.0));
        assert_eq!(v(2.5).round_ties_even(), v(2.0));
        assert_eq!(v(0.5).round(), v(1.0));
        assert_eq!(v(1.5).round(), v(2.0));
        assert_eq!(v(2.5).round(), v(3.0));
        assert_eq!(v(-0.5).round(), v(-1.0));
        assert_eq!(v(-0.5).round_ties_even(), v(0.0));
        assert!(v(-0.5).round_ties_even().signum() < v(0.0));
    }

    #[test]
    fn rounding_ties_follow_ieee_754() {
        rounds::<f32>();
        rounds::<f64>();
    }

    #[test]
    fn atan_of_one_is_a_quarter_pi_and_sqrt_of_four_is_two() {
        assert!((<f64 as Real>::atan(1.0) - core::f64::consts::FRAC_PI_4).abs() < 1e-15);
        assert!((<f32 as Real>::atan(1.0) - core::f32::consts::FRAC_PI_4).abs() < 1e-7);
        assert_eq!(<f32 as Real>::sqrt(4.0), 2.0);
        assert_eq!(<f64 as Real>::sqrt(4.0), 2.0);
    }
}
