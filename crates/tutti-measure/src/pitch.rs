//! The YIN pitch-detection engine — **crate-internal**.
//!
//! The public pitch surface is [`yin`](fn@crate::yin)'s: `yin`, `yin_track`,
//! `Pitch`, `PitchEstimate`, `YinConfig`. This module is what that one is built
//! from, and it is deliberately not exported — two parallel pitch APIs on one
//! crate would be a choice a caller has no basis to make. `PitchDetector` holds
//! the reusable difference/autocorrelation buffers; `yin.rs` builds one per
//! call in `detector()` and drives it through `estimate()`.
//!
//! ## Algorithm
//!
//! The YIN algorithm (de Cheveigné & Kawahara, 2002) is a robust
//! autocorrelation-based pitch detector. This implementation covers
//! steps 1 to 5 of the paper (autocorrelation through parabolic interpolation),
//! grouped here as four stages, and omits step 6, the best-local-estimate
//! search over neighbouring frames:
//!
//! 1. **Difference function** - d(τ) = Σ(x\[j\] - x\[j+τ\])²
//! 2. **Cumulative mean normalized difference** - d'(τ)
//! 3. **Absolute threshold** - Find first τ where d'(τ) < threshold
//! 4. **Parabolic interpolation** - Sub-sample accuracy
//!
//! ## Performance
//!
//! Uses FFT-based autocorrelation via Wiener-Khinchin theorem for O(n log n)
//! computation: r(τ) = IFFT(|FFT(x)|²)

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PitchResult {
    /// The detected pitch in [`Hz`]; `Hz(0.0)` if unvoiced.
    pub frequency: Hz,
    /// Detection strength as a [`Confidence`] reading, `0.0..=1.0`. A
    /// measurement, not a blend — deliberately not `Mix`.
    pub confidence: Confidence,
}

impl PitchResult {
    /// Whether this reading is a real pitch: both a positive frequency and a
    /// non-zero [`Confidence`]. A default (unvoiced) result is neither.
    pub fn is_voiced(&self) -> bool {
        self.frequency > Hz(0.0) && self.confidence > Confidence(0.0)
    }
}

use tutti_units::{Confidence, Hz};

use tutti_buffers::{Mono, MonoMut};
use tutti_dsp::Correlator;
use tutti_dsp::FftScratch;
use tutti_dsp::parabolic_peak;

use crate::yin::YinPlan;

/// YIN pitch detector (de Cheveigné & Kawahara, 2002).
///
/// Uses FFT-based autocorrelation for O(n log n) performance. Everything a
/// call needs is sized and planned at construction, so [`detect`](Self::detect)
/// allocates nothing and a track reuses one detector for every frame.
pub(crate) struct PitchDetector {
    /// The validated range, rate and threshold this detector runs at.
    cfg: YinPlan,
    difference: Vec<f32>,
    cumulative_mean: Vec<f32>,
    fft: FftScratch,
    correlator: Correlator,
    /// The circular cross-correlation `r(τ) = Σ_j x[j]*x[j+τ]`, `fft_size`
    /// points.
    correlation: Vec<f32>,
    /// Prefix sums of the squared input, for the windowed energies.
    cum_sq: Vec<f64>,
}

impl PitchDetector {
    pub fn new(cfg: YinPlan) -> Self {
        let max_period = cfg.max_period().get();
        let fft_size = (max_period * 2).next_power_of_two();
        let mut detector = Self {
            cfg,
            difference: vec![0.0; max_period + 1],
            cumulative_mean: vec![0.0; max_period + 1],
            fft: FftScratch::new(),
            correlator: Correlator::with_capacity(fft_size),
            correlation: vec![0.0; fft_size],
            // `usable` is at most `2 * max_period` (see `compute_difference`).
            cum_sq: vec![0.0; 2 * max_period + 1],
        };
        // Plans the transform, so a detect call allocates nothing.
        detector.correlator.correlate(
            Mono::new(&[]),
            Mono::new(&[]),
            &mut detector.fft,
            |_| {},
            MonoMut::new(&mut detector.correlation),
        );
        detector
    }

    /// Needs at least `buffer_size()` samples.
    pub(crate) fn detect(&mut self, samples: &[f32]) -> PitchResult {
        let (min_period, max_period) = self.periods(samples.len());

        if samples.len() < max_period * 2 || max_period <= min_period {
            return PitchResult::default();
        }

        self.compute_difference(samples, max_period);
        self.compute_cumulative_mean(max_period);
        let (period, aperiodicity) = self.find_best_period_full(min_period, max_period);

        if period == 0 {
            return PitchResult::default();
        }

        let refined_period = self.parabolic_interpolation(period, max_period);
        let frequency = Hz((self.cfg.sample_rate().get() / refined_period) as f32);
        // Clamped rather than wrapped raw: `Confidence` is a 0..=1 reading, and
        // this is the one place the aperiodicity inversion could leave the range.
        let confidence = Confidence::new_clamped(1.0 - aperiodicity);

        PitchResult {
            frequency,
            confidence,
        }
    }

    /// The lag range one call searches: the config's, the longest capped by
    /// what `len` samples can hold.
    fn periods(&self, len: usize) -> (usize, usize) {
        let max_period = self
            .cfg
            .max_period()
            .get()
            .min(len / 2)
            .min(self.difference.len() - 1);
        (self.cfg.min_period().get(), max_period)
    }

    /// YIN steps 1-2: `d(τ) = r_x(0,W) + r_x(τ,W) - 2*autocorr(τ)`, with the
    /// autocorrelation taken by FFT via Wiener-Khinchin:
    /// `r(τ) = IFFT(|FFT(x)|²)`.
    ///
    /// `r(τ) = Σ_{j=0}^{W-1} x[j]*x[j+τ]` is the cross-correlation of
    /// `x[0..W+max_period]` against `x[0..W]`, taken by the [`Correlator`].
    fn compute_difference(&mut self, samples: &[f32], max_period: usize) {
        let window = max_period;
        let usable = samples.len().min(window + max_period);

        // Cumulative squared energy for running window sums
        let cum_sq = &mut self.cum_sq;
        cum_sq[0] = 0.0;
        for i in 0..usable {
            cum_sq[i + 1] = cum_sq[i] + (samples[i] as f64) * (samples[i] as f64);
        }
        let cum_sq = &*cum_sq;

        let energy = |start: usize, len: usize| -> f64 {
            let end = (start + len).min(usable);
            if start < usable {
                cum_sq[end] - cum_sq[start]
            } else {
                0.0
            }
        };

        // r(τ) = Σ_j a[j]*b[j+τ] with a = x[0..W] and b = x[0..W+max_period]:
        // `b` against `a`. The transform holds `2 * max_period` points, so the
        // lags `0..=max_period` read here are free of alias.
        let a_len = window.min(usable);
        self.correlator.correlate(
            Mono::new(&samples[..usable]),
            Mono::new(&samples[..a_len]),
            &mut self.fft,
            |_| {},
            MonoMut::new(&mut self.correlation),
        );

        self.difference[0] = 0.0;

        let e0 = energy(0, window);
        for tau in 1..=max_period {
            let autocorr = f64::from(self.correlation[tau]);
            let e_tau = energy(tau, window);
            self.difference[tau] = (e0 + e_tau - 2.0 * autocorr).max(0.0) as f32;
        }
    }

    /// YIN step 3: d'(τ) = d(τ) / ((1/τ) * Σ d(j)), d'(0) = 1
    fn compute_cumulative_mean(&mut self, max_period: usize) {
        self.cumulative_mean[0] = 1.0;

        let mut running_sum = 0.0f32;
        for tau in 1..=max_period {
            running_sum += self.difference[tau];
            if running_sum > 1e-10 {
                self.cumulative_mean[tau] = self.difference[tau] * tau as f32 / running_sum;
            } else {
                self.cumulative_mean[tau] = 1.0;
            }
        }
    }

    /// YIN step 4: return the FIRST local minimum below threshold (not the global
    /// minimum) to prevent octave errors from subharmonic detection.
    /// Returns (period, aperiodicity).
    fn find_best_period_full(&self, min_period: usize, max_period: usize) -> (usize, f32) {
        let mut tau = min_period;

        while tau < max_period {
            if self.cumulative_mean[tau] < self.cfg.threshold.get() {
                // Walk to the local minimum
                while tau + 1 < max_period
                    && self.cumulative_mean[tau + 1] < self.cumulative_mean[tau]
                {
                    tau += 1;
                }
                return (tau, self.cumulative_mean[tau]);
            }
            tau += 1;
        }

        // Fallback to global minimum for noisy but periodic signals
        let mut best_tau = min_period;
        let mut best_val = self.cumulative_mean[min_period];

        for (tau, &mean) in self.cumulative_mean[..=max_period]
            .iter()
            .enumerate()
            .skip(min_period + 1)
        {
            if mean < best_val {
                best_val = mean;
                best_tau = tau;
            }
        }

        if best_val < 0.5 {
            (best_tau, best_val)
        } else {
            (0, 1.0)
        }
    }

    /// YIN step 5: parabolic interpolation for sub-sample accuracy.
    fn parabolic_interpolation(&self, tau: usize, max_period: usize) -> f64 {
        if tau < 1 || tau >= max_period {
            return tau as f64;
        }

        // The period is a minimum of the cumulative mean: the peak of its negation.
        let peak = parabolic_peak(
            -self.cumulative_mean[tau - 1],
            -self.cumulative_mean[tau],
            -self.cumulative_mean[tau + 1],
        );
        tau as f64 + f64::from(peak.offset)
    }
}

#[cfg(test)]
mod tests;
