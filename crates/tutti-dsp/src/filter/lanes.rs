//! [`Lanes`]: the arithmetic a filter's per-frame step runs in.

use core::ops::{Add, Div, Mul, Sub};

use tutti_units::Real;

/// The arithmetic one frame of a filter recurrence runs in: one value (`f32`,
/// `f64`) or several side by side (`wide::f32x8`, the polysynth's voice lanes).
///
/// The SVF and ladder steps ([`svf_step`](crate::svf_step),
/// [`ladder_step`](crate::ladder_step)) are written once over it, so a lane bank
/// runs the same recurrence as a filter node rather than a copy of it.
///
/// Sealed: the three types above are the set, so `wide` is not a version a
/// downstream crate can plug into.
pub trait Lanes:
    sealed::Sealed
    + Copy
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
{
    /// The numeric literal `x` in every lane: only a literal argument, as `Real::lit`.
    fn lit(x: f64) -> Self;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
    impl Sealed for wide::f32x8 {}
}

impl Lanes for f32 {
    #[inline(always)]
    fn lit(x: f64) -> Self {
        <f32 as Real>::from_design(x)
    }
}

impl Lanes for f64 {
    #[inline(always)]
    fn lit(x: f64) -> Self {
        x
    }
}

impl Lanes for wide::f32x8 {
    #[inline(always)]
    fn lit(x: f64) -> Self {
        wide::f32x8::splat(<f32 as Real>::from_design(x))
    }
}
