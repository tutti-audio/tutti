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
mod tests {
    use super::*;
    use core::f64::consts::TAU;

    /// A whole number of periods of a full-scale sine has mean square 1/2, so
    /// RMS `1/sqrt(2)`.
    #[test]
    fn a_full_scale_sine_reads_one_over_root_two() {
        let n = 4800;
        let sine: Vec<f64> = (0..n)
            .map(|i| libm::sin(TAU * 10.0 * i as f64 / n as f64))
            .collect();
        let got = rms(Mono::new(&sine)).get();
        assert!(
            (f64::from(got) - core::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6,
            "{got}"
        );
    }

    /// A constant block reads its magnitude, whatever its sign.
    #[test]
    fn a_constant_reads_its_magnitude() {
        assert_eq!(rms(Mono::new(&[0.5f32; 100])), Amplitude(0.5));
        assert_eq!(rms(Mono::new(&[-0.25f32; 7])), Amplitude(0.25));
    }

    /// There is nothing to average, and no level.
    #[test]
    fn an_empty_block_is_silent() {
        assert_eq!(rms(Mono::new(&[] as &[f32])), Amplitude::SILENT);
    }
}
