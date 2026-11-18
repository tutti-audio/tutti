//! A control that is not finite reads as its default.

/// A float that can say whether it is finite: `f32` and `f64`.
pub(crate) trait Finite: Copy {
    /// Whether the value is neither NaN nor infinite.
    fn finite(self) -> bool;
}

impl Finite for f32 {
    fn finite(self) -> bool {
        self.is_finite()
    }
}

impl Finite for f64 {
    fn finite(self) -> bool {
        self.is_finite()
    }
}

/// `x` if it is finite, else `default`, exactly: a finite value is not narrowed.
pub(crate) fn finite_or<T: Finite>(x: T, default: T) -> T {
    if x.finite() { x } else { default }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_or_passes_a_finite_value_and_defaults_nan_and_infinities() {
        assert_eq!(finite_or(2.5_f32, 7.0), 2.5);
        assert_eq!(finite_or(f32::NAN, 7.0), 7.0);
        assert_eq!(finite_or(f32::INFINITY, 7.0), 7.0);
        assert_eq!(finite_or(f32::NEG_INFINITY, 7.0), 7.0);
        assert_eq!(finite_or(-1e300_f64, 7.0), -1e300);
        assert_eq!(finite_or(f64::NAN, 7.0), 7.0);
    }
}
