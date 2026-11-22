//! Spectral shape of one magnitude spectrum: centroid, bandwidth, flatness, crest, slope and
//! rolloff.
//!
//! Each is a pure function of one magnitude spectrum (`DC..=Nyquist`, one
//! value per bin), so it runs over a row of a magnitude STFT or any magnitudes
//! a caller already holds. A silent spectrum has no shape: every feature reads
//! zero rather than a division by nothing.

use tutti_buffers::Float;
use tutti_units::{Hz, Normalized};

/// Power below this share of the frame's mean power is treated as this much
/// when taking a logarithm (-120 dB), so empty bins do not send the geometric
/// mean to zero by an amount that depends on the bin count. Relative, so a quiet
/// frame reads the same as a loud one.
pub(crate) const RELATIVE_POWER_FLOOR: f64 = 1e-12;

/// The magnitude-weighted mean frequency: where the spectrum's centre of mass
/// sits, and the usual proxy for brightness.
///
/// `bin_width` is the spacing of the bins in [`Hz`]
/// (`sample_rate / window`). `Hz::new(0.0)` for a silent spectrum, and for one with
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
        Hz::new(weighted / total * bin_width.get())
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
/// `fraction` is a [`Normalized`] share of a whole (85 % is the usual choice,
/// `Normalized::of(0.85)`): a value outside `0..=1` or a NaN is refused when the
/// `Normalized` is built, so no share is read as another.
///
/// `Hz::new(0.0)` for a silent spectrum, and for one with a NaN or infinite
/// magnitude. At `1.0` this is the highest bin that holds any energy.
pub fn spectral_rolloff<F: Float>(magnitudes: &[F], bin_width: Hz, fraction: Normalized) -> Hz {
    let energy = |m: F| {
        let m = m.convert_to::<f64>();
        m * m
    };
    let total: f64 = magnitudes.iter().map(|&m| energy(m)).sum();
    if !total.is_finite() || total <= 0.0 {
        return Hz::new(0.0);
    }
    let target = total * fraction.get();
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
    Hz::new(last_active as f64 * bin_width.get())
}

/// The magnitudes' sum, when it is finite and above zero: the spectrum has a shape only then.
fn magnitude_total<F: Float>(magnitudes: &[F]) -> Option<f64> {
    let total: f64 = magnitudes.iter().map(|&m| m.convert_to::<f64>()).sum();
    (total.is_finite() && total > 0.0).then_some(total)
}

/// The magnitude-weighted spread of the spectrum about its centroid: the order-2 spectral
/// bandwidth, `sqrt(sum(m_k (f_k - c)^2) / sum(m_k))` with `c` the
/// [`spectral_centroid`] (Klapuri and Davy, 2006, the spectral spread; librosa's
/// `spectral_bandwidth` at `p = 2`). It is `0` for one bin and grows as the energy spreads
/// away from its centre of mass.
///
/// `bin_width` is the spacing of the bins in [`Hz`]. The bandwidth does not depend on the
/// spectrum's level. `Hz::new(0.0)` for a silent spectrum, and for one with a NaN or infinite
/// magnitude.
pub fn spectral_bandwidth<F: Float>(magnitudes: &[F], bin_width: Hz) -> Hz {
    let Some(total) = magnitude_total(magnitudes) else {
        return Hz::ZERO;
    };
    let centre = spectral_centroid(magnitudes, Hz::new(1.0)).get();
    let spread: f64 = magnitudes
        .iter()
        .enumerate()
        .map(|(k, &m)| {
            let d = k as f64 - centre;
            m.convert_to::<f64>() * d * d
        })
        .sum();
    Hz::new(libm::sqrt(spread / total) * bin_width.get())
}

/// The peak of the spectrum over its mean: `max(m_k) / mean(m_k)` (Peeters, 2004, the spectral
/// crest, taken over the whole band). `1` for a flat spectrum and the number of bins for a
/// single bin, so it is a ratio in `1..=len`.
///
/// A plain `f64`, for the reason given at [`spectral_flatness`]: no unit covers a ratio of
/// magnitudes (`CompressionRatio` is a compressor's setting, and its range is not a measurement's).
/// It does not depend on the spectrum's level.
///
/// `0.0` for a silent or empty spectrum, and for one with a NaN or infinite magnitude.
pub fn spectral_crest<F: Float>(magnitudes: &[F]) -> f64 {
    let Some(total) = magnitude_total(magnitudes) else {
        return 0.0;
    };
    let peak = magnitudes
        .iter()
        .map(|&m| m.convert_to::<f64>())
        .fold(0.0f64, f64::max);
    peak * magnitudes.len() as f64 / total
}

/// The slope of the least-squares line through magnitude against frequency, in magnitude
/// per [`Hz`] (the regression of Peeters, 2004, the spectral slope, without his division by the
/// magnitude sum): `sum((f_k - mean f)(m_k - mean m)) / sum((f_k - mean f)^2)`. Negative for a
/// spectrum that falls with frequency, as most audio does.
///
/// A plain `f64`: the unit is the magnitudes' own per hertz, which no unit type covers. It
/// scales with the spectrum's level (dividing by the magnitude sum gives Peeters's level-free
/// form).
///
/// `0.0` for fewer than two bins, a silent spectrum, and one with a NaN or infinite magnitude.
pub fn spectral_slope<F: Float>(magnitudes: &[F], bin_width: Hz) -> f64 {
    let n = magnitudes.len();
    if n < 2 || magnitude_total(magnitudes).is_none() {
        return 0.0;
    }
    let mean_k = (n - 1) as f64 / 2.0;
    let mean_m = magnitudes
        .iter()
        .map(|&m| m.convert_to::<f64>())
        .sum::<f64>()
        / n as f64;
    let (cross, spread) =
        magnitudes
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(c, s), (k, &m)| {
                let d = k as f64 - mean_k;
                (c + d * (m.convert_to::<f64>() - mean_m), s + d * d)
            });
    cross / spread / bin_width.get()
}

#[cfg(test)]
mod tests;
