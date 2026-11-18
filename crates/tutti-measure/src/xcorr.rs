//! FFT cross-correlation of two buffers.
//!
//! Both inputs are zero-padded to a power of two that holds the whole linear
//! result and correlated by [`Correlator`], `O(n log n)` rather than the
//! `O(n * lags)` of the direct sum. A caller that wants a weighted variant
//! (phase transform) reshapes the cross-spectrum through [`cross_correlate_shaped`].

use tutti_buffers::{Mono, MonoMut, Sample};
use tutti_types::Samples;

use crate::convert::to_f32;
use tutti_dsp::Complex32;
use tutti_dsp::Correlator;
use tutti_dsp::FftScratch;

/// The linear cross-correlation of two buffers, at every lag they overlap.
///
/// `at(lag)` is `sum_n a[n + lag] * b[n]`, so a **positive lag means `a` is a
/// delayed copy of `b`**. It is not normalised: the zero lag of a buffer with
/// itself is its energy.
///
/// Lags are `isize`: [`Samples`] is unsigned, and a correlation is defined
/// either side of zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CrossCorrelation {
    values: Vec<f32>,
    /// Index of lag zero in `values`: `b.len() - 1`.
    zero: usize,
}

/// The strongest lag of a [`CrossCorrelation`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorrelationPeak {
    /// Where the maximum sits, in whole samples; see [`CrossCorrelation`] for
    /// the sign.
    pub lag: isize,
    /// The correlation there, in the units of the product of the two inputs.
    pub value: f32,
}

impl CrossCorrelation {
    /// The values from the most negative lag to the most positive, one per
    /// lag: `a.len() + b.len() - 1` of them, or none if either input was
    /// empty.
    #[inline]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// The most negative lag held (`-(b.len() - 1)`), or zero when empty.
    #[inline]
    pub fn min_lag(&self) -> isize {
        -(self.zero as isize)
    }

    /// The most positive lag held (`a.len() - 1`), or zero when empty.
    #[inline]
    pub fn max_lag(&self) -> isize {
        self.values.len().saturating_sub(1) as isize + self.min_lag()
    }

    /// The correlation at `lag`, or `None` outside the overlap.
    #[inline]
    pub fn at(&self, lag: isize) -> Option<f32> {
        let index = lag.checked_add(self.zero as isize)?;
        usize::try_from(index)
            .ok()
            .and_then(|i| self.values.get(i))
            .copied()
    }

    /// The largest value over every lag.
    #[inline]
    pub fn peak(&self) -> Option<CorrelationPeak> {
        self.peak_within(Samples(usize::MAX))
    }

    /// The largest value over lags `-max_lag..=max_lag`, the first on a tie.
    ///
    /// `None` when that window holds no lag. A bound beyond the overlap is
    /// clamped to it.
    pub fn peak_within(&self, max_lag: Samples) -> Option<CorrelationPeak> {
        let bound = isize::try_from(max_lag.get()).unwrap_or(isize::MAX);
        let (lo, hi) = (self.min_lag().max(-bound), self.max_lag().min(bound));
        (lo..=hi)
            .filter_map(|lag| self.at(lag).map(|value| CorrelationPeak { lag, value }))
            .fold(None, |best: Option<CorrelationPeak>, next| match best {
                Some(b) if b.value >= next.value => Some(b),
                _ => Some(next),
            })
    }
}

/// Cross-correlates `a` against `b` through the FFT.
///
/// See [`CrossCorrelation`] for the lag convention. Allocates the padded
/// buffers and the result; an offline call.
pub fn cross_correlate<S: Sample>(
    a: Mono<'_, S>,
    b: Mono<'_, S>,
    fft: &mut FftScratch,
) -> CrossCorrelation {
    cross_correlate_shaped(&to_f32(a.samples()), &to_f32(b.samples()), fft, |_| {})
}

/// [`cross_correlate`] with the cross-spectrum `A * conj(B)` handed to `shape`
/// before the inverse transform, for a weighting such as the phase transform.
///
/// `shape` sees `n / 2 + 1` bins, `n` the power of two that holds the whole
/// linear result. A phase transform that divides by the bin magnitude should
/// add an epsilon relative to the spectrum's own level, not an absolute one,
/// so quiet and loud inputs weight alike.
pub(crate) fn cross_correlate_shaped(
    a: &[f32],
    b: &[f32],
    fft: &mut FftScratch,
    shape: impl FnOnce(&mut [Complex32]),
) -> CrossCorrelation {
    if a.is_empty() || b.is_empty() {
        return CrossCorrelation::default();
    }
    let len = a.len() + b.len() - 1;
    // At least 2: a one-point real transform has no distinct Nyquist bin.
    let size = len.next_power_of_two().max(2);
    let mut circular = vec![0.0f32; size];
    Correlator::new().correlate(
        Mono::new(a),
        Mono::new(b),
        fft,
        shape,
        MonoMut::new(&mut circular),
    );

    // Negative lags wrapped to the end of the circular result; lay them out in
    // front of the non-negative ones.
    let zero = b.len() - 1;
    let values = (0..len)
        .map(|i| circular[(i + size - zero) % size])
        .collect();
    CrossCorrelation { values, zero }
}

#[cfg(test)]
mod tests;
