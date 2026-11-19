//! Spectral shape features: centroid, flatness and rolloff.
//!
//! Each is a pure function of one magnitude spectrum (`DC..=Nyquist`, one
//! value per bin), so it runs over a row of an [`StftMagnitude`] or any
//! magnitudes a caller already holds. A silent spectrum has no shape: every
//! feature reads zero rather than a division by nothing.

use tutti_buffers::Float;
use tutti_units::Hz;

use crate::transform::StftMagnitude;
use tutti_dsp::{BinIndex, FrameIndex};

/// Power below this share of the frame's mean power is treated as this much
/// when taking a logarithm (-120 dB), so empty bins do not send the geometric
/// mean to zero by an amount that depends on the bin count. Relative, so a quiet
/// frame reads the same as a loud one.
const RELATIVE_POWER_FLOOR: f64 = 1e-12;

/// The magnitude-weighted mean frequency: where the spectrum's centre of mass
/// sits, and the usual proxy for brightness.
///
/// `bin_width` is the spacing of the bins in [`Hz`]
/// (`sample_rate / window`). `Hz(0.0)` for a silent spectrum, and for one with
/// a NaN or infinite magnitude.
pub fn spectral_centroid<F: Float>(magnitudes: &[F], bin_width: Hz) -> Hz {
    let (weighted, total) =
        magnitudes
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(w, t), (k, &m)| {
                let m = m.convert_to::<f64>();
                (w + k as f64 * m, t + m)
            });
    if total.is_finite() && total > 0.0 {
        Hz(weighted / total * bin_width.get())
    } else {
        Hz::ZERO
    }
}

/// Wiener entropy: the geometric over the arithmetic mean of the power
/// spectrum, `1` for a flat (noise-like) spectrum and toward `0` for a
/// tonal one. Bins under `1e-12` of the mean power count as that much, so the
/// result depends on the spectrum's shape and not on its level.
///
/// A plain `f64` in `0..=1`, not a unit type: no unit covers a 0..1 spectral
/// ratio. `Confidence` is an estimator's certainty and `Mix` is a blend
/// control, and a new unit needs a distinct range or algebra.
///
/// `0.0` for a silent spectrum, and for one with a NaN or infinite magnitude.
pub fn spectral_flatness<F: Float>(magnitudes: &[F]) -> f64 {
    if magnitudes.is_empty() {
        return 0.0;
    }
    let power = |m: F| {
        let m = m.convert_to::<f64>();
        m * m
    };
    let mean: f64 = magnitudes.iter().map(|&m| power(m)).sum::<f64>() / magnitudes.len() as f64;
    if !mean.is_finite() || mean <= 0.0 {
        return 0.0;
    }
    let floor = mean * RELATIVE_POWER_FLOOR;
    let mean_log: f64 = magnitudes
        .iter()
        .map(|&m| libm::log(power(m).max(floor)))
        .sum::<f64>()
        / magnitudes.len() as f64;
    // Both means in the log domain: exp(mean_log) / mean.
    libm::exp(mean_log - libm::log(mean)).clamp(0.0, 1.0)
}

/// The frequency below which `fraction` of the spectrum's energy lies: the
/// centre of the first bin at which the running sum of squared magnitudes
/// reaches `fraction` of the total.
///
/// `fraction` is a share of a whole in `0..=1` (85 % is the usual choice,
/// `0.85`); a value outside it acts as the nearer end and a NaN as `0.0`. A plain
/// `f64`, not a unit type, for the reason given at [`spectral_flatness`]: no
/// unit covers a 0..1 spectral ratio, and `Mix` is a blend control.
///
/// `Hz(0.0)` for a silent spectrum, and for one with a NaN or infinite
/// magnitude. At `1.0` this is the highest bin that holds any energy.
pub fn spectral_rolloff<F: Float>(magnitudes: &[F], bin_width: Hz, fraction: f64) -> Hz {
    let energy = |m: F| {
        let m = m.convert_to::<f64>();
        m * m
    };
    let total: f64 = magnitudes.iter().map(|&m| energy(m)).sum();
    if !total.is_finite() || total <= 0.0 {
        return Hz(0.0);
    }
    let fraction = if fraction.is_nan() { 0.0 } else { fraction };
    let target = total * fraction;
    let mut running = 0.0f64;
    let mut last_active = 0usize;
    for (k, &m) in magnitudes.iter().enumerate() {
        if m > F::ZERO {
            last_active = k;
        }
        running += energy(m);
        if running >= target {
            break;
        }
    }
    // The bin that crossed the target, or, if the float sum fell a hair short
    // of the total at a full share, the last one that holds energy.
    Hz(last_active as f64 * bin_width.get())
}

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
