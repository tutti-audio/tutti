// Ballistics: the attack and release of a one-pole follower.

use super::*;

/// The attack and release times of a follower on a quantity that rises toward a target and
/// falls back: a meter's face, a spectrum's averaging, a compressor's gain.
///
/// Both times are finite and not negative; [`new`](Self::new) refuses anything else. A time of 0
/// is instant. The pair is the times only: the one-pole step each follower runs is its own,
/// and [`coeffs`](Self::coeffs) and [`coeff_over`](Self::coeff_over) are the coefficients it
/// steps with, as `current + c * (target - current)`.
///
/// ```
/// use tutti_units::{Ballistics, SampleRate, Seconds};
///
/// let meter = Ballistics::new(Seconds::new(0.01), Seconds::new(0.1)).unwrap();
/// let (attack, release) = meter.coeffs(SampleRate::of(48_000.0));
/// assert!(attack > release, "the shorter time closes more of the gap per sample");
/// assert!(Ballistics::new(Seconds::new(-1.0), Seconds::new(0.1)).is_none());
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "(f64, f64)", into = "(f64, f64)")
)]
pub struct Ballistics {
    attack: Seconds,
    release: Seconds,
}

/// A time that is finite and not negative.
fn valid(t: Seconds) -> bool {
    t.get().is_finite() && t.get() >= 0.0
}

impl Ballistics {
    /// The pair of `attack` and `release`, or `None` when either is negative, NaN or infinite.
    /// Never clamps.
    #[inline]
    #[must_use]
    pub fn new(attack: Seconds, release: Seconds) -> Option<Self> {
        if valid(attack) && valid(release) {
            Some(Self { attack, release })
        } else {
            None
        }
    }

    /// The time a rising quantity takes to close `1 - 1/e` of the gap.
    #[inline]
    #[must_use]
    pub fn attack(self) -> Seconds {
        self.attack
    }

    /// The time a falling quantity takes to close `1 - 1/e` of the gap.
    #[inline]
    #[must_use]
    pub fn release(self) -> Seconds {
        self.release
    }

    /// The one-pole coefficients `(attack, release)` of a smoother stepped once per sample at
    /// `rate`: [`Seconds::one_pole_coeff`] of each time, `1 - exp(-1 / (t * rate))`, 1 for a
    /// time of 0.
    ///
    /// The coefficients are bare `f64`, which no unit covers (a fraction of the gap closed
    /// per step). Design coefficients, in `f64`; a bank narrows them once.
    #[inline]
    #[must_use]
    pub fn coeffs(self, rate: SampleRate) -> (f64, f64) {
        (
            self.attack.one_pole_coeff(rate),
            self.release.one_pole_coeff(rate),
        )
    }

    /// The coefficient of the attack (`rising`) or the release, for a smoother stepped once
    /// per `dt` of audio time: [`Seconds::one_pole_coeff_over`]. For a smoother stepped once
    /// per block or per display frame. A time of 0 is 1, and a `dt` that is not above zero
    /// with a positive time moves nothing: 0. A design coefficient, in `f64`; a bank narrows it
    /// once.
    #[inline]
    #[must_use]
    pub fn coeff_over(self, rising: bool, dt: Seconds) -> f64 {
        let time = if rising { self.attack } else { self.release };
        time.one_pole_coeff_over(dt)
    }
}

/// The wire form of [`Ballistics`] is the pair `(attack, release)` in seconds, read back
/// through [`Ballistics::new`], so a document cannot hold a refused time.
#[cfg(feature = "serde")]
impl From<Ballistics> for (f64, f64) {
    fn from(b: Ballistics) -> Self {
        (b.attack.get(), b.release.get())
    }
}

#[cfg(feature = "serde")]
impl TryFrom<(f64, f64)> for Ballistics {
    type Error = &'static str;

    fn try_from((attack, release): (f64, f64)) -> Result<Self, Self::Error> {
        Self::new(Seconds::new(attack), Seconds::new(release))
            .ok_or("an attack and release must be finite and not negative")
    }
}

#[cfg(test)]
mod tests;
