//! [`Sample`] and [`Float`]: the storage formats and the arithmetic types.

use crate::I24;
use core::fmt::{Debug, Display};
use core::iter::Sum;
use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Rem, Sub, SubAssign};

mod sealed {
    /// The closed set of formats: only this crate adds one.
    pub trait Sealed {
        /// The bit width of an integer format, `None` for a float.
        const INT_BITS: Option<u32>;
        /// The value of an integer format, `None` for a float.
        fn to_int(self) -> Option<i32>;
        /// The format's value for an integer already within its range. A float
        /// format never reaches this.
        fn from_int(x: i32) -> Self;
        /// The format's value for a nominal-unit `f64`, and whether the rule
        /// clipped it: the one place a float becomes this format.
        fn from_f64_clipped(x: f64) -> (Self, bool)
        where
            Self: Sized;
    }
}

/// The storage formats a [`Sample`] can be, as a value: what an encoder, a
/// device or a file header dispatches on at run time.
///
/// [`Sample::FORMAT`] names an implementor's variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleFormat {
    /// Signed 8-bit integer.
    I8,
    /// Signed 16-bit integer.
    I16,
    /// Signed 24-bit integer ([`I24`]).
    I24,
    /// Signed 32-bit integer.
    I32,
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
}

/// A storage format: what a file, a device, a plugin port or a wire holds one
/// value of.
///
/// The implementors are `i8`, `i16`, [`I24`], `i32`, `f32` and `f64`; the trait
/// is sealed. A float format holds the signal as it is, nominally within
/// `-1.0..=1.0`. An `n`-bit integer format holds it scaled by `2^(n-1)`, so
/// full scale is `-2^(n-1)` (that is `-1.0`) to `2^(n-1) - 1`.
///
/// # Conversion
///
/// [`convert_to`](Self::convert_to) is the one conversion between any two
/// formats, and every pair follows the same rule:
///
/// - integer to float is `x / 2^(n-1)`; the result is exact in `f64`, and
///   `f32` rounds it to nearest even;
/// - float to integer is `x * 2^(n-1)`, rounded half to even, clipped to
///   `[MIN, MAX]` of the integer; NaN is `0`, and an infinity clips;
/// - float to float is a plain cast;
/// - integer to integer is the composition of the two above, computed in
///   integers with no float hop. Widening shifts left by the width difference
///   and is exact. Narrowing divides by `2^(n-m)`, rounds half to even and
///   clips to the target's range (so the source's `MAX` into a narrower format
///   rounds up out of range and clips to the target's `MAX`).
///
/// The conversion is not the inverse of itself: widening then narrowing, or
/// integer to float to the same integer, returns the original value; float to
/// integer to float is within one least significant bit of the original (half
/// of one for a value in range).
///
/// ```
/// use tutti_buffers::Sample;
///
/// assert_eq!(0.5f32.convert_to::<i16>(), 16384);
/// assert_eq!(16384i16.convert_to::<f32>(), 0.5);
/// assert_eq!(2.0f32.convert_to::<i16>(), i16::MAX); // clips
/// assert_eq!(f32::NAN.convert_to::<i16>(), 0);
/// assert_eq!(0x1234i16.convert_to::<i32>(), 0x1234_0000);
/// ```
pub trait Sample:
    sealed::Sealed + Copy + Default + Debug + PartialEq + PartialOrd + Send + Sync + 'static
{
    /// The value of silence: zero.
    const SILENCE: Self;

    /// The bits one value occupies: 8, 16, 24, 32 or 64.
    const BITS: u32;

    /// Whether the format is a float rather than a scaled integer.
    const IS_FLOAT: bool;

    /// The format as a run-time value.
    const FORMAT: SampleFormat;

    /// The size of one least significant bit in nominal units, `2^-(n-1)`
    /// for an `n`-bit integer format (the reciprocal of the scale the
    /// float-to-integer rule multiplies by), and `None` for a float, which has
    /// no step.
    const STEP: Option<f64>;

    /// The value as a nominal-unit `f64`: an integer divided by `2^(n-1)`
    /// (exact), a float widened.
    #[must_use]
    fn to_f64(self) -> f64;

    /// The format's value for a nominal-unit `f64`: a float is cast, an integer
    /// is scaled by `2^(n-1)`, rounded half to even and clipped, with NaN as
    /// `0`.
    #[must_use]
    fn from_f64(x: f64) -> Self;

    /// Converts to another format by the rule in the [trait docs](Sample).
    #[inline]
    #[must_use]
    fn convert_to<T: Sample>(self) -> T {
        self.convert_to_clipped().0
    }

    /// Converts to another format as [`convert_to`](Self::convert_to) does,
    /// and says whether the value was clipped: past an integer target's `MAX`
    /// or `MIN` before the clamp. A float target never clips, and NaN, which
    /// reads as `0`, is not a clip.
    ///
    /// ```
    /// use tutti_buffers::Sample;
    ///
    /// assert_eq!(1.0f32.convert_to_clipped::<i16>(), (i16::MAX, true));
    /// assert_eq!((-1.0f32).convert_to_clipped::<i16>(), (i16::MIN, false));
    /// assert_eq!(i16::MAX.convert_to_clipped::<i8>(), (i8::MAX, true));
    /// ```
    #[inline]
    #[must_use]
    fn convert_to_clipped<T: Sample>(self) -> (T, bool) {
        match (Self::INT_BITS, self.to_int(), T::INT_BITS) {
            (Some(from), Some(x), Some(to)) => {
                let (v, clipped) = rescale_clipped(x, from, to);
                (T::from_int(v), clipped)
            }
            _ => T::from_f64_clipped(self.to_f64()),
        }
    }
}

/// The nominal-unit scale of an `n`-bit integer format: `2^(n-1)`.
#[inline]
const fn scale(bits: u32) -> f64 {
    (1u64 << (bits - 1)) as f64
}

/// One least significant bit of an `n`-bit integer format in nominal units:
/// the reciprocal of [`scale`].
const fn step(bits: u32) -> f64 {
    1.0 / scale(bits)
}

/// Float to an `n`-bit integer: scale, round half to even, clip. NaN is `0`.
/// The flag says the rounded value lay past `MAX` or `MIN` and was clipped;
/// NaN is not a clip.
#[inline]
fn float_to_int_clipped(x: f64, bits: u32) -> (i32, bool) {
    if x.is_nan() {
        return (0, false);
    }
    let s = scale(bits);
    let v = libm::rint(x * s);
    if v > s - 1.0 {
        ((s - 1.0) as i32, true)
    } else if v < -s {
        ((-s) as i32, true)
    } else {
        (v as i32, false)
    }
}

/// An `n`-bit integer to `m` bits: the exact integer form of scaling by
/// `2^(m-n)`, rounding half to even and clipping to the `m`-bit range. The
/// flag says the rounded value lay outside that range and was clipped.
#[inline]
fn rescale_clipped(x: i32, from: u32, to: u32) -> (i32, bool) {
    let x = i64::from(x);
    if to >= from {
        return ((x << (to - from)) as i32, false);
    }
    let d = from - to;
    let floor = x >> d;
    let rem = x & ((1 << d) - 1);
    let half = 1i64 << (d - 1);
    let up = rem > half || (rem == half && floor & 1 == 1);
    let q = floor + i64::from(up);
    let max = (1i64 << (to - 1)) - 1;
    (q.clamp(-max - 1, max) as i32, q > max || q < -max - 1)
}

macro_rules! int_sample {
    ($t:ty, $bits:literal, $format:ident) => {
        impl sealed::Sealed for $t {
            const INT_BITS: Option<u32> = Some($bits);
            #[inline]
            fn to_int(self) -> Option<i32> {
                Some(i32::from(self))
            }
            #[inline]
            fn from_int(x: i32) -> Self {
                x as $t
            }
            #[inline]
            fn from_f64_clipped(x: f64) -> (Self, bool) {
                let (v, clipped) = float_to_int_clipped(x, $bits);
                (v as $t, clipped)
            }
        }
        impl Sample for $t {
            const SILENCE: Self = 0;
            const BITS: u32 = $bits;
            const IS_FLOAT: bool = false;
            const FORMAT: SampleFormat = SampleFormat::$format;
            const STEP: Option<f64> = Some(step($bits));
            #[inline]
            fn to_f64(self) -> f64 {
                f64::from(i32::from(self)) / scale($bits)
            }
            #[inline]
            fn from_f64(x: f64) -> Self {
                <Self as sealed::Sealed>::from_f64_clipped(x).0
            }
        }
    };
}

int_sample!(i8, 8, I8);
int_sample!(i16, 16, I16);
int_sample!(i32, 32, I32);

impl sealed::Sealed for I24 {
    const INT_BITS: Option<u32> = Some(24);
    #[inline]
    fn to_int(self) -> Option<i32> {
        Some(self.get())
    }
    #[inline]
    fn from_int(x: i32) -> Self {
        I24::of(x)
    }
    #[inline]
    fn from_f64_clipped(x: f64) -> (Self, bool) {
        let (v, clipped) = float_to_int_clipped(x, 24);
        (I24::of(v), clipped)
    }
}

impl Sample for I24 {
    const SILENCE: Self = I24::ZERO;
    const BITS: u32 = 24;
    const IS_FLOAT: bool = false;
    const FORMAT: SampleFormat = SampleFormat::I24;
    const STEP: Option<f64> = Some(step(24));
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self.get()) / scale(24)
    }
    #[inline]
    fn from_f64(x: f64) -> Self {
        <Self as sealed::Sealed>::from_f64_clipped(x).0
    }
}

impl sealed::Sealed for f32 {
    const INT_BITS: Option<u32> = None;
    #[inline]
    fn to_int(self) -> Option<i32> {
        None
    }
    #[inline]
    fn from_int(_: i32) -> Self {
        0.0
    }
    #[inline]
    fn from_f64_clipped(x: f64) -> (Self, bool) {
        (x as f32, false)
    }
}

impl Sample for f32 {
    const SILENCE: Self = 0.0;
    const BITS: u32 = 32;
    const IS_FLOAT: bool = true;
    const FORMAT: SampleFormat = SampleFormat::F32;
    const STEP: Option<f64> = None;
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline]
    fn from_f64(x: f64) -> Self {
        <Self as sealed::Sealed>::from_f64_clipped(x).0
    }
}

impl sealed::Sealed for f64 {
    const INT_BITS: Option<u32> = None;
    #[inline]
    fn to_int(self) -> Option<i32> {
        None
    }
    #[inline]
    fn from_int(_: i32) -> Self {
        0.0
    }
    #[inline]
    fn from_f64_clipped(x: f64) -> (Self, bool) {
        (x, false)
    }
}

impl Sample for f64 {
    const SILENCE: Self = 0.0;
    const BITS: u32 = 64;
    const IS_FLOAT: bool = true;
    const FORMAT: SampleFormat = SampleFormat::F64;
    const STEP: Option<f64> = None;
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn from_f64(x: f64) -> Self {
        <Self as sealed::Sealed>::from_f64_clipped(x).0
    }
}

/// A [`Sample`] that arithmetic runs in: `f32` or `f64`.
///
/// Its math goes through the `libm` crate, never the standard library's float
/// methods, so a render is the same on every platform. A concrete `f32` or
/// `f64` call (`x.sqrt()`) resolves to the standard library's inherent method;
/// generic code over `F: Float`, or the qualified `<f32 as Float>::sqrt(x)`,
/// reaches `libm`.
///
/// A `Float` is plain bytes ([`Pod`](bytemuck::Pod)), so a buffer of lines of
/// it can be cast to a slice of it.
///
/// ```
/// use tutti_buffers::Float;
///
/// assert_eq!(<f32 as Float>::sqrt(4.0), 2.0);
/// assert_eq!(<f64 as Float>::exp(0.0), 1.0);
/// assert_eq!(<f64 as Float>::MINUS_3_DB, core::f64::consts::FRAC_1_SQRT_2);
/// ```
pub trait Float:
    Sample
    + bytemuck::Pod
    + Display
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
    /// The value of an `f32`: exact for `f64` and for `f32`.
    #[must_use]
    fn from_f32(x: f32) -> Self;
    /// The value as an `f32`: rounded to nearest even from `f64`.
    #[must_use]
    fn to_f32(self) -> f32;
}

macro_rules! float_impl {
    (
        $t:ident,
        $abs:ident, $floor:ident, $ceil:ident, $round:ident, $roundeven:ident, $atan:ident,
        $sqrt:ident, $exp:ident, $exp2:ident,
        $ln:ident, $log10:ident, $pow:ident, $sin:ident, $cos:ident, $tan:ident,
        $tanh:ident, $atan2:ident, $fma:ident, $min:ident, $max:ident, $copysign:ident
    ) => {
        impl Float for $t {
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
            #[inline]
            fn from_f32(x: f32) -> Self {
                x as $t
            }
            #[inline]
            fn to_f32(self) -> f32 {
                self as f32
            }
        }
    };
}

float_impl!(
    f32, fabsf, floorf, ceilf, roundf, roundevenf, atanf, sqrtf, expf, exp2f, logf, log10f, powf,
    sinf, cosf, tanf, tanhf, atan2f, fmaf, fminf, fmaxf, copysignf
);
float_impl!(
    f64, fabs, floor, ceil, round, roundeven, atan, sqrt, exp, exp2, log, log10, pow, sin, cos,
    tan, tanh, atan2, fma, fmin, fmax, copysign
);
