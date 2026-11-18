mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// The raw float of a unit a [`Trajectory`](crate::Trajectory) automates: `f32`
/// or `f64`. A trajectory is evaluated in `f64` and the value read is narrowed
/// once to the unit's width.
///
/// Sealed: this is the one place both widths are accepted. Every continuous
/// unit becomes `f64`; the `f32` impl is deleted with the last `f32` unit.
pub trait CurveRaw: Copy + sealed::Sealed {
    /// The value as an `f64`, exactly.
    fn widen(self) -> f64;

    /// The `f64` rounded to this width.
    fn narrow(wide: f64) -> Self;
}

impl CurveRaw for f32 {
    #[inline]
    fn widen(self) -> f64 {
        f64::from(self)
    }

    #[inline]
    fn narrow(wide: f64) -> Self {
        wide as f32
    }
}

impl CurveRaw for f64 {
    #[inline]
    fn widen(self) -> f64 {
        self
    }

    #[inline]
    fn narrow(wide: f64) -> Self {
        wide
    }
}
