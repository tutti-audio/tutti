//! The root-mean-square level of a block.

use tutti_buffers::{Mono, Sample};
use tutti_types::Amplitude;

/// The root-mean-square level of `x`: the square root of the mean of the
/// squares, `sqrt(sum(x^2) / n)`. A full-scale sine reads `1/sqrt(2)` and a
/// constant `c` reads `|c|`. An empty block is [`Amplitude::SILENT`].
///
/// The squares are summed in `f64`, so a long block does not lose its low bits
/// to the running total. A NaN or an infinity in `x` gives a NaN or an infinity.
///
/// ```
/// use tutti_buffers::Mono;
/// use tutti_dsp::rms;
///
/// assert_eq!(rms(Mono::new(&[0.5f32; 64])).get(), 0.5);
/// ```
#[must_use]
pub fn rms<S: Sample>(x: Mono<'_, S>) -> Amplitude {
    let samples = x.samples();
    if samples.is_empty() {
        return Amplitude::SILENT;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| {
            let v = s.to_f64();
            v * v
        })
        .sum();
    Amplitude(libm::sqrt(sum / samples.len() as f64) as f32)
}

#[cfg(test)]
mod tests;
