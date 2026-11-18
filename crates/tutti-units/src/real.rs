//! [`Real`]: the arithmetic of `f32` and `f64`.

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
/// assert_eq!(<f64 as Real>::MINUS_3_DB, core::f64::consts::FRAC_1_SQRT_2);
/// assert_eq!(<f32 as Real>::BITS, 32);
/// ```
pub trait Real:
    Copy
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
    /// The bits one value occupies: 32 or 64.
    const BITS: u32;
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
    /// The −3 dB amplitude, `1/√2 ≈ 0.7071`: the attenuation of the ITU-R
    /// BS.775 / Dolby consumer downmix for a channel folded into the front
    /// pair.
    const MINUS_3_DB: Self;

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
        $t:ident, $bits:literal,
        $abs:ident, $floor:ident, $ceil:ident, $round:ident, $roundeven:ident, $atan:ident,
        $sqrt:ident, $exp:ident, $exp2:ident,
        $ln:ident, $log10:ident, $pow:ident, $sin:ident, $cos:ident, $tan:ident,
        $tanh:ident, $atan2:ident, $fma:ident, $min:ident, $max:ident, $copysign:ident
    ) => {
        impl Real for $t {
            const BITS: u32 = $bits;
            const MAX: Self = $t::MAX;
            const MANTISSA_DIGITS: u32 = $t::MANTISSA_DIGITS;
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            const PI: Self = core::$t::consts::PI;
            const TAU: Self = core::$t::consts::TAU;
            const EPSILON: Self = $t::EPSILON;
            const INFINITY: Self = $t::INFINITY;
            const NAN: Self = $t::NAN;
            const MINUS_3_DB: Self = core::$t::consts::FRAC_1_SQRT_2;
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

real_impl!(
    f32, 32, fabsf, floorf, ceilf, roundf, roundevenf, atanf, sqrtf, expf, exp2f, logf, log10f,
    powf, sinf, cosf, tanf, tanhf, atan2f, fmaf, fminf, fmaxf, copysignf
);
real_impl!(
    f64, 64, fabs, floor, ceil, round, roundeven, atan, sqrt, exp, exp2, log, log10, pow, sin, cos,
    tan, tanh, atan2, fma, fmin, fmax, copysign
);

#[cfg(test)]
mod tests {
    use super::Real;

    #[test]
    fn bits_are_the_ieee_widths() {
        assert_eq!(<f32 as Real>::BITS, 32);
        assert_eq!(<f64 as Real>::BITS, 64);
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
