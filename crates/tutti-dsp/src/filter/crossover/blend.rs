//! [`CrossoverBlend`]: the value a crossover's glide moves, the band count and the
//! integrator gain of each split, and the blend of two of them.

use core::f64::consts::SQRT_2;

use tutti_buffers::Float;

use super::bands::MAX_SPLITS;
use crate::filter::integrator_gains;
use crate::{Biquad, Lerp};

/// The damping `k` of every section: `1 / Q` of a Butterworth section, `Q = 1 / sqrt(2)`.
pub(super) const DAMPING: f64 = SQRT_2;

/// One split's section coefficients at one integrator gain `g = tan(pi fc / fs)`: the
/// trapezoidal state-variable filter's `d = 1 - 1 / (1 + g (g + k))`, `g / (1 + g (g + k))`
/// and `g` times that, with `k` [`DAMPING`] (the same set as
/// [`SvfDesign`](crate::SvfDesign)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Tpt {
    pub(super) g: f64,
    pub(super) d: f64,
    pub(super) a2: f64,
    pub(super) a3: f64,
}

impl Tpt {
    /// The coefficients at `g`.
    pub(super) fn new(g: f64) -> Self {
        let [d, a2, a3] = integrator_gains(g, DAMPING);
        Self { g, d, a2, a3 }
    }

    /// Simper's `a1 = 1 / (1 + g (g + k))`.
    fn a1(&self) -> f64 {
        1.0 - self.d
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
        let b0 = self.a1();
        Biquad {
            b0,
            b1: -2.0 * b0,
            b2: b0,
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
            2.0 * (g2 - 1.0) * self.a1(),
            (1.0 - DAMPING * self.g + g2) * self.a1(),
        )
    }
}

/// What a [`CrossoverBank`](super::CrossoverBank)'s glide moves: the band count and
/// the integrator gain of each split, from a [`CrossoverModel`](super::CrossoverModel)
/// ([`blend`](super::CrossoverModel::blend)).
///
/// A [`Lerp`]: a blend moves each split's gain `g` linearly and solves the section
/// coefficients at that `g`, so it is the coefficients of a split at some frequency, and a
/// split's low-pass, high-pass and all-pass stay a Butterworth set whole at every step.
///
/// The band count of a blend is the target's. A crossover keeps its band count for its life
/// (a model holds it, and the node and multiband are built for one), so the splits that
/// blend are the ones active at both ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossoverBlend {
    pub(super) bands: usize,
    pub(super) split: [Tpt; MAX_SPLITS],
}

impl CrossoverBlend {
    /// How many bands they make.
    pub(crate) fn bands(&self) -> usize {
        self.bands
    }
}

impl Lerp for CrossoverBlend {
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
            *o = Tpt::new(crate::lerp(a.g, b.g, t));
        }
        out
    }
}

/// A split's section coefficients in the float `F`: the integrator gains and the damping with
/// its double, what a section runs a frame at.
#[derive(Clone, Copy, Debug)]
pub(super) struct Gains<F> {
    /// `[d, a2, a3]`.
    pub(super) a: [F; 3],
    pub(super) k: F,
    pub(super) two_k: F,
}

impl Tpt {
    /// These coefficients in `F`: the design is in `f64`, narrowed here once.
    pub(super) fn narrowed<F: Float>(&self) -> Gains<F> {
        Gains {
            a: [self.d, self.a2, self.a3].map(F::from_design),
            k: F::from_design(DAMPING),
            two_k: F::from_design(2.0 * DAMPING),
        }
    }
}

/// [`CrossoverBlend`] narrowed to the float `F`, once per design: what a
/// [`CrossoverState<F>`](super::CrossoverState) runs a frame at.
#[derive(Clone, Copy, Debug)]
pub(super) struct Narrowed<F> {
    pub(super) bands: usize,
    pub(super) split: [Gains<F>; MAX_SPLITS],
}

impl CrossoverBlend {
    /// These coefficients in `F`.
    pub(super) fn narrowed<F: Float>(&self) -> Narrowed<F> {
        Narrowed {
            bands: self.bands,
            split: self.split.map(|t| t.narrowed()),
        }
    }
}
