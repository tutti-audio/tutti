// Level: decibels, the linear amplitude, and the signed linear gain.

use super::*;

unit! {
    /// Amplitude in decibels. Used for thresholds, gains, makeup, ceilings.
    pub struct Db(pub f64): Unit, Ordered, Bounded, Additive, Signed;
}
// dB is logarithmic, so cascaded gain stages ADD — unlike `Beat + Beat`, this
// is meaningful. `Neg` inverts a gain (cut <-> boost).
// NOT `Scalable`: `Db(-6.0) * 2.0 == Db(-12.0)` squares the *amplitude*,
// it does not double the gain. Convert through `Amplitude` for amplitude scaling.
// NOT `Ratio`: a quotient of logarithms is not a quantity.

impl Db {
    /// The metering floor. Silence has no logarithm, so a display needs a
    /// finite stand-in for it; `-inf` cannot be drawn on a fader.
    ///
    /// −144 dB is roughly the noise floor of 24-bit audio, so it is below
    /// anything real while staying finite.
    pub const FLOOR: Db = Db(-144.0);

    /// Unity gain.
    pub const UNITY: Db = Db(0.0);

    /// This gain as a linear amplitude multiplier.
    #[inline]
    #[must_use]
    pub fn to_amplitude(self) -> Amplitude {
        Amplitude(libm::pow(10.0, self.0 / 20.0))
    }

    /// Amplitude as decibels (`20·log10`). **Silence is `-inf`, not
    /// [`FLOOR`](Self::FLOOR)**, the constant a consumer clamps to at its own
    /// boundary. A negative amplitude is NaN.
    ///
    /// No floor is applied. Silence has no logarithm, and what to substitute
    /// for it belongs to the consumer, not to the conversion: a meter wants one
    /// engine-wide floor to bottom out at, a ratio on a ±60 dB scale wants its
    /// own bound, and arithmetic that must round-trip wants no floor at all
    /// (`to_amplitude` of `-inf` is exactly `0.0`, which a pinned floor loses).
    /// A caller that needs a floor pins it at its own boundary, where the
    /// choice is visible.
    ///
    /// `From<Amplitude> for Db` is this method, so `.into()` also yields `-inf`
    /// for silence — reach for it only where that is correct.
    #[inline]
    #[must_use]
    pub fn from_amplitude(amp: Amplitude) -> Db {
        Db(20.0 * libm::log10(amp.0))
    }

    /// A power ratio as decibels (`10·log10`). `power` is a dimensionless
    /// ratio of powers, the square of an amplitude ratio, so `0.5` is
    /// about -3.01 dB.
    ///
    /// Follows the rule of [`from_amplitude`](Self::from_amplitude): zero power
    /// is `-inf`, not [`FLOOR`](Self::FLOOR), the constant a consumer clamps to
    /// at its own boundary, and a negative power is NaN.
    #[inline]
    #[must_use]
    pub fn from_power(power: f64) -> Db {
        Db(10.0 * libm::log10(power))
    }
}

unit! {
    /// A linear gain multiplier.
    ///
    /// **Not** a 0..1 quantity: a +6 dB peak is `Amplitude(2.0)`, and the
    /// dynamics detectors routinely see values above unity. The only floor is
    /// zero — [`SILENT`](Self::SILENT).
    pub struct Amplitude(pub f64): Unit, Ordered, Bounded, Scalable;
}
// NOT `Additive`: cascading two gain stages MULTIPLIES them. Summing
// amplitudes is what mixing two signals does, and that is the signals' job,
// not the gains'.

impl Amplitude {
    /// Silence.
    pub const SILENT: Amplitude = Amplitude(0.0);
    /// Unity gain — the signal passes unchanged.
    pub const UNITY: Amplitude = Amplitude(1.0);

    /// This amplitude in decibels. Silence is `-inf`; see
    /// [`Db::from_amplitude`], which this is.
    #[inline]
    #[must_use]
    pub fn to_db(self) -> Db {
        Db::from_amplitude(self)
    }
}

// Silence converts to `-inf`, per `Db::from_amplitude` — the floor belongs to
// the consumer. A site that needs a finite floor pins it itself rather than
// reaching for `.into()`.
impl From<Amplitude> for Db {
    #[inline]
    fn from(amp: Amplitude) -> Db {
        Db::from_amplitude(amp)
    }
}

impl From<Db> for Amplitude {
    #[inline]
    fn from(db: Db) -> Amplitude {
        db.to_amplitude()
    }
}

unit! {
    /// A signed linear gain: the multiplier on a signal, which may invert it.
    ///
    /// Distinct from [`Amplitude`], which floors at zero and has a decibel
    /// view: a feedback tap, an all-pass coefficient, a routing weight or a
    /// mixing-matrix entry is negative as often as positive, and a negative gain
    /// has no logarithm. A `Gain` is a label on a float, as [`Amplitude`] is:
    /// any float is a gain, and the value a cell holds is what it reads back.
    ///
    /// It composes by multiplication (`Gain * Gain`, signs multiply), sums
    /// (`Gain + Gain`, the gain of two parallel paths) and scales a sample
    /// ([`apply`](Self::apply)).
    ///
    /// The signed multiplier: an all-pass coefficient, a polarity, a routing
    /// weight. A delay's bounded feedback amount is [`Feedback`], not a `Gain`.
    pub struct Gain(pub f64): Unit, Ordered, Bounded, Additive, Signed;
}
// NOT `Scalable`: `Amplitude * f64` scales the amplitude, so a `Gain * f64`
// that applied it to a sample would read differently for the same `x * 0.5`.
// Apply with `apply`; compose gains with `Gain * Gain`.
// NOT `Div` / `Ratio`: a quotient of gains divides by a zero gain. `recip` is
// the one reciprocal, and refuses zero.
// NOT a dB view: a negative gain has no logarithm. Take `to_amplitude` and its
// `Amplitude::to_db` where a level is meant.

impl Gain {
    /// No signal passes.
    pub const SILENT: Gain = Gain(0.0);
    /// The signal passes unchanged.
    pub const UNITY: Gain = Gain(1.0);
    /// The signal passes with its polarity inverted.
    pub const INVERTING: Gain = Gain(-1.0);

    /// This gain as an [`Amplitude`], or `None` when it is negative or NaN.
    #[inline]
    #[must_use]
    pub fn to_amplitude(self) -> Option<Amplitude> {
        if self.0 >= 0.0 {
            Some(Amplitude(self.0.abs()))
        } else {
            None
        }
    }

    /// How loud this gain is, without its sign: `|g|` as an [`Amplitude`].
    /// Total, unlike [`to_amplitude`](Self::to_amplitude), which refuses a
    /// negative gain; a NaN gain has a NaN magnitude.
    #[inline]
    #[must_use]
    pub fn magnitude(self) -> Amplitude {
        Amplitude(self.0.abs())
    }

    /// This gain applied to a sample.
    #[inline]
    #[must_use]
    pub fn apply(self, sample: f64) -> f64 {
        self.0 * sample
    }

    /// The gain that undoes this one (`1 / g`), or `None` when that is not
    /// finite: a zero, a gain so small its reciprocal overflows, or NaN.
    #[inline]
    #[must_use]
    pub fn recip(self) -> Option<Gain> {
        let r = 1.0 / self.0;
        if r.is_finite() { Some(Gain(r)) } else { None }
    }
}

/// Gains in series: signs multiply, magnitudes multiply.
impl core::ops::Mul for Gain {
    type Output = Gain;
    #[inline]
    fn mul(self, rhs: Gain) -> Gain {
        Gain(self.0 * rhs.0)
    }
}

impl core::ops::MulAssign for Gain {
    #[inline]
    fn mul_assign(&mut self, rhs: Gain) {
        self.0 *= rhs.0;
    }
}

// An amplitude is a gain that never inverts: total, lossless, one answer.
impl From<Amplitude> for Gain {
    #[inline]
    fn from(amp: Amplitude) -> Gain {
        Gain(amp.0)
    }
}

#[cfg(test)]
mod tests;
