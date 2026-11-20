//! Mel-frequency cepstral coefficients of one magnitude spectrum.
//!
//! The mel energies of the spectrum ([`MelFilterbank::process`]), their natural logarithm,
//! then the orthonormal [`Dct2`], keeping the first coefficients (Davis and Mermelstein,
//! 1980).
//!
//! # The log floor
//!
//! An energy under `1e-12` of the mean band energy counts as that much (-120 dB, the
//! relative floor of [`spectral_flatness`](crate::spectral_flatness)), so an empty band
//! does not send the logarithm to minus infinity, and the floor follows the level: a
//! quiet frame reads the same as a loud one. A silent spectrum, or one with a NaN or
//! infinite magnitude, has no shape and reads all zero.
//!
//! # Properties
//!
//! Scaling the magnitudes by `g` adds `ln(g^2)` to every log energy, which the DCT puts in
//! coefficient 0 alone (`sqrt(N) ln(g^2)` for `N` bands) and leaves the others unchanged,
//! so coefficients 1.. do not depend on level. A bank whose weights are
//! [`Sum`](crate::MelNorm::Sum)-normalised turns a flat spectrum of magnitude `a` into
//! `N` equal energies `a^2`, so its coefficients 1.. are exactly zero.

use core::num::NonZeroUsize;

use tutti_buffers::Float;

use crate::spectral::RELATIVE_POWER_FLOOR;
use crate::{Dct2, MelFilterbank};

/// A mel filterbank, a logarithm and a DCT-II, with the scratch they share.
#[derive(Debug, Clone, PartialEq)]
pub struct Mfcc {
    bank: MelFilterbank,
    dct: Dct2,
    energies: Vec<f64>,
}

impl Mfcc {
    /// The coefficients of `bank`'s bands, keeping the first `coefficients` (at most as many
    /// as the bank has bands).
    #[must_use]
    pub fn new(bank: MelFilterbank, coefficients: NonZeroUsize) -> Self {
        let bands = bank.bands().len();
        Self {
            // A bank has at least one band.
            dct: Dct2::new(
                NonZeroUsize::new(bands).unwrap_or(NonZeroUsize::MIN),
                coefficients,
            ),
            energies: vec![0.0; bands],
            bank,
        }
    }

    /// The filterbank.
    #[must_use]
    pub fn bank(&self) -> &MelFilterbank {
        &self.bank
    }

    /// The coefficients written per spectrum.
    #[must_use]
    pub fn coefficients(&self) -> usize {
        self.dct.outputs()
    }

    /// Writes the coefficients of `magnitudes` into `output`, as many as it has room for up
    /// to [`coefficients`](Self::coefficients), and zero for a spectrum with no shape. The
    /// sums run in `f64` and are narrowed once. Allocates nothing.
    pub fn process<F: Float>(&mut self, magnitudes: &[F], output: &mut [F]) {
        self.bank.process(magnitudes, &mut self.energies);
        let mean = self.energies.iter().sum::<f64>() / self.energies.len() as f64;
        if !mean.is_finite() || mean <= 0.0 {
            output.fill(F::ZERO);
            return;
        }
        let floor = mean * RELATIVE_POWER_FLOOR;
        for e in &mut self.energies {
            *e = libm::log(e.max(floor));
        }
        self.dct.process(&self.energies, output);
    }
}

#[cfg(test)]
mod tests;
