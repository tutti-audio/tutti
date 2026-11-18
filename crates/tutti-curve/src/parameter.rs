//! The parameters of the laws that take one: [`Exponent`] and [`Curvature`].
//!
//! Each refuses what its law cannot evaluate instead of falling back.

tutti_units::ranged! {
    /// The exponent `p` of a power law (`t^p`): finite and above 0.
    ///
    /// `new` refuses anything else (NaN, infinity, zero, negatives) and never
    /// clamps. `2.0` is a quadratic, `0.5` a square root.
    ///
    /// ```
    /// use tutti_curve::{Exponent, Law};
    /// use tutti_units::Normalized;
    /// assert_eq!(
    ///     Law::PowerIn(Exponent::of(2.0)).eval(Normalized::of(0.5)),
    ///     Normalized::of(0.25)
    /// );
    /// assert!(Exponent::new(f64::NAN).is_none());
    /// assert!(Exponent::new(0.0).is_none());
    /// assert!(Exponent::new(-2.0).is_none());
    /// ```
    #[derive(PartialOrd)]
    Exponent(f64), ExponentError, "a finite exponent above 0",
    |raw| raw.is_finite() && raw > 0.0
}

impl Exponent {
    /// `1`: the power law that is linear.
    pub const ONE: Self = Self(1.0);
}

tutti_units::ranged! {
    /// The curvature `k` of [`Law::Exp`](crate::Law::Exp): finite, `|k| <= 64`.
    ///
    /// `new` refuses anything else and never clamps. `k > 0` starts slow, `k < 0`
    /// starts fast, `0` is linear.
    ///
    /// ```
    /// use tutti_curve::{Curvature, Law};
    /// use tutti_units::Normalized;
    /// assert!(Law::Exp(Curvature::of(3.0)).eval(Normalized::of(0.5)) < Normalized::of(0.5));
    /// assert!(Curvature::new(100.0).is_none());
    /// assert!(Curvature::new(f64::INFINITY).is_none());
    /// ```
    #[derive(PartialOrd)]
    Curvature(f64), CurvatureError, "a finite curvature within -64..=64",
    |raw| raw.is_finite() && raw >= -64.0 && raw <= 64.0
}

impl Curvature {
    /// The largest `|k|`.
    pub const MAX: f64 = 64.0;
    /// `0`: linear.
    pub const ZERO: Self = Self(0.0);
}

#[cfg(test)]
mod tests;
