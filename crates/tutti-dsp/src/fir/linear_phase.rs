//! Linear-phase FIR design from a target magnitude.
//!
//! A [`LinearPhaseDesign`] takes the product of the magnitudes of a set of [`Lti`] bands
//! (an equalizer's sections, each a closed form) as the target `|H(f)|`, samples it on a dense
//! grid, takes the zero-phase inverse transform, windows it with a Kaiser window and shifts it
//! by `(N - 1) / 2` frames. The taps are an odd-length, symmetric (type I) [`Fir`] whose
//! response is the target smoothed by the window's main lobe and whose phase is exactly
//! linear. Everything here runs on the control side.
//!
//! # The length rule
//!
//! The `resolution` is the width of the window's main lobe: the finest feature of the target
//! the taps resolve. With the stopband attenuation `A` it fixes the length by Kaiser's formula,
//! `N = (A - 7.95) / (2.285 Δω) + 1` for `Δω = 2π · resolution / rate`, rounded up to an odd
//! number ([`FirDesign`](crate::FirDesign) uses the same rule). A finer resolution is a longer filter, and a
//! length past the supported maximum is refused.
//!
//! # The error
//!
//! The windowed response is the target convolved with the window's spectrum `W`, normalized to
//! unit area. For a target twice differentiable, the error at a frequency is, to second order,
//! `½ |H''| μ₂` with `μ₂ = ∫ u² W(u) du = 4 β I₁(β) / (I₀(β) (N - 1)²)` rad², the second
//! moment of the window (the Kaiser window's curvature at its centre); the window's sidelobes
//! add at most about `10^(-A/20)` of the target's peak.

use tutti_units::{Db, Hz, SampleRate};

use super::{Fir, FirError, MAX_ATTENUATION, MAX_LEN, kaiser_beta, kaiser_order, odd_up};
use crate::stft::window::KaiserWindow;
use crate::{Complex64, Lti, RealFft};
use tutti_buffers::MonoMut;

/// The attenuation a design has until [`LinearPhaseDesign::attenuation`] sets it.
const DEFAULT_ATTENUATION: Db = Db::new(60.0);

/// A linear-phase FIR design, described without a sample rate: how finely it resolves the
/// target and how far its window's sidelobes sit down.
///
/// Built as `LinearPhaseDesign::new(resolution)` and the option
/// [`attenuation`](Self::attenuation); [`design`](Self::design) binds a rate and the bands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearPhaseDesign {
    resolution: Hz,
    attenuation: Db,
}

impl LinearPhaseDesign {
    /// A design that resolves features of the target `resolution` wide, at 60 dB of window
    /// attenuation until [`attenuation`](Self::attenuation) sets it.
    #[must_use]
    pub fn new(resolution: Hz) -> Self {
        Self {
            resolution,
            attenuation: DEFAULT_ATTENUATION,
        }
    }

    /// The window's sidelobe attenuation, in positive dB (see
    /// [`FirDesign::attenuation`](crate::FirDesign::attenuation)).
    #[must_use]
    pub fn attenuation(self, attenuation: Db) -> Self {
        Self {
            attenuation,
            ..self
        }
    }

    /// The taps whose magnitude is the product of `bands`' magnitudes at `rate`: each band
    /// is read through [`Lti::frequency_response`] at the grid's frequencies, so the target is
    /// the bands' closed forms, and their phases are discarded. No bands is a flat target of
    /// gain 1.
    ///
    /// # Errors
    ///
    /// - [`FirError::NonPositiveTransition`] or [`FirError::NonPositiveAttenuation`] for a
    ///   resolution or attenuation that is not a positive, finite number.
    /// - [`FirError::AttenuationTooHigh`] past [`MAX_ATTENUATION`].
    /// - [`FirError::TooLong`] when the length would exceed the supported maximum.
    /// - [`FirError::NonFiniteTarget`] when a band has no response to give or answers with
    ///   a value that is not finite.
    pub fn design<L: Lti>(&self, rate: SampleRate, bands: &[L]) -> Result<Fir, FirError> {
        let resolution = self.resolution.get();
        let attenuation = self.attenuation.get();
        if !(resolution.is_finite() && resolution > 0.0) {
            return Err(FirError::NonPositiveTransition(self.resolution));
        }
        if !(attenuation.is_finite() && attenuation > 0.0) {
            return Err(FirError::NonPositiveAttenuation(self.attenuation));
        }
        if attenuation > MAX_ATTENUATION.get() {
            return Err(FirError::AttenuationTooHigh {
                attenuation: self.attenuation,
                max: MAX_ATTENUATION,
            });
        }
        let delta_omega = self.resolution.angle_per_frame(rate).get();
        let len = odd_up(kaiser_order(attenuation, delta_omega));
        if len > MAX_LEN {
            return Err(FirError::TooLong { len, max: MAX_LEN });
        }

        let target = target_magnitude(rate, bands, grid_size(len))?;
        Ok(Fir {
            taps: taps(&target, len, kaiser_beta(self.attenuation)),
        })
    }
}

/// The transform size: a power of two at least four lengths long, so the target's time
/// aliasing lands outside the window.
fn grid_size(len: usize) -> usize {
    (4 * len).next_power_of_two()
}

/// `∏ |H_band(f)|` at the `size / 2 + 1` frequencies `k · rate / size`.
fn target_magnitude<L: Lti>(
    rate: SampleRate,
    bands: &[L],
    size: usize,
) -> Result<Vec<f64>, FirError> {
    let bins = size / 2 + 1;
    let step = rate.get() / size as f64;
    let freqs: Vec<Hz> = (0..bins).map(|k| Hz::new(k as f64 * step)).collect();
    let mut gain = vec![Complex64::new(0.0, 0.0); bins];
    let mut target = vec![1.0; bins];
    for band in bands {
        band.frequency_response(0, &freqs, &mut gain);
        for (t, g) in target.iter_mut().zip(&gain) {
            *t *= g.norm();
        }
    }
    if target.iter().all(|t| t.is_finite()) {
        Ok(target)
    } else {
        Err(FirError::NonFiniteTarget)
    }
}

/// The `len` taps of the zero-phase response `target` (a real, even spectrum on
/// `target.len()` bins of a `2 (target.len() - 1)`-point transform): the right half of the
/// inverse transform, windowed and mirrored so the taps are symmetric bit for bit.
fn taps(target: &[f64], len: usize, beta: f64) -> Vec<f64> {
    let size = 2 * (target.len() - 1);
    let fft = RealFft::<f64>::new(size);
    let mut bins: Vec<Complex64> = target.iter().map(|&m| Complex64::new(m, 0.0)).collect();
    let mut scratch = fft.new_scratch();
    let mut zero_phase = vec![0.0; size];
    fft.inverse(&mut bins, MonoMut::new(&mut zero_phase), &mut scratch);

    let m = (len - 1) / 2;
    let kaiser = KaiserWindow::new(beta);
    // `right[k]` is the tap k samples from the centre.
    let right: Vec<f64> = (0..=m)
        .map(|k| zero_phase[k] / size as f64 * kaiser.centred(k as f64 / (2 * m) as f64))
        .collect();
    let mut taps: Vec<f64> = right.iter().skip(1).rev().copied().collect();
    taps.extend(right);
    taps
}

#[cfg(test)]
mod tests;
