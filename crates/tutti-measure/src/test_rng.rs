//! The seeded draws and signals the unit tests share.

use tutti_buffers::Float;
use tutti_rt::SplitMix64;

/// The next value of `rng`, uniform over `[lo, hi)` on the `2^-MANTISSA_DIGITS` grid.
pub(crate) fn uniform<F: Float>(rng: &mut SplitMix64, lo: F, hi: F) -> F {
    lo + (hi - lo) * F::from_fraction_bits(rng.next_u64())
}

/// `len` samples of the sum of unit sines at `freqs` hertz, at `rate` hertz,
/// scaled to peak under one.
pub(crate) fn tones(rate: f64, freqs: &[f64], len: usize) -> Vec<f64> {
    (0..len)
        .map(|n| {
            freqs
                .iter()
                .map(|f| libm::sin(core::f64::consts::TAU * f * n as f64 / rate))
                .sum::<f64>()
                / (freqs.len() as f64 + 1.0)
        })
        .collect()
}
