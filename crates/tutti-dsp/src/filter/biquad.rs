//! The second-order section both the crossover and the SVF's equivalent forms
//! share: its coefficients, its response and its poles.

use tutti_units::{Db, SampleRate, Seconds};

use crate::{Complex64, DecayTime};

/// A second-order section with `a0 = 1`:
/// `H(z) = (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)`.
///
/// A plain value of coefficients. Several structures compute it (the transposed direct form
/// II, a lattice, the trapezoidal state-variable one the SVF and the crossover run). The
/// coefficients are one *realization's* numbers for a response, never the response itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    /// Numerator at `z^0`.
    pub b0: f64,
    /// Numerator at `z^-1`.
    pub b1: f64,
    /// Numerator at `z^-2`.
    pub b2: f64,
    /// Denominator at `z^-1`.
    pub a1: f64,
    /// Denominator at `z^-2`.
    pub a2: f64,
}

impl Biquad {
    /// `H(e^jw)`, evaluated directly from the coefficients. Poorly conditioned
    /// for a pole very near `z = 1` and a frequency near DC (the SVF's own
    /// response reads its trapezoidal form instead).
    #[must_use]
    pub fn response(&self, w: f64) -> Complex64 {
        let z1 = Complex64::from_polar(1.0, -w);
        let z2 = z1 * z1;
        (self.b0 + self.b1 * z1 + self.b2 * z2) / (1.0 + self.a1 * z1 + self.a2 * z2)
    }

    /// The two poles, the roots of `z^2 + a1 z + a2`.
    #[must_use]
    pub fn poles(&self) -> [Complex64; 2] {
        let disc = Complex64::new(self.a1 * self.a1 - 4.0 * self.a2, 0.0).sqrt();
        let half = Complex64::new(-self.a1 / 2.0, 0.0);
        [half + disc / 2.0, half - disc / 2.0]
    }

    /// The larger pole modulus: the slowest decay per frame.
    #[must_use]
    pub fn spectral_radius(&self) -> f64 {
        let [p, q] = self.poles();
        p.norm().max(q.norm())
    }
}

/// `DecayTime` of a system whose slowest pole has modulus `radius` and whose response
/// is `n^(m - 1) radius^n` at its slowest (a pole of multiplicity `m`): the
/// frames until that falls to `floor`, which is the figure of a model that knows
/// only its spectral radius, plus one time constant (`1 / -ln r`): the response
/// peaks some frames in and not at `n = 0`, so a tail measured from the peak must
/// not be reported short. [`DecayTime::Unbounded`] for a radius that is not below 1.
pub(crate) fn decay_time(radius: f64, multiplicity: u32, floor: Db, rate: SampleRate) -> DecayTime {
    if radius.is_nan() || radius >= 1.0 {
        return DecayTime::Unbounded;
    }
    if radius <= 0.0 {
        return DecayTime::Finite(Seconds::new(0.0));
    }
    let eps = libm::log(floor.to_amplitude().get());
    let per_frame = -libm::log(radius);
    let mut frames = eps / -per_frame;
    for _ in 0..3 {
        let poly = f64::from(multiplicity - 1) * libm::log(frames.max(2.0));
        frames = (eps.abs() + poly) / per_frame;
    }
    DecayTime::Finite(Seconds::new(
        (frames.max(0.0) + 1.0 / per_frame) / rate.get(),
    ))
}

/// An upper bound on the l1 norm of the impulse response of one section:
/// `(|b0| + |b1| + |b2|)` times the l1 bound of the all-pole part, which is
/// `min((1 - r)^-2, K / (1 - r))` for `r` its slowest pole and `K = 2r / |p1 - p2|`
/// (the all-pole response is at most `min(n + 1, K) r^n`).
pub(crate) fn section_l1(c: &Biquad) -> f64 {
    let [p, q] = c.poles();
    let r = p.norm().max(q.norm());
    if r.is_nan() || r >= 1.0 {
        return f64::INFINITY;
    }
    let gap = (p - q).norm();
    let k = if gap > 0.0 {
        2.0 * r / gap
    } else {
        f64::INFINITY
    };
    let slack = 1.0 - r;
    let all_pole = (1.0 / (slack * slack)).min(k / slack);
    (c.b0.abs() + c.b1.abs() + c.b2.abs()) * all_pole
}

#[cfg(test)]
mod tests;
