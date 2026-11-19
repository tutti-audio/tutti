//! Band edges on a logarithmic frequency axis: equal ratios between neighbours.

use tutti_units::Hz;

/// The `count + 1` edges of `count` bands of equal width on a log axis from
/// `lo` to `hi`: edge `j` is `lo * (hi / lo)^(j / count)`, so the first is `lo`
/// and the last `hi`, up to the rounding of the power.
///
/// Empty for no bands.
///
/// ```
/// use tutti_dsp::log_band_edges;
/// use tutti_units::Hz;
///
/// let edges = log_band_edges(Hz(100.0), Hz(10_000.0), 2);
/// assert_eq!(edges.len(), 3);
/// assert!((edges[1].get() - 1000.0).abs() < 1e-9);
/// ```
#[must_use]
pub fn log_band_edges(lo: Hz, hi: Hz, count: usize) -> Vec<Hz> {
    if count == 0 {
        return Vec::new();
    }
    let (lo, hi) = (lo.get(), hi.get());
    (0..=count)
        .map(|j| Hz(lo * libm::pow(hi / lo, j as f64 / count as f64)))
        .collect()
}

/// The edges of the whole fractional-octave bands that fit from `lo` up to `hi`,
/// `per_octave` to the octave: edge `j` is `lo * 2^(j / per_octave)`, for `j` up
/// to the number of whole bands, `floor(per_octave * log2(hi / lo))`. A band
/// that would end past `hi` is left off, so the last edge is at or below it.
///
/// Empty when not even one band fits, and for `per_octave` of zero.
///
/// ```
/// use tutti_dsp::octave_band_edges;
/// use tutti_units::Hz;
///
/// // Two whole octaves fit in 100..=500 (a third would end at 800).
/// let edges = octave_band_edges(Hz(100.0), Hz(500.0), 1);
/// assert_eq!(edges, [Hz(100.0), Hz(200.0), Hz(400.0)]);
/// ```
#[must_use]
pub fn octave_band_edges(lo: Hz, hi: Hz, per_octave: usize) -> Vec<Hz> {
    let (lo, hi) = (lo.get(), hi.get());
    let n = per_octave as f64;
    let bands = (n * libm::log2(hi / lo) + 1e-9).floor() as usize;
    if per_octave == 0 || bands == 0 {
        return Vec::new();
    }
    (0..=bands)
        .map(|j| Hz(lo * libm::exp2(j as f64 / n)))
        .collect()
}

#[cfg(test)]
mod tests;
