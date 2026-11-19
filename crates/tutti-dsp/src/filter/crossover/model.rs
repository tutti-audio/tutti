//! The Linkwitz-Riley crossover as a model: [`CrossoverModel`], the design
//! (the splits' coefficients at a rate), separated from the state that runs it
//! ([`CrossoverBank`](super::CrossoverBank)).
//!
//! A band of an `n`-split tree is `HP^2` of every earlier split, `LP^2` of its
//! own and the all-pass of every later one (see `state.rs`), so each band's
//! response is the product of those, in closed form.

use tutti_units::{Db, Hz, SampleRate, Samples};

use super::bands::{CrossoverError, MAX_BANDS, MAX_SPLITS, SOLVE_CEILING, check_bands, validate};
use super::design::Tpt;
use super::{CrossoverControls, CrossoverDesign};
use crate::design::prewarp;
use crate::filter::biquad::{decay_ring, section_l1};
use crate::{Complex64, Lti, Ring, System};

/// The sections' coefficients of the split at `hz` against `rate`, the frequency held under
/// [`SOLVE_CEILING`] of the rate.
fn solve_split(hz: Hz, rate: SampleRate) -> Tpt {
    let hz = Hz::new(hz.get().min(SOLVE_CEILING * rate.get()));
    Tpt::new(prewarp(hz, rate))
}

/// The design of a Linkwitz-Riley crossover: its sample rate, the frequencies its
/// coefficients are solved at and those coefficients. A value without state; the
/// crossover that runs it keeps the integrators and the glide between two of these
/// ([`design`](Self::design)).
///
/// It is an [`Lti`] with one input and one output a band, lowest first: the bands
/// are at -6.02 dB at their crossover and sum to unit magnitude.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossoverModel {
    pub(super) rate: SampleRate,
    pub(super) bands: usize,
    pub(super) freqs: [Hz; MAX_SPLITS],
    pub(super) coeffs: [Tpt; MAX_SPLITS],
}

impl CrossoverModel {
    /// The most bands.
    pub const MAX_BANDS: usize = MAX_BANDS;

    /// A model with a split at each of `frequencies` at `rate`, so
    /// `frequencies.len() + 1` bands.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`]: a band count
    /// outside 2 to 6, a rate that is not finite and positive, or frequencies that
    /// are not finite, above 0, strictly increasing and below Nyquist.
    pub fn new(frequencies: &[Hz], rate: impl Into<SampleRate>) -> Result<Self, CrossoverError> {
        let rate = rate.into();
        validate(frequencies, rate)?;
        let mut model = Self::empty(frequencies.len() + 1, rate);
        model.freqs[..frequencies.len()].copy_from_slice(frequencies);
        model.solve();
        Ok(model)
    }

    /// The model of `controls` at `rate`, `bands` bands wide. Total in the controls (see
    /// [`design_into`](Self::design_into)).
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Bands`] outside 2 to 6.
    pub fn designed(
        bands: usize,
        controls: &CrossoverControls,
        rate: SampleRate,
    ) -> Result<Self, CrossoverError> {
        check_bands(bands)?;
        let mut model = Self::empty(bands, rate);
        model.design_into(controls, rate);
        Ok(model)
    }

    /// Designs `controls` at `rate` in place, keeping the band count. Total: a frequency is
    /// held between the split below it (1 Hz for the first) and what the coefficients can
    /// be solved at ([`SOLVE_CEILING`] of the rate), a NaN reading as the floor, so a set
    /// that is not increasing is solved with its splits no lower than the one before.
    /// [`CrossoverControls::new`] is what refuses one. Allocates nothing.
    pub fn design_into(&mut self, controls: &CrossoverControls, rate: SampleRate) {
        self.rate = rate;
        let cap = SOLVE_CEILING * rate.get();
        let mut floor = 1.0f64;
        for (f, hz) in self.freqs.iter_mut().zip(&controls.frequencies) {
            floor = hz.get().max(floor).min(cap);
            *f = Hz::new(floor);
        }
        self.solve();
    }

    /// A model of `bands` bands at `rate` before any design: every split at 1 kHz, with
    /// the coefficients of 1 kHz at 48 kHz. [`design_into`](Self::design_into) is what sets them.
    fn empty(bands: usize, rate: SampleRate) -> Self {
        let split = solve_split(Hz::new(1_000.0), SampleRate::SR_48K);
        Self {
            rate,
            bands,
            freqs: [Hz::new(1_000.0); MAX_SPLITS],
            coeffs: [split; MAX_SPLITS],
        }
    }

    /// Solves every split's coefficients at `self.freqs`.
    fn solve(&mut self) {
        for j in 0..self.splits() {
            self.coeffs[j] = solve_split(self.freqs[j], self.rate);
        }
    }

    /// The coefficients the sections run at: each split's integrator gain `tan(pi f / rate)`.
    #[must_use]
    pub fn design(&self) -> CrossoverDesign {
        CrossoverDesign {
            bands: self.bands,
            split: self.coeffs,
        }
    }

    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Crossover `i`'s frequency as solved, or `None` past the last.
    #[must_use]
    pub fn frequency(&self, i: usize) -> Option<Hz> {
        (i < self.splits()).then(|| self.freqs[i])
    }

    /// The sample rate the coefficients are solved at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.rate
    }

    fn splits(&self) -> usize {
        self.bands - 1
    }
}

impl System for CrossoverModel {
    /// Zero: the bands are not delayed.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The slowest split's pole ring, with the double pole of its squared
    /// sections (`n r^n`).
    fn tail(&self, floor: Db) -> Ring {
        decay_ring(self.spectral_radius(), 2, floor, self.sample_rate())
    }
}

impl Lti for CrossoverModel {
    fn outputs(&self) -> usize {
        self.bands
    }

    /// Band `output`: `HP^2` of every split before it, `LP^2` of its own and the
    /// all-pass of every split after. NaN past the last band.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        for (g, f) in gain.iter_mut().zip(freqs) {
            *g = if output < self.bands {
                let w = f.angle_per_frame(self.rate).get();
                let mut h = Complex64::new(1.0, 0.0);
                for (j, c) in self.coeffs[..self.splits()].iter().enumerate() {
                    if j < output {
                        let hp = c.high().response(w);
                        h *= hp * hp;
                    } else if j == output {
                        let lp = c.low().response(w);
                        h *= lp * lp;
                    } else {
                        h *= c.all_pass().response(w);
                    }
                }
                h
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The slowest section's pole radius (every section of one split shares its
    /// poles).
    fn spectral_radius(&self) -> f64 {
        self.coeffs[..self.splits()]
            .iter()
            .map(|c| c.low().spectral_radius())
            .fold(0.0, f64::max)
    }

    /// The largest band's product of its sections' l1 bounds.
    fn l1_norm(&self) -> f64 {
        let n = self.splits();
        (0..self.bands)
            .map(|band| {
                (0..n)
                    .map(|j| {
                        let c = &self.coeffs[j];
                        match j.cmp(&band) {
                            std::cmp::Ordering::Less => section_l1(&c.high()).powi(2),
                            std::cmp::Ordering::Equal => section_l1(&c.low()).powi(2),
                            std::cmp::Ordering::Greater => section_l1(&c.all_pass()),
                        }
                    })
                    .product::<f64>()
            })
            .fold(0.0, f64::max)
    }

    /// 1: every band is the product of Butterworth low-passes and high-passes
    /// (each under 1 everywhere, so each squared too) and all-passes (exactly 1),
    /// and the band that holds DC (or Nyquist) reaches it.
    fn hinf_norm(&self) -> f64 {
        1.0
    }
}

#[cfg(test)]
mod tests;
