//! [`Lane`]: the SIMD vector the voice bank runs its voices across, and
//! [`SynthFloat`], the float that picks it.
//!
//! A lane group is `f32x8` at `f32` and `f64x4` at `f64`: the same 256 bits, so a
//! group costs the same instructions at either width and the bank holds half as
//! many voices per group at `f64`.

use core::fmt::Debug;
use core::ops::{AddAssign, BitAnd, Neg};
use tutti_buffers::Float;
use tutti_dsp::Lanes;
use wide::{f32x8, f64x4, i32x4, u32x4, u32x8};

/// A float the synth renders in.
///
/// Sealed with the two widths: `f32` runs [`f32x8`] lanes and `f64` runs [`f64x4`].
pub trait SynthFloat: Float {
    /// The lane group this float's bank is made of.
    type Lanes: Lane<Scalar = Self>;
}

impl SynthFloat for f32 {
    type Lanes = f32x8;
}

impl SynthFloat for f64 {
    type Lanes = f64x4;
}

/// A group of voice lanes of one float: the arithmetic of the bank's kernels, written
/// once over `f32x8` and `f64x4`.
///
/// A comparison answers a mask lane (all bits set where true), which [`pick`](Self::pick)
/// selects with and `&` combines. Sealed through [`Lanes`].
pub trait Lane:
    Lanes + Neg<Output = Self> + AddAssign + BitAnd<Output = Self> + Debug + Send + Sync + 'static
{
    /// One lane's float.
    type Scalar: Float;
    /// The noise generators of every lane, advanced together.
    type Rng: Copy + Debug + Send + Sync + 'static;

    /// How many voices a group holds.
    const LANES: usize;
    /// Zero in every lane.
    const ZERO: Self;
    /// One in every lane.
    const ONE: Self;

    /// `x` in every lane.
    fn splat(x: Self::Scalar) -> Self;
    /// The lanes in order.
    fn as_slice(&self) -> &[Self::Scalar];
    /// The lanes in order, mutably.
    fn as_mut_slice(&mut self) -> &mut [Self::Scalar];
    /// `self * a + b`, fused.
    #[must_use]
    fn mul_add(self, a: Self, b: Self) -> Self;
    /// The magnitudes.
    #[must_use]
    fn abs(self) -> Self;
    /// The larger of each pair of lanes.
    #[must_use]
    fn max(self, other: Self) -> Self;
    /// Each lane pinned to `lo..=hi`.
    #[must_use]
    fn clamp(self, lo: Self, hi: Self) -> Self;
    /// The mask of lanes below `other`.
    #[must_use]
    fn lt(self, other: Self) -> Self;
    /// The mask of lanes above `other`.
    #[must_use]
    fn gt(self, other: Self) -> Self;
    /// The mask of lanes at or above `other`.
    #[must_use]
    fn ge(self, other: Self) -> Self;
    /// The mask of lanes not equal to `other`.
    #[must_use]
    fn ne(self, other: Self) -> Self;
    /// `if_true` where this mask is set, `if_false` elsewhere.
    #[must_use]
    fn pick(self, if_true: Self, if_false: Self) -> Self;
    /// Whether any lane of this mask is set.
    fn any(self) -> bool;
    /// The mask's lanes as bits, lane 0 lowest.
    fn bits(self) -> u32;
    /// The lanes' sum.
    fn sum(self) -> Self::Scalar;

    /// One generator per lane, seeded from `seed` in lane order. Each seed is nonzero.
    fn seed_rng(seed: impl FnMut() -> u32) -> Self::Rng;
    /// Advance every lane's [`Xorshift32`](tutti_dsp::Xorshift32) one step and return white
    /// noise in `-1.0..1.0`.
    fn white(state: &mut Self::Rng) -> Self;
}

impl Lane for f32x8 {
    type Scalar = f32;
    type Rng = u32x8;

    const LANES: usize = 8;
    const ZERO: Self = f32x8::ZERO;
    const ONE: Self = f32x8::ONE;

    #[inline(always)]
    fn splat(x: f32) -> Self {
        f32x8::splat(x)
    }
    #[inline(always)]
    fn as_slice(&self) -> &[f32] {
        self.as_array()
    }
    #[inline(always)]
    fn as_mut_slice(&mut self) -> &mut [f32] {
        self.as_mut_array()
    }
    #[inline(always)]
    fn mul_add(self, a: Self, b: Self) -> Self {
        f32x8::mul_add(self, a, b)
    }
    #[inline(always)]
    fn abs(self) -> Self {
        f32x8::abs(self)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        f32x8::max(self, other)
    }
    #[inline(always)]
    fn clamp(self, lo: Self, hi: Self) -> Self {
        self.fast_clamp(lo, hi)
    }
    #[inline(always)]
    fn lt(self, other: Self) -> Self {
        self.simd_lt(other)
    }
    #[inline(always)]
    fn gt(self, other: Self) -> Self {
        self.simd_gt(other)
    }
    #[inline(always)]
    fn ge(self, other: Self) -> Self {
        self.simd_ge(other)
    }
    #[inline(always)]
    fn ne(self, other: Self) -> Self {
        self.simd_ne(other)
    }
    #[inline(always)]
    fn pick(self, if_true: Self, if_false: Self) -> Self {
        self.select(if_true, if_false)
    }
    #[inline(always)]
    fn any(self) -> bool {
        f32x8::any(self)
    }
    #[inline(always)]
    fn bits(self) -> u32 {
        self.to_bitmask()
    }
    #[inline(always)]
    fn sum(self) -> f32 {
        self.reduce_add()
    }

    fn seed_rng(mut seed: impl FnMut() -> u32) -> u32x8 {
        u32x8::new(core::array::from_fn(|_| seed()))
    }

    /// The top 23 bits become the mantissa of a float in `1.0..2.0`, which maps
    /// linearly onto the output range with no division and no bias toward zero.
    #[inline(always)]
    fn white(state: &mut u32x8) -> Self {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *state = x;
        let bits = (x >> 9) | u32x8::splat(0x3F80_0000);
        let one_to_two: f32x8 = wide::bytemuck::cast(bits);
        one_to_two + one_to_two - f32x8::splat(3.0)
    }
}

impl Lane for f64x4 {
    type Scalar = f64;
    type Rng = u32x4;

    const LANES: usize = 4;
    const ZERO: Self = f64x4::ZERO;
    const ONE: Self = f64x4::ONE;

    #[inline(always)]
    fn splat(x: f64) -> Self {
        f64x4::splat(x)
    }
    #[inline(always)]
    fn as_slice(&self) -> &[f64] {
        self.as_array()
    }
    #[inline(always)]
    fn as_mut_slice(&mut self) -> &mut [f64] {
        self.as_mut_array()
    }
    #[inline(always)]
    fn mul_add(self, a: Self, b: Self) -> Self {
        f64x4::mul_add(self, a, b)
    }
    #[inline(always)]
    fn abs(self) -> Self {
        f64x4::abs(self)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        f64x4::max(self, other)
    }
    #[inline(always)]
    fn clamp(self, lo: Self, hi: Self) -> Self {
        self.fast_clamp(lo, hi)
    }
    #[inline(always)]
    fn lt(self, other: Self) -> Self {
        self.simd_lt(other)
    }
    #[inline(always)]
    fn gt(self, other: Self) -> Self {
        self.simd_gt(other)
    }
    #[inline(always)]
    fn ge(self, other: Self) -> Self {
        self.simd_ge(other)
    }
    #[inline(always)]
    fn ne(self, other: Self) -> Self {
        self.simd_ne(other)
    }
    #[inline(always)]
    fn pick(self, if_true: Self, if_false: Self) -> Self {
        self.select(if_true, if_false)
    }
    #[inline(always)]
    fn any(self) -> bool {
        f64x4::any(self)
    }
    #[inline(always)]
    fn bits(self) -> u32 {
        self.to_bitmask()
    }
    #[inline(always)]
    fn sum(self) -> f64 {
        self.reduce_add()
    }

    fn seed_rng(mut seed: impl FnMut() -> u32) -> u32x4 {
        u32x4::new(core::array::from_fn(|_| seed()))
    }

    /// The top 31 bits as an integer, scaled by `2^-30` and centred: exact in `f64`,
    /// uniform on `-1.0..1.0` with no bias toward zero.
    #[inline(always)]
    fn white(state: &mut u32x4) -> Self {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *state = x;
        let top: i32x4 = wide::bytemuck::cast(x >> 1);
        f64x4::from_i32x4(top) * f64x4::splat(1.0 / f64::from(1_u32 << 30)) - f64x4::ONE
    }
}

#[cfg(test)]
mod tests;
