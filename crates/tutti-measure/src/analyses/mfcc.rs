//! Mel-frequency cepstral coefficients of every frame of a magnitude STFT: the
//! [`Mfcc`] kernel applied row by row.
//!
//! The mel bank is laid over the transform's own bins (`sample_rate / window` apart), so it
//! cannot disagree with the grid, and no second transform is taken.

use core::num::NonZeroUsize;
use tutti_dsp::{
    BinCount, BinIndex, Grid, MelBandCount, MelConfig, MelFilterbank, MelNorm, MelScale, Mfcc,
};

use tutti_units::Hz;

use crate::Result;
use crate::analyses::transform::StftMagnitude;

/// The size and shape of an MFCC analysis.
///
/// By default the [`Htk`](MelScale::Htk) scale, [`Peak`](MelNorm::Peak) triangles and a bank
/// over the whole spectrum, `0 Hz` to Nyquist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MfccConfig {
    bands: MelBandCount,
    coefficients: NonZeroUsize,
    scale: MelScale,
    norm: MelNorm,
    range: Option<(Hz, Hz)>,
}

impl MfccConfig {
    /// `coefficients` cepstral coefficients (at most `bands`) from a bank of `bands` mel
    /// filters.
    #[must_use]
    pub fn new(bands: MelBandCount, coefficients: NonZeroUsize) -> Self {
        Self {
            bands,
            coefficients,
            scale: MelScale::Htk,
            norm: MelNorm::Peak,
            range: None,
        }
    }

    /// The scale the bank's edges are equally spaced in.
    #[must_use]
    pub fn scale(mut self, scale: MelScale) -> Self {
        self.scale = scale;
        self
    }

    /// How each triangle is scaled.
    #[must_use]
    pub fn norm(mut self, norm: MelNorm) -> Self {
        self.norm = norm;
        self
    }

    /// The bank spans `low` to `high` instead of the whole spectrum.
    #[must_use]
    pub fn range(mut self, low: Hz, high: Hz) -> Self {
        self.range = Some((low, high));
        self
    }
}

/// The coefficients of every frame of `transform`, `frames x coefficients` (a
/// [`BinCount`] counts the columns, which are coefficients here).
///
/// # Errors
/// [`Error::Mel`](crate::Error::Mel) when the bank does not fit the transform: a range
/// above Nyquist, or a band that holds no bin (a longer window cures it).
pub fn mfcc(config: &MfccConfig, transform: &StftMagnitude) -> Result<Grid<f64>> {
    let geometry = transform.geometry();
    let mut mel = MelConfig::new(
        config.bands,
        geometry.bins_per_frame(),
        geometry.bin_frequency(BinIndex(1)),
    )
    .scale(config.scale)
    .norm(config.norm);
    if let Some((low, high)) = config.range {
        mel = mel.range(low, high);
    }
    let mut kernel = Mfcc::new(MelFilterbank::try_from(mel)?, config.coefficients);
    let input = transform.magnitudes().grid();
    let mut out = Grid::filled(0.0, input.frames(), BinCount(kernel.coefficients()));
    for frame in input.frames().indices() {
        kernel.process(input.row(frame), out.row_mut(frame));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
