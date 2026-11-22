//! Onset novelty functions: how much one spectrum (or time-domain frame)
//! differs from the one before.
//!
//! Each is a pure function of the current frame and, where it compares, the
//! previous frame's magnitudes; the caller owns the framing and the transform.

use tutti_buffers::{Float, Mono, Sample};

use crate::Complex;

/// Positive-rectified sum of magnitude increases.
///
/// Only *growth* counts: a bin that quietens contributes nothing, which is
/// what makes this an onset detector rather than a change detector.
pub fn spectral_flux<F: Float>(previous: &[F], current: &[Complex<F>]) -> f64 {
    current
        .iter()
        .zip(previous)
        .map(|(&bin, &prev)| (bin_magnitude(bin) - prev.convert_to::<f64>()).max(0.0))
        .sum()
}

/// A bin's modulus `hypot(re, im)`, widened to `f64` and computed by `libm`, so
/// it is the same on every platform (the standard library's `hypot` is not).
#[must_use]
pub fn bin_magnitude<F: Float>(bin: Complex<F>) -> f64 {
    libm::hypot(bin.re.convert_to::<f64>(), bin.im.convert_to::<f64>())
}

/// A bin's squared magnitude, taken at `f64`.
fn power<F: Float>(bin: Complex<F>) -> f64 {
    let (re, im) = (bin.re.convert_to::<f64>(), bin.im.convert_to::<f64>());
    re * re + im * im
}

/// Magnitude energy weighted by bin index.
///
/// Bright transients concentrate energy at high frequencies, so weighting by
/// bin favours them over low-frequency rumble.
pub fn high_frequency_content<F: Float>(current: &[Complex<F>]) -> f64 {
    current
        .iter()
        .enumerate()
        .map(|(i, &bin)| (i + 1) as f64 * power(bin))
        .sum::<f64>()
        .sqrt()
}

/// Broadband level of a time-domain frame: its L2 norm, the square root of
/// the summed squares.
///
/// Not an RMS: the sum is not divided by the frame's length, so the value
/// grows with the window (by `sqrt(len)` for a steady signal). Onset
/// detection compares frames of one length, where the scale cancels.
pub fn spectral_energy<S: Sample>(frame: Mono<'_, S>) -> f64 {
    frame
        .samples()
        .iter()
        .map(|s| {
            let v = s.convert_to::<f64>();
            v * v
        })
        .sum::<f64>()
        .sqrt()
}

/// Euclidean distance between successive magnitude spectra:
/// `sqrt(sum((|X_k| - previous_k)^2))`.
///
/// Unlike [`spectral_flux`] this counts decreases too, which is what makes it
/// sensitive to tonal onsets where energy moves between bins rather than
/// simply arriving. It compares magnitudes only (`previous` holds the last
/// frame's magnitudes); no phase enters it.
pub fn spectral_distance<F: Float>(previous: &[F], current: &[Complex<F>]) -> f64 {
    current
        .iter()
        .zip(previous)
        .map(|(&bin, &prev)| {
            let diff = bin_magnitude(bin) - prev.convert_to::<f64>();
            diff * diff
        })
        .sum::<f64>()
        .sqrt()
}

#[cfg(test)]
mod tests;
