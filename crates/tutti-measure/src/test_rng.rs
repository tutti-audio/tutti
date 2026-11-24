//! The seeded draws and signals the unit tests share.

pub(crate) use tutti_fixtures::uniform;

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
