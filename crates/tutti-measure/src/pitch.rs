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

use tutti_types::{Confidence, Hz};

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

        for tau in min_period + 1..=max_period {
            if self.cumulative_mean[tau] < best_val {
                best_val = self.cumulative_mean[tau];
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
mod tests {
    use super::*;
    use crate::yin::YinConfig;
    use tutti_types::SampleRate;

    const SR: SampleRate = SampleRate::SR_48K;
    const N: usize = 4096;

    fn sine(freq: Hz, amp: f32) -> Vec<f32> {
        tutti_fixtures::signal::sine(
            freq,
            tutti_types::Amplitude(amp),
            SR,
            tutti_types::Samples(N),
        )
    }

    fn saw(freq: Hz, amp: f32) -> Vec<f32> {
        let f = f64::from(freq.get());
        (0..N)
            .map(|i| {
                let phase = (f * i as f64 / SR.get()).fract();
                ((2.0 * phase - 1.0) * f64::from(amp)) as f32
            })
            .collect()
    }

    fn white_noise() -> Vec<f32> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..N)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0) as f32 * 0.5
            })
            .collect()
    }

    fn detector() -> PitchDetector {
        PitchDetector::new(YinConfig::new().plan(SR).unwrap())
    }

    fn period_samples(freq: Hz) -> f64 {
        SR.get() / f64::from(freq.get())
    }

    fn fill_cmndf(det: &mut PitchDetector, samples: &[f32]) -> (usize, usize) {
        let (min_period, max_period) = det.periods(samples.len());
        det.compute_difference(samples, max_period);
        det.compute_cumulative_mean(max_period);
        (min_period, max_period)
    }

    /// d'(0) is 1 by definition, for any signal the difference function can see.
    #[test]
    fn cmndf_equals_one_at_lag_zero() {
        let mut det = detector();
        fill_cmndf(&mut det, &sine(Hz(440.0), 1.0));
        assert!(
            (det.cumulative_mean[0] - 1.0).abs() < f32::EPSILON,
            "d'(0) must be 1 by definition, got {}",
            det.cumulative_mean[0]
        );
    }

    /// A pure sine's CMNDF dips below the YIN threshold at the true period.
    #[test]
    fn cmndf_dips_below_threshold_at_the_true_period() {
        let freq = Hz(440.0);
        let tau = period_samples(freq).round() as usize;
        let mut det = detector();
        fill_cmndf(&mut det, &sine(freq, 1.0));
        assert!(
            det.cumulative_mean[tau] < det.cfg.threshold.get(),
            "d'({tau}) = {} was not below threshold {}",
            det.cumulative_mean[tau],
            det.cfg.threshold.get()
        );
    }

    /// The first dip below threshold wins, even when a later (octave-down) dip
    /// is deeper. A 110 Hz saw is periodic at T *and* 2T; 2T is the global
    /// minimum — the second-harmonic dip of the period — but the first
    /// below-threshold dip is the fundamental.
    #[test]
    fn first_local_minimum_beats_a_deeper_later_dip() {
        let freq = Hz(110.0);
        let true_period = period_samples(freq);
        let tau = true_period.round() as usize;
        let tau_2t = (true_period * 2.0).round() as usize;

        let mut det = detector();
        let (min_period, max_period) = fill_cmndf(&mut det, &saw(freq, 0.5));

        let dip_t = det.cumulative_mean[tau];
        let dip_2t = det.cumulative_mean[tau_2t];
        assert!(
            dip_2t < dip_t,
            "precondition: the 2T dip ({dip_2t}) must be deeper than the T dip ({dip_t})"
        );
        assert!(
            dip_t < det.cfg.threshold.get(),
            "precondition: the fundamental dip {dip_t} must be below threshold"
        );

        let (picked, _) = det.find_best_period_full(min_period, max_period);
        assert!(
            (picked as f64 - true_period).abs() < 2.0,
            "first-local-minimum should return ~{true_period:.1} (fundamental), got {picked}"
        );
        assert!(
            (picked as f64 - tau_2t as f64).abs() > 10.0,
            "picked the deeper 2T dip at {picked}, an octave down"
        );
    }

    /// Parabolic interpolation recovers a half-sample period; the integer lag
    /// alone is off by half a sample.
    #[test]
    fn parabolic_interpolation_recovers_a_half_sample_period() {
        let true_period = 100.5;
        let freq = Hz((SR.get() / true_period) as f32);
        let mut det = detector();
        let (min_period, max_period) = fill_cmndf(&mut det, &sine(freq, 1.0));
        let (tau, _) = det.find_best_period_full(min_period, max_period);
        let interpolated = det.parabolic_interpolation(tau, max_period);

        assert!(
            (tau as f64 - true_period).abs() > 0.4,
            "un-interpolated integer lag {tau} should be ~0.5 samples off {true_period}"
        );
        assert!(
            (interpolated - true_period).abs() < 0.05,
            "interpolated period {interpolated} should be within 0.05 of {true_period}"
        );
    }

    /// All-zero, DC, and white-noise buffers are unvoiced: no lag falls below
    /// threshold, so the detector must not invent a pitch.
    #[test]
    fn aperiodic_buffers_are_unvoiced() {
        for (label, samples) in [
            ("zeros", vec![0.0f32; N]),
            ("dc", vec![0.5f32; N]),
            ("noise", white_noise()),
        ] {
            let mut det = detector();
            let (min_period, max_period) = fill_cmndf(&mut det, &samples);
            let (period, aperiodicity) = det.find_best_period_full(min_period, max_period);
            assert_eq!(
                period, 0,
                "{label}: a lag of {period} (aperiodicity {aperiodicity}) fell out of an aperiodic buffer"
            );

            let mut det = detector();
            let result = det.detect(&samples);
            assert!(
                !result.is_voiced(),
                "{label} reported as {} Hz at confidence {}",
                result.frequency.get(),
                result.confidence.get()
            );
            assert_eq!(result.frequency, Hz(0.0), "{label} invented a frequency");
        }
    }

    /// YIN's CMNDF is a ratio, so a 40 dB amplitude change must not move the
    /// frequency.
    #[test]
    fn the_estimate_is_invariant_to_amplitude_scaling() {
        let freq = Hz(440.0);
        let quiet = {
            let mut det = detector();
            det.detect(&sine(freq, 0.01))
        };
        let loud = {
            let mut det = detector();
            det.detect(&sine(freq, 1.0))
        };

        assert!(quiet.is_voiced(), "0.01-amp 440 Hz was unvoiced");
        assert!(loud.is_voiced(), "1.0-amp 440 Hz was unvoiced");
        assert!(
            (quiet.frequency.get() - loud.frequency.get()).abs() < 0.05,
            "440 Hz read as {} at amp 0.01 but {} at amp 1.0",
            quiet.frequency.get(),
            loud.frequency.get()
        );
        assert!(
            (loud.frequency.get() - freq.get()).abs() < 0.1,
            "loud reading {} drifted from {}",
            loud.frequency.get(),
            freq.get()
        );
    }

    /// The FFT-derived difference function equals its definition
    /// `d(tau) = sum_{j<W} (x[j] - x[j+tau])^2`, summed directly, and is zero
    /// at a lag that is a whole number of periods.
    #[test]
    fn the_difference_function_matches_its_definition() {
        // 480 Hz at 48 kHz: exactly 100 samples per period.
        let x = sine(Hz(480.0), 0.8);
        let mut det = detector();
        let (_, max_period) = det.periods(x.len());
        det.compute_difference(&x, max_period);
        let window = max_period;
        for tau in [1usize, 17, 50, 100, 200, max_period] {
            let direct: f64 = (0..window)
                .map(|j| {
                    let d = f64::from(x[j]) - f64::from(x[j + tau]);
                    d * d
                })
                .sum();
            let got = f64::from(det.difference[tau]);
            assert!(
                (got - direct).abs() < 1e-3 * direct.max(1.0),
                "tau {tau}: {got} vs {direct}"
            );
        }
        assert!(det.difference[100] < 1e-3, "{}", det.difference[100]);
        assert!(det.difference[50] > 100.0, "{}", det.difference[50]);
    }
}
