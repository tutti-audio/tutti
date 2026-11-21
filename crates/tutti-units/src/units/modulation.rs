// Frequency modulation: an operator's frequency ratio and a modulation index.

crate::ranged! {
    /// An operator's or partial's frequency as a multiple of the base frequency,
    /// `fc = ratio * f`: finite and above zero. A ratio that is not a whole number is
    /// inharmonic.
    ///
    /// [`new`](Self::new) refuses NaN, the infinities, zero and anything negative, and so
    /// do `TryFrom<f64>` and serde; [`of`](Self::of) is for a literal. It is a
    /// configuration value, not a [`Unit`].
    ///
    /// ```
    /// use tutti_units::{FrequencyRatio, Hz};
    ///
    /// assert_eq!(Hz::new(440.0) * FrequencyRatio::of(1.5), Hz::new(660.0));
    /// assert_eq!(Hz::new(440.0) * FrequencyRatio::OCTAVE, Hz::new(880.0));
    /// assert_eq!(FrequencyRatio::new(0.0), None);
    /// ```
    #[repr(transparent)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    FrequencyRatio(f64), FrequencyRatioOutOfRange, "finite and above zero",
    |raw| raw > 0.0 && raw <= f64::MAX
}

// `ModulationIndex` is a `Unit` (below), which is already a `Quantity`, so it takes the
// type and its error without the macro's own `Quantity` impl.
crate::ranged! {
    @items []
    /// The modulation index `beta` of frequency modulation: the peak phase deviation in
    /// radians, finite and at least zero. It scales a carrier's modulator and an
    /// operator's self-feedback.
    ///
    /// [`new`](Self::new) refuses NaN, the infinities and anything negative, and so do
    /// `TryFrom<f64>` and serde; [`of`](Self::of) is for a literal. A negative index is
    /// only a phase flip, so it is unwritable.
    ///
    /// ```
    /// use tutti_units::{ModulationIndex, Radians};
    ///
    /// assert_eq!(ModulationIndex::of(2.5).to_radians(), Radians::new(2.5));
    /// assert_eq!(ModulationIndex::new(-1.0), None);
    /// ```
    #[repr(transparent)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    ModulationIndex(f64), ModulationIndexOutOfRange, "finite and at least zero",
    |raw| raw >= 0.0 && raw <= f64::MAX
}

use crate::{Hz, Radians, Unit};

// Deliberately absent, and the omissions are load-bearing:
//
// - `FrequencyRatio * FrequencyRatio` — two finite positives can overflow to infinity or
//   underflow to zero, so the product is not closed. Multiply through `Hz`: `f * a * b`.
// - `Hz / Hz -> FrequencyRatio` — `Hz`'s `Ratio` already gives `Hz / Hz -> f64`. The
//   checked conversion is `FrequencyRatio::new(f / base)`.
// - `FrequencyRatio + FrequencyRatio`, `From<f64>` — a ratio is not an amount, and a bare
//   number cannot be refused there; `new` is the checked constructor.
// - `FrequencyRatio` as a `Unit` — it is a configuration value, not a parameter cell.
// - `ModulationIndex - ModulationIndex` — the difference can be negative, which no index
//   is.
// - `ModulationIndex + ModulationIndex` — not yet used; the replacement is
//   `ModulationIndex::new(a.get() + b.get())`.
// - `ModulationIndex * f64`, `/ f64` — a negative or NaN factor leaves the range;
//   `scaled` is the replacement and refuses it.
// - `ModulationIndex::from(f64)` — a bare number cannot be refused there; `new` is the
//   checked constructor. `Unit::from_raw` is `clamped`, the one saturating constructor,
//   since a parameter cell holds any float.
impl FrequencyRatio {
    /// The base frequency itself, `1`.
    pub const UNISON: Self = Self(1.0);

    /// Twice the base frequency, `2`.
    pub const OCTAVE: Self = Self(2.0);
}

impl ModulationIndex {
    /// No modulation.
    pub const ZERO: Self = Self(0.0);

    /// The index scaled by `factor`, or `None` when the product is not an index: a
    /// negative or NaN factor, or an overflow.
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

    /// The peak phase deviation as an angle, for the phase sum `sin(wt + beta * m)`.
    #[inline]
    #[must_use]
    pub fn to_radians(self) -> Radians {
        Radians::new(self.0)
    }
}

/// `from_raw` saturates ([`ModulationIndex::clamped`]): a parameter cell holds any float,
/// and a unit's `from_raw` cannot refuse.
impl Unit for ModulationIndex {
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

impl Default for ModulationIndex {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

impl PartialOrd for FrequencyRatio {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl PartialOrd for ModulationIndex {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

/// The frequency of a partial at `rhs` times the base, `fc = ratio * f`.
impl core::ops::Mul<FrequencyRatio> for Hz {
    type Output = Hz;
    #[inline]
    fn mul(self, rhs: FrequencyRatio) -> Hz {
        Hz::new(self.get() * rhs.0)
    }
}

/// The frequency of a partial at the ratio times `rhs`, `fc = ratio * f`.
impl core::ops::Mul<Hz> for FrequencyRatio {
    type Output = Hz;
    #[inline]
    fn mul(self, rhs: Hz) -> Hz {
        rhs * self
    }
}

#[cfg(test)]
mod tests;
