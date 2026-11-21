// Distance and the speed of sound.

// `Metres` is a `Unit` (below), which is already a `Quantity`, so it takes the type and its
// error without the macro's own `Quantity` impl.
crate::ranged! {
    @items []
    /// A distance in metres: finite and at least zero.
    ///
    /// [`new`](Self::new) refuses NaN, the infinities and anything negative, and so
    /// do `TryFrom<f64>` and serde; [`of`](Self::of) is for a literal. A
    /// separation between two points has no direction, so there is no signed
    /// distance type: [`abs_diff`](Self::abs_diff) is the gap between two
    /// distances.
    ///
    /// ```
    /// use tutti_units::{Metres, Seconds, Speed};
    ///
    /// let delay = Metres::of(343.0) / Speed::SOUND_IN_AIR_20C;
    /// assert_eq!(delay, Seconds::new(1.0));
    /// assert_eq!(Metres::of(2.0) / Metres::of(4.0), 0.5);
    /// assert_eq!(Metres::new(-1.0), None);
    /// ```
    #[repr(transparent)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    Metres(f64), MetresOutOfRange, "finite and at least zero",
    |raw| raw >= 0.0 && raw <= f64::MAX
}

crate::ranged! {
    /// A speed in metres per second: finite and above zero. It is the speed of
    /// a wave in a medium, so [`SOUND_IN_AIR_20C`](Self::SOUND_IN_AIR_20C) is
    /// the constant a delay is computed from; there is no signed velocity type.
    ///
    /// [`new`](Self::new) refuses NaN, the infinities, zero and anything
    /// negative, and so do `TryFrom<f64>` and serde; [`of`](Self::of) is for a
    /// literal.
    ///
    /// ```
    /// use tutti_units::Speed;
    ///
    /// assert_eq!(Speed::new(343.0), Some(Speed::SOUND_IN_AIR_20C));
    /// assert_eq!(Speed::new(0.0), None);
    /// ```
    #[repr(transparent)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    Speed(f64), SpeedOutOfRange, "finite and above zero",
    |raw| raw > 0.0 && raw <= f64::MAX
}

use crate::{Seconds, Unit};

// Deliberately absent, and the omissions are load-bearing:
//
// - `Sub` (`Additive`) — the difference of two distances can be negative, which
//   no `Metres` is. `abs_diff` is the replacement.
// - `Mul<f64>` / `Div<f64>` (`Scalable`) — a negative or NaN factor leaves the
//   range. `scaled` is the replacement: it refuses the result `new` refuses.
// - `From<f64>` — a bare number cannot be refused there; `new` is the checked
//   constructor. `Unit::from_raw` is `clamped`, the one saturating constructor, since
//   a parameter cell holds any float.
// - `Speed * Seconds -> Metres` and `Metres / Seconds -> Speed` — not yet used.
impl Metres {
    /// No distance.
    pub const ZERO: Self = Self(0.0);

    /// The distance scaled by `factor`, or `None` when the product is not a
    /// distance: a negative or NaN factor, or an overflow.
    #[inline]
    #[must_use]
    pub const fn scaled(self, factor: f64) -> Option<Self> {
        Self::new(self.0 * factor)
    }

    /// `raw` constrained into the range; NaN is [`ZERO`](Self::ZERO). The saturating
    /// constructor, for a value that arrives from a parameter cell, which holds any
    /// float.
    #[inline]
    #[must_use]
    pub const fn clamped(raw: f64) -> Self {
        if raw.is_nan() || raw < 0.0 {
            Self::ZERO
        } else if raw > f64::MAX {
            Self(f64::MAX)
        } else {
            Self(raw)
        }
    }

    /// The larger of the two distances.
    #[inline]
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }

    /// The gap between two distances, `|self - other|`: the replacement for
    /// `Sub`, which a non-negative distance cannot have.
    #[inline]
    #[must_use]
    pub fn abs_diff(self, other: Self) -> Self {
        Self((self.0 - other.0).abs())
    }
}

impl Speed {
    /// The speed of sound in dry air at 20 degrees Celsius, 343 m/s.
    pub const SOUND_IN_AIR_20C: Self = Self(343.0);
}

/// `from_raw` saturates ([`Metres::clamped`]): a parameter cell holds any float, and a
/// unit's `from_raw` cannot refuse.
impl Unit for Metres {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self::clamped(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.0
    }
}

impl Default for Metres {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

impl PartialOrd for Metres {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl PartialOrd for Speed {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

/// Distances add. A sum that overflows `f64` stays at the largest finite
/// distance, so the range holds; that is unreachable for a physical distance.
impl core::ops::Add for Metres {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self((self.0 + rhs.0).min(f64::MAX))
    }
}

impl core::ops::AddAssign for Metres {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

/// The ratio of two distances, a bare number: the `1 / r` of a gain.
impl core::ops::Div for Metres {
    type Output = f64;
    #[inline]
    fn div(self, rhs: Self) -> f64 {
        self.0 / rhs.0
    }
}

/// The time a wave at `rhs` takes to cover the distance, `r / c`.
impl core::ops::Div<Speed> for Metres {
    type Output = Seconds;
    #[inline]
    fn div(self, rhs: Speed) -> Seconds {
        Seconds::new(self.0 / rhs.0)
    }
}

#[cfg(test)]
mod tests;
