//! [`Sample`] and [`Float`]: the storage formats and the arithmetic types.

use crate::I24;
use core::fmt::Debug;
use tutti_units::{ReadPosition, Real, RunningPhase, Samples};

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
/// [`Sample::FORMAT`] names an implementor's variant. With the `serde` feature
/// it is the variant's name on a wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

/// The two float widths, as a value: what a plugin ABI, a shared-memory slab or a
/// wire says audio is exchanged in, where an integer format is not a choice.
///
/// [`Float::WIDTH`] names an implementor's variant. It converts to the
/// [`SampleFormat`] of the same width, and a [`SampleFormat`] converts back only
/// when it is a float. With the `serde` feature it is the variant's name on a
/// wire.
///
/// ```
/// use tutti_buffers::{FloatWidth, SampleFormat};
///
/// assert_eq!(SampleFormat::from(FloatWidth::F64), SampleFormat::F64);
/// assert_eq!(FloatWidth::try_from(SampleFormat::F32), Ok(FloatWidth::F32));
/// assert!(FloatWidth::try_from(SampleFormat::I16).is_err());
/// assert_eq!(FloatWidth::F64.bytes(), 8);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FloatWidth {
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
}

impl From<FloatWidth> for SampleFormat {
    fn from(width: FloatWidth) -> Self {
        match width {
            FloatWidth::F32 => Self::F32,
            FloatWidth::F64 => Self::F64,
        }
    }
}

impl FloatWidth {
    /// The bytes one value occupies: 4 or 8.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::F32 => size_of::<f32>(),
            Self::F64 => size_of::<f64>(),
        }
    }
}

/// An integer [`SampleFormat`] is no [`FloatWidth`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a float width")]
pub struct NotAFloat(pub SampleFormat);

impl TryFrom<SampleFormat> for FloatWidth {
    type Error = NotAFloat;

    fn try_from(format: SampleFormat) -> Result<Self, NotAFloat> {
        match format {
            SampleFormat::F32 => Ok(Self::F32),
            SampleFormat::F64 => Ok(Self::F64),
            integer => Err(NotAFloat(integer)),
        }
    }
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
///
/// ```
/// use tutti_buffers::{ByWidth, Float, OverFloat};
///
/// struct Words;
/// impl OverFloat for Words {
///     type At<S: Float> = Vec<S>;
/// }
///
/// assert!(matches!(f32::by_width::<Words>(vec![0.5]), ByWidth::F32(_)));
/// ```
pub trait OverFloat {
    /// The type at width `S`.
    type At<S: Float>;
}

/// A value of `T::At<S>` for a `S` that is `f32` or `f64`, as the one it is.
///
/// Made by [`Float::by_width`], and it carries no conversion: `F32` holds the
/// `f32` value of the shape, `F64` the `f64` one. The two arms are exhaustive:
/// `Float` is closed (`Sample` is sealed), so a float width is one of them.
pub enum ByWidth<T: OverFloat> {
    /// The value, at `f32`.
    F32(T::At<f32>),
    /// The value, at `f64`.
    F64(T::At<f64>),
}

impl<T: OverFloat> ByWidth<T> {
    /// The width this value is at.
    #[must_use]
    pub const fn width(&self) -> FloatWidth {
        match self {
            Self::F32(_) => FloatWidth::F32,
            Self::F64(_) => FloatWidth::F64,
        }
    }
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

    /// This width as a value.
    const WIDTH: FloatWidth;

    /// A running phase in cycles as data of this width, in `[0, 1)`: the top
    /// [`MANTISSA_DIGITS`](Real::MANTISSA_DIGITS) bits of the position as an integer,
    /// scaled by a power of two. The integer converts exactly and so does the
    /// scaling, so the value is the position truncated to the float's own
    /// resolution and is never rounded up to `1.0`.
    ///
    /// This is the one spelling of phase to data; a processor multiplies by
    /// [`TAU`](Real::TAU) after it when its wave takes radians.
    ///
    /// ```
    /// use tutti_buffers::Float;
    /// use tutti_units::{PhaseStep, RunningPhase};
    ///
    /// let half = RunningPhase::START.advance(PhaseStep::from_count(1 << 63));
    /// assert_eq!(f32::from_phase(half), 0.5);
    /// assert_eq!(f64::from_phase(half), 0.5);
    /// ```
    #[must_use]
    fn from_phase(p: RunningPhase) -> Self;

    /// A 64-bit word read as a `0.64` fixed-point fraction, as data of this width in `[0, 1)`:
    /// the top [`MANTISSA_DIGITS`](Real::MANTISSA_DIGITS) bits kept and truncated, so the value
    /// is uniform over the grid of `2^-MANTISSA_DIGITS` for uniform bits and never rounds up to
    /// `1.0`. The integer converts exactly and so does the scaling.
    ///
    /// ```
    /// use tutti_buffers::Float;
    ///
    /// assert_eq!(f32::from_fraction_bits(1 << 63), 0.5);
    /// assert_eq!(f64::from_fraction_bits(u64::MAX), 1.0 - 2f64.powi(-53));
    /// ```
    #[must_use]
    fn from_fraction_bits(bits: u64) -> Self;

    /// A read position as its whole frames and a fraction of this width in `[0, 1)`:
    /// the top bits of the position's fraction (the width's mantissa, capped at the
    /// fraction's own [`FRACTION_BITS`](ReadPosition::FRACTION_BITS)) as an integer,
    /// scaled by a power of two. The integer converts exactly and so does the
    /// scaling, so the fraction is the position's truncated to the float's own
    /// resolution and is never rounded up to `1.0`, whatever the whole frames are.
    ///
    /// This is the one spelling of position to data; an interpolating read takes the
    /// frames as the index and the fraction as the weight.
    ///
    /// ```
    /// use tutti_buffers::Float;
    /// use tutti_units::{ReadPosition, Samples};
    ///
    /// let p = ReadPosition::from_count((7_u64 << 32) | (1 << 31));
    /// assert_eq!(f32::split_position(p), (Samples::new(7), 0.5));
    /// assert_eq!(f64::split_position(p), (Samples::new(7), 0.5));
    /// ```
    #[must_use]
    fn split_position(p: ReadPosition) -> (Samples, Self);

    /// The position `by` frames on from `p`, `by` data of a modulation: the one
    /// spelling of data to position. The offset is widened to `f64` (exact), scaled
    /// to counts (exact) and rounded to the nearest `2^-32` frame, so the sum is
    /// exact to the position's quantum, ties rounding away from zero. It saturates
    /// at [`ReadPosition::START`] for an offset that reaches behind it and at
    /// [`ReadPosition::MAX`] past the range; a NaN offset moves nothing.
    ///
    /// ```
    /// use tutti_buffers::Float;
    /// use tutti_units::ReadPosition;
    ///
    /// let p = ReadPosition::from_count(10_u64 << 32);
    /// assert_eq!(f32::offset_position(p, 0.5).get(), (10_u64 << 32) + (1 << 31));
    /// assert_eq!(f64::offset_position(p, -11.0), ReadPosition::START);
    /// ```
    #[must_use]
    fn offset_position(p: ReadPosition, by: Self) -> ReadPosition {
        let counts = Into::<f64>::into(by) * ReadPosition::COUNTS_PER_FRAME;
        // `as` saturates at the `u64` range and maps NaN to 0.
        let moved = libm::round(counts.abs()) as u64;
        ReadPosition::from_count(if counts >= 0.0 {
            p.get().saturating_add(moved)
        } else {
            p.get().saturating_sub(moved)
        })
    }

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
    /// of `x` ([`OverFloat`]);
    /// [`InputOutput::by_width`](crate::InputOutput::by_width) is the usual
    /// caller.
    #[must_use]
    fn by_width<T: OverFloat>(x: T::At<Self>) -> ByWidth<T>;
}

/// The phase and position crossings, written once for both widths. Each
/// converts an integer of at most the width's mantissa bits and scales it by a
/// power of two, so both `as` conversions and the product are exact.
macro_rules! position_crossings {
    ($t:ty) => {
        #[inline]
        fn from_fraction_bits(bits: u64) -> Self {
            let digits = <$t as Real>::MANTISSA_DIGITS;
            let top = bits >> (u64::BITS - digits);
            top as $t * (1.0 / (1_u64 << digits) as $t)
        }
        #[inline]
        fn from_phase(p: RunningPhase) -> Self {
            Self::from_fraction_bits(p.get())
        }
        #[inline]
        fn split_position(p: ReadPosition) -> (Samples, Self) {
            let bits = <$t as Real>::MANTISSA_DIGITS.min(ReadPosition::FRACTION_BITS);
            let fraction = (p.get() & ((1 << ReadPosition::FRACTION_BITS) - 1))
                >> (ReadPosition::FRACTION_BITS - bits);
            (p.frames(), fraction as $t * (1.0 / (1_u64 << bits) as $t))
        }
    };
}

impl Float for f32 {
    const MINUS_3_DB: Self = core::f32::consts::FRAC_1_SQRT_2;
    const WIDTH: FloatWidth = FloatWidth::F32;
    position_crossings!(f32);
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
    const WIDTH: FloatWidth = FloatWidth::F64;
    position_crossings!(f64);
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
    use tutti_units::{
        Hz, PhaseIncrement, PhaseStep, ReadPosition, RunningPhase, SampleRate, Samples,
    };

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

    fn at(count: u64) -> RunningPhase {
        RunningPhase::START.advance(PhaseStep::from_count(count))
    }

    #[test]
    fn the_crossing_is_the_top_24_bits_over_two_to_the_24_at_f32() {
        for top in [0_u64, 1, 12_345, (1 << 23) + 1, (1 << 24) - 1] {
            // Garbage below the top 24 bits must not change the value.
            let low = (1_u64 << 40) - 1;
            let v = f32::from_phase(at((top << 40) | low));
            assert_eq!(f64::from(v), top as f64 / 16_777_216.0, "top {top}");
        }
    }

    #[test]
    fn the_crossing_is_the_top_53_bits_over_two_to_the_53_at_f64() {
        for top in [0_u64, 1, 0x1f_ffff_ffff_ffff, (1 << 52) + 7] {
            let v = f64::from_phase(at((top << 11) | 0x7ff));
            assert_eq!(v, top as f64 / 9_007_199_254_740_992.0, "top {top}");
        }
    }

    #[test]
    fn the_crossing_stays_below_one_for_the_largest_position_at_both_widths() {
        let last = at(u64::MAX);
        assert!(f32::from_phase(last) < 1.0);
        assert!(f64::from_phase(last) < 1.0);
        assert_eq!(f32::from_phase(last), 1.0 - f32::EPSILON / 2.0);
        assert_eq!(f64::from_phase(last), 1.0 - f64::EPSILON / 2.0);
    }

    #[test]
    fn the_crossing_is_monotone_in_the_position() {
        let mut prev32 = -1.0_f32;
        let mut prev64 = -1.0_f64;
        let mut counts: Vec<u64> = (0..4096_u64)
            .map(|k| k.wrapping_mul(0x0040_1001_0020_0401))
            .collect();
        counts.sort_unstable();
        for c in counts {
            let (a, b) = (f32::from_phase(at(c)), f64::from_phase(at(c)));
            assert!(a >= prev32 && b >= prev64, "count {c}");
            (prev32, prev64) = (a, b);
        }
    }

    /// The failure a float phase had: at 0.01 Hz and 192 kHz the step is 5.2e-8 cycles, and an
    /// `f32` phase near 0.5 rounds each add to one ulp (6.0e-8), a rate 15% fast. The running
    /// phase read through `f32` wraps on the period, ten times, each to within one sample.
    #[test]
    fn an_f32_read_phase_at_001_hz_and_192_khz_completes_three_periods_to_within_a_sample() {
        let rate = SampleRate::new(192_000.0).unwrap();
        let step = PhaseStep::from(PhaseIncrement::per_sample(Hz::new(0.01), rate));
        let period = 19_200_000_u64;
        let mut p = RunningPhase::START;
        let mut wraps = 0_u32;
        let mut last = 0.0_f32;
        for n in 1..=3 * period {
            p = p.advance(step);
            let v = f32::from_phase(p);
            if v < last {
                wraps += 1;
                // The wrap lands on a multiple of the period, to within one sample.
                let k = n / period;
                assert!(n.abs_diff(k * period) <= 1, "wrap at {n}");
            }
            last = v;
        }
        assert_eq!(wraps, 2);
        assert!(f32::from_phase(p) < 1e-6 || f32::from_phase(p) > 1.0 - 1e-6);
    }

    fn position_at(count: u64) -> ReadPosition {
        ReadPosition::from_count(count)
    }

    #[test]
    fn the_split_is_the_whole_frames_and_the_top_24_fraction_bits_over_two_to_the_24_at_f32() {
        for top in [0_u64, 1, 12_345, (1 << 23) + 1, (1 << 24) - 1] {
            // Garbage below the top 24 fraction bits must not change the value.
            let (whole, frac) = f32::split_position(position_at((7_u64 << 32) | (top << 8) | 0xff));
            assert_eq!(whole, Samples::new(7));
            assert_eq!(f64::from(frac), top as f64 / 16_777_216.0, "top {top}");
        }
    }

    #[test]
    fn the_split_is_the_whole_32_fraction_bits_over_two_to_the_32_at_f64() {
        for low in [0_u64, 1, 0x8000_0000, 0xffff_ffff] {
            let (whole, frac) = f64::split_position(position_at((1_900_800_u64 << 32) | low));
            assert_eq!(whole, Samples::new(1_900_800));
            assert_eq!(
                frac,
                low as f64 / ReadPosition::COUNTS_PER_FRAME,
                "low {low}"
            );
        }
    }

    #[test]
    fn the_split_fraction_stays_below_one_for_the_largest_position_at_both_widths() {
        let (whole, f32_frac) = f32::split_position(ReadPosition::MAX);
        assert_eq!(whole, Samples::new(u32::MAX as usize));
        assert!(f32_frac < 1.0);
        assert!(f64::split_position(ReadPosition::MAX).1 < 1.0);
    }

    #[test]
    fn an_offset_of_a_dyadic_frame_count_moves_exactly_that_many_quanta() {
        let base = position_at(1_000_u64 << 32);
        // 2^-3 and 3 * 2^-5 are exact in f32; 2^-32 is the quantum itself.
        assert_eq!(
            Float::offset_position(base, 0.125_f32).get(),
            (1_000_u64 << 32) + (1 << 29)
        );
        assert_eq!(
            Float::offset_position(base, -0.093_75_f32).get(),
            (1_000_u64 << 32) - 3 * (1 << 27)
        );
        assert_eq!(
            Float::offset_position(base, 2f64.powi(-32)).get(),
            (1_000_u64 << 32) + 1
        );
        assert_eq!(
            Float::offset_position(base, -(2f64.powi(-32))).get(),
            (1_000_u64 << 32) - 1
        );
    }

    #[test]
    fn an_offset_rounds_to_the_nearest_quantum() {
        let base = position_at(0x1234_u64 << 32);
        // 0.4 and 0.6 of a quantum.
        assert_eq!(Float::offset_position(base, 0.4 * 2f64.powi(-32)), base);
        assert_eq!(
            Float::offset_position(base, 0.6 * 2f64.powi(-32)).get(),
            base.get() + 1
        );
    }

    #[test]
    fn an_offset_saturates_at_the_start_and_at_the_range() {
        let base = position_at(5_u64 << 32);
        assert_eq!(Float::offset_position(base, -6.0_f32), ReadPosition::START);
        assert_eq!(
            Float::offset_position(base, f32::NEG_INFINITY),
            ReadPosition::START
        );
        assert_eq!(
            Float::offset_position(base, 4.0e9_f32).get(),
            base.get() + (4_000_000_000_u64 << 32)
        );
        assert_eq!(
            Float::offset_position(ReadPosition::MAX, 1.0_f32),
            ReadPosition::MAX
        );
        assert_eq!(
            Float::offset_position(base, f32::INFINITY),
            ReadPosition::MAX
        );
    }

    #[test]
    fn a_nan_offset_moves_nothing() {
        let base = position_at(77);
        assert_eq!(Float::offset_position(base, f32::NAN), base);
    }

    #[test]
    fn an_offset_of_half_a_quantum_rounds_away_from_zero_in_both_directions() {
        let base = position_at(1_000);
        let half = 2f64.powi(-33);
        assert_eq!(Float::offset_position(base, half).get(), 1_001);
        assert_eq!(Float::offset_position(base, -half).get(), 999);
        assert_eq!(
            Float::offset_position(base, -1.5 * 2f64.powi(-32)).get(),
            998
        );
    }

    #[test]
    fn a_negative_offset_moves_back_by_that_many_quanta() {
        let base = position_at((10_u64 << 32) + 5);
        assert_eq!(
            Float::offset_position(base, -2.5_f32).get(),
            (7_u64 << 32) + (1 << 31) + 5
        );
    }

    /// A fraction word keeps its top mantissa bits: 0 is 0, `2^63` is a half, and the largest
    /// word is `1 - 2^-digits`, below 1 (`u64::MAX as f32 / 2^64` would round to exactly 1).
    #[test]
    fn a_fraction_word_truncates_to_the_width_and_stays_below_one() {
        assert_eq!(f32::from_fraction_bits(0), 0.0);
        assert_eq!(f64::from_fraction_bits(0), 0.0);
        assert_eq!(f32::from_fraction_bits(1 << 63), 0.5);
        assert_eq!(f64::from_fraction_bits(1 << 63), 0.5);
        assert_eq!(f32::from_fraction_bits(u64::MAX), 1.0 - 2f32.powi(-24));
        assert_eq!(f64::from_fraction_bits(u64::MAX), 1.0 - 2f64.powi(-53));
        assert!(f32::from_fraction_bits(u64::MAX) < 1.0);
    }
}
