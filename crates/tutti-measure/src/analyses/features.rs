//! Spectral shape features of every frame of a magnitude STFT: the centroid, bandwidth,
//! flatness, crest, slope and rolloff kernels of [`tutti_dsp`] applied row by row.

use tutti_units::{Hz, Normalized};

use crate::analyses::transform::StftMagnitude;
use tutti_dsp::{
    BinIndex, FrameIndex, spectral_bandwidth, spectral_centroid, spectral_crest, spectral_flatness,
    spectral_rolloff, spectral_slope,
};

/// The six features of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectralFeatures {
    /// See [`spectral_centroid`].
    pub centroid: Hz,
    /// See [`spectral_bandwidth`].
    pub bandwidth: Hz,
    /// See [`spectral_flatness`].
    pub flatness: f64,
    /// See [`spectral_crest`].
    pub crest: f64,
    /// See [`spectral_slope`], in magnitude per [`Hz`].
    pub slope: f64,
    /// See [`spectral_rolloff`].
    pub rolloff: Hz,
}

/// The features of every frame of a magnitude STFT, in frame order.
///
/// `rolloff` is the energy share passed to [`spectral_rolloff`], as described there. The bin width
/// comes from the transform's own geometry, so it cannot disagree with the grid.
pub fn spectral_features(rolloff: Normalized, transform: &StftMagnitude) -> Vec<SpectralFeatures> {
    let geometry = transform.geometry();
    let bin_width = geometry.bin_frequency(BinIndex(1));
    let grid = transform.magnitudes().grid();
    grid.frames()
        .indices()
        .map(|frame: FrameIndex| {
            let row = grid.row(frame);
            SpectralFeatures {
                centroid: spectral_centroid(row, bin_width),
                bandwidth: spectral_bandwidth(row, bin_width),
                flatness: spectral_flatness(row),
                crest: spectral_crest(row),
                slope: spectral_slope(row, bin_width),
                rolloff: spectral_rolloff(row, bin_width, rolloff),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
