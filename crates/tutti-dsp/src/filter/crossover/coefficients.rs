//! [`CrossoverCoefficients`]: what the sections of a Linkwitz-Riley crossover run at, the
//! integrator gain of each split, and the blend of two of them.

use core::f64::consts::SQRT_2;

use super::bands::MAX_SPLITS;
use crate::filter::integrator_gains;
use crate::{Biquad, Lerp};

/// The damping `k` of every section: `1 / Q` of a Butterworth section, `Q = 1 / sqrt(2)`.
pub(super) const DAMPING: f64 = SQRT_2;

/// One split's section coefficients at one integrator gain `g = tan(pi fc / fs)`: the
/// trapezoidal state-variable filter's `1 / (1 + g (g + k))`, `g` times it and `g` times
/// that, with `k` [`DAMPING`] (the same set as [`SvfCoeffs`](crate::SvfCoeffs)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Tpt {
    pub(super) g: f64,
    pub(super) a1: f64,
    pub(super) a2: f64,
    pub(super) a3: f64,
}

impl Tpt {
    /// The coefficients at `g`.
    pub(super) fn new(g: f64) -> Self {
        let [a1, a2, a3] = integrator_gains(g, DAMPING);
        Self { g, a1, a2, a3 }
    }

    /// The section's low-pass as a biquad (the bilinear transform at `g`).
    pub(super) fn low(&self) -> Biquad {
        let (a1, a2) = self.feedback();
        Biquad {
            b0: self.a3,
            b1: 2.0 * self.a3,
            b2: self.a3,
            a1,
            a2,
        }
    }

    /// The section's high-pass as a biquad.
    pub(super) fn high(&self) -> Biquad {
        let (a1, a2) = self.feedback();
        Biquad {
            b0: self.a1,
            b1: -2.0 * self.a1,
            b2: self.a1,
            a1,
            a2,
        }
    }

    /// The section's all-pass as a biquad.
    pub(super) fn all_pass(&self) -> Biquad {
        let (a1, a2) = self.feedback();
        Biquad {
            b0: a2,
            b1: a1,
            b2: 1.0,
            a1,
            a2,
        }
    }

    /// The denominator `1 + a1 z^-1 + a2 z^-2` shared by the three: `2 (g^2 - 1)` and
    /// `1 - k g + g^2`, each over `1 + k g + g^2`.
    fn feedback(&self) -> (f64, f64) {
        let g2 = self.g * self.g;
        (
            2.0 * (g2 - 1.0) * self.a1,
            (1.0 - DAMPING * self.g + g2) * self.a1,
        )
    }
}

/// The coefficients a [`CrossoverBank`](super::CrossoverBank) runs at: the band count and
/// the integrator gain of each split, from a [`CrossoverModel`](super::CrossoverModel)
/// ([`coefficients`](super::CrossoverModel::coefficients)).
///
/// A [`Lerp`]: a blend moves each split's gain `g` linearly and solves the section
/// coefficients at that `g`, so it is the coefficients of a split at some frequency, and a
/// split's low-pass, high-pass and all-pass stay a Butterworth set whole at every step.
///
/// The band count of a blend is the target's. A crossover keeps its band count for its life
/// (a design holds it, and the node and multiband are built for one), so the splits that
/// blend are the ones active at both ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossoverCoefficients {
    pub(super) bands: usize,
    pub(super) split: [Tpt; MAX_SPLITS],
}

impl CrossoverCoefficients {
    /// How many bands they make.
    pub(crate) fn bands(&self) -> usize {
        self.bands
    }
}

impl Lerp for CrossoverCoefficients {
    /// Each split's `g` blended linearly, `k / n` of the way, and its coefficients solved
    /// there: one division a split, for one frame only.
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = f64::from(k) / f64::from(n);
        let mut out = *to;
        let splits = to.bands.saturating_sub(1);
        for (o, (a, b)) in out
            .split
            .iter_mut()
            .zip(self.split.iter().zip(&to.split))
            .take(splits)
        {
            *o = Tpt::new(a.g + (b.g - a.g) * t);
        }
        out
    }
}
