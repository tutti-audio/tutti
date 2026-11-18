//! [`Sample`] and [`Float`]: the storage formats and the arithmetic types.

use crate::I24;
use core::fmt::Debug;
use tutti_units::Real;

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
/// A type built over a float width: `At<S>` for each `S: Float`. The shape
/// [`Float::by_width`] takes a value of, and what [`ByWidth`] holds one of at
/// each width.
pub trait OverFloat {
    /// The type at width `S`.
    type At<S: Float>;
}

/// A value of `T::At<S>` for a `S` that is `f32` or `f64`, as the one it is.
///
/// Made by [`Float::by_width`], and it carries no conversion: `F32` holds the
/// `f32` value of the shape, `F64` the `f64` one.
pub enum ByWidth<T: OverFloat> {
    /// The value, at `f32`.
    F32(T::At<f32>),
    /// The value, at `f64`.
    F64(T::At<f64>),
}

impl<T: OverFloat> core::fmt::Debug for ByWidth<T>
where
    T::At<f32>: core::fmt::Debug,
    T::At<f64>: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::F32(x) => f.debug_tuple("F32").field(x).finish(),
            Self::F64(x) => f.debug_tuple("F64").field(x).finish(),
        }
    }
}

/// A [`Sample`] that arithmetic runs in: `f32` or `f64`.
///
/// Its arithmetic is [`Real`]; a `Float` adds what a sample
/// format needs: plain bytes ([`Pod`](bytemuck::Pod)), so a buffer of lines of
/// it can be cast to a slice of it. The crossings from a quantity
/// ([`from_unit`](Real::from_unit)) and from a literal ([`lit`](Real::lit)) are
/// `Real`'s.
/// Widening is `From`/`Into` (a `Float` is `Into<f64>`), narrowing is `as`.
///
/// ```
/// use tutti_buffers::Float;
/// use tutti_units::Real;
///
/// assert_eq!(<f32 as Real>::sqrt(4.0), 2.0);
/// assert_eq!(<f32 as Float>::from_f32(0.5), 0.5);
/// ```
pub trait Float: Real + Sample + bytemuck::Pod {
    /// The −3 dB amplitude, `1/√2 ≈ 0.7071`: the attenuation of the ITU-R
    /// BS.775 / Dolby consumer downmix for a channel folded into the front
    /// pair.
    const MINUS_3_DB: Self;

    /// The value of an `f32`: exact for `f64` and for `f32`.
    #[must_use]
    fn from_f32(x: f32) -> Self;
    /// The value as an `f32`: rounded to nearest even from `f64`.
    #[must_use]
    fn to_f32(self) -> f32;

    /// `x`, a value built over this width, as the `f32` or the `f64` one it
    /// is: the type-level fact that `Self` is one of the two, in a form a
    /// `match` reads. No conversion happens and nothing is allocated.
    ///
    /// What a block does to pass through code written for one width (a plugin
    /// format's ABI) while it is generic over the graph's. `T` names the shape
    /// of `x` ([`OverFloat`]); [`InputOutput::by_width`] is the usual caller.
    #[must_use]
    fn by_width<T: OverFloat>(x: T::At<Self>) -> ByWidth<T>;
}

impl Float for f32 {
    const MINUS_3_DB: Self = core::f32::consts::FRAC_1_SQRT_2;
    #[inline]
    fn from_f32(x: f32) -> Self {
        x
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline]
    fn by_width<T: OverFloat>(x: T::At<Self>) -> ByWidth<T> {
        ByWidth::F32(x)
    }
}

impl Float for f64 {
    const MINUS_3_DB: Self = core::f64::consts::FRAC_1_SQRT_2;
    #[inline]
    fn from_f32(x: f32) -> Self {
        f64::from(x)
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn by_width<T: OverFloat>(x: T::At<Self>) -> ByWidth<T> {
        ByWidth::F64(x)
    }
}

#[cfg(test)]
mod tests {
    use super::{ByWidth, Float, OverFloat};

    /// A value over `S` is the `f32` arm at `f32` and the `f64` arm at
    /// `f64`, and keeps its contents.
    #[test]
    fn by_width_names_the_arm_of_the_width_it_was_made_at() {
        struct Words;
        impl OverFloat for Words {
            type At<S: Float> = Vec<S>;
        }
        fn arm<S: Float>(words: Vec<S>) -> &'static str {
            match S::by_width::<Words>(words) {
                ByWidth::F32(w) => {
                    assert_eq!(w, [0.5f32]);
                    "f32"
                }
                ByWidth::F64(w) => {
                    assert_eq!(w, [0.5f64]);
                    "f64"
                }
            }
        }
        assert_eq!(arm(vec![0.5f32]), "f32");
        assert_eq!(arm(vec![0.5f64]), "f64");
    }

    #[test]
    fn minus_3_db_is_one_over_root_two_at_both_widths() {
        assert_eq!(f32::MINUS_3_DB, core::f32::consts::FRAC_1_SQRT_2);
        assert_eq!(f64::MINUS_3_DB, core::f64::consts::FRAC_1_SQRT_2);
    }
}
