//! Spectral shape features of every frame of a magnitude STFT: the centroid,
//! flatness and rolloff kernels of [`tutti_dsp`] applied row by row.

use tutti_units::Hz;

use crate::analyses::transform::StftMagnitude;
use tutti_dsp::{BinIndex, FrameIndex, spectral_centroid, spectral_flatness, spectral_rolloff};

/// The three features of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectralFeatures {
    /// See [`spectral_centroid`].
    pub centroid: Hz,
    /// See [`spectral_flatness`].
    pub flatness: f64,
    /// See [`spectral_rolloff`].
    pub rolloff: Hz,
}

/// The features of every frame of a magnitude STFT, in frame order.
///
/// `rolloff` is the energy share passed to [`spectral_rolloff`], as described there. The bin width
/// comes from the transform's own geometry, so it cannot disagree with the grid.
pub fn spectral_features(transform: &StftMagnitude, rolloff: f64) -> Vec<SpectralFeatures> {
    let geometry = transform.geometry();
    let bin_width = geometry.bin_frequency(BinIndex(1));
    let grid = transform.magnitudes().grid();
    grid.frames()
        .indices()
        .map(|frame: FrameIndex| {
            let row = grid.row(frame);
            SpectralFeatures {
                centroid: spectral_centroid(row, bin_width),
                flatness: spectral_flatness(row),
                rolloff: spectral_rolloff(row, bin_width, rolloff),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
