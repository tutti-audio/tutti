//! YIN monophonic pitch estimation.
//!
//! de Cheveigné & Kawahara, 2002. Named for the algorithm rather than for what
//! a DAW does with it: YIN has specific failure modes — octave errors on
//! strong harmonics, and a buffer-length floor set by the lowest frequency it
//! is asked to find — that a caller reaching for "pitch detection" would not
//! know it was choosing.
//!
//! The estimate is a pure function of its input: the underlying detector's
//! scratch is fully overwritten on every call, so nothing carries between
//! them. [`yin`] builds a fresh detector regardless; [`yin_track`] reuses one
//! across frames, which relies on that property — and
//! `a_reused_detector_matches_fresh_ones` pins it.
//!
//! ## Algorithm
//!
//! The detector covers steps 1 to 5 of the paper (autocorrelation through
//! parabolic interpolation), grouped as four stages, and omits step 6, the
//! best-local-estimate search over neighbouring frames:
//!
//! 1. **Difference function** - d(τ) = Σ(x\[j\] - x\[j+τ\])²
//! 2. **Cumulative mean normalized difference** - d'(τ)
//! 3. **Absolute threshold** - Find first τ where d'(τ) < threshold
//! 4. **Parabolic interpolation** - Sub-sample accuracy
//!
//! The difference function comes from an FFT-based autocorrelation via the
//! Wiener-Khinchin theorem, r(τ) = IFFT(|FFT(x)|²), in O(n log n).

use tutti_buffers::{Mono, MonoMut, Sample};
use tutti_music::Note;
use tutti_units::{Cents, Confidence, Hz, SampleRate, Samples};

use crate::PitchError;
use crate::analyses::notes::PitchHop;
use crate::convert::widen;
use crate::framing::{Framing, check_nyquist};
use crate::meters::chroma::FrequencyBand;

use tutti_dsp::{Correlator, FftScratch, FrameCount, median_of_sorted, parabolic_peak};
use tutti_units::ranged;

ranged! {
    /// YIN's absolute threshold on the cumulative mean difference: within
    /// `0.01..=0.5`.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Confidence", into = "Confidence")
    )]
    YinThreshold(Confidence), YinThresholdError, "a YIN threshold within 0.01..=0.5",
    |raw| raw.get() >= 0.01 && raw.get() <= 0.5
}

/// YIN parameters: the band searched and the absolute threshold. Immutable
/// once built, and in hertz, so it holds no sample rate: [`yin`] and
/// [`yin_track`] take the buffer's rate and check the band against its
/// Nyquist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YinConfig {
    band: FrequencyBand,
    threshold: Confidence,
}

impl YinConfig {
    /// The standard 50-2000 Hz vocal and instrument band and the paper's
    /// threshold of 0.1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            band: FrequencyBand::VOICE,
            threshold: Confidence::new(0.1),
        }
    }

    /// Searches `band`. The band holds an ordered, finite, positive pair, so an
    /// inverted range (which would invert the period bounds and report
    /// "unvoiced" forever) cannot be written. A band whose top exceeds half
    /// the buffer's sample rate is refused by the call that takes the buffer
    /// ([`AboveNyquist`](crate::AboveNyquist)).
    #[must_use]
    pub fn band(mut self, band: FrequencyBand) -> Self {
        self.band = band;
        self
    }

    /// YIN's absolute threshold on the cumulative mean difference. The paper's
    /// default is 0.1; lower finds fewer pitches and fewer errors.
    #[must_use]
    pub fn threshold(mut self, threshold: YinThreshold) -> Self {
        self.threshold = threshold.get();
        self
    }

    /// The config at `rate`.
    ///
    /// # Errors
    ///
    /// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`.
    pub(crate) fn plan(&self, rate: SampleRate) -> Result<YinPlan, PitchError> {
        let (min_freq, max_freq) = (self.band.min(), self.band.max());
        check_nyquist(max_freq, rate)?;
        Ok(YinPlan {
            sample_rate: rate,
            min_freq,
            max_freq,
            threshold: self.threshold,
        })
    }

    /// The shortest buffer [`yin`] reads at `rate`; below this it returns
    /// [`PitchError::InsufficientInput`] rather than a silent "unvoiced".
    ///
    /// # Errors
    ///
    /// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`.
    pub fn buffer_size(&self, rate: impl Into<SampleRate>) -> Result<Samples, PitchError> {
        Ok(self.plan(rate.into())?.buffer_size())
    }
}

impl Default for YinConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`YinConfig`] at one sample rate, its band checked against Nyquist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct YinPlan {
    sample_rate: SampleRate,
    min_freq: Hz,
    max_freq: Hz,
    pub(crate) threshold: Confidence,
}

impl YinPlan {
    /// The rate the analyzed buffer is denominated at.
    #[inline]
    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Shortest period the range can express: set by the *highest* frequency.
    #[inline]
    pub(crate) fn min_period(&self) -> Samples {
        Samples::new((self.sample_rate.get() / self.max_freq.get()) as usize)
    }

    /// Longest period: set by the *lowest* frequency.
    #[inline]
    pub(crate) fn max_period(&self) -> Samples {
        Samples::new((self.sample_rate.get() / self.min_freq.get()) as usize)
    }

    /// Minimum input length.
    #[inline]
    pub(crate) fn buffer_size(&self) -> Samples {
        Samples::new(self.max_period().get() * 2)
    }

    pub(crate) fn detector(&self) -> PitchDetector {
        PitchDetector::new(*self)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PitchResult {
    /// The detected pitch in [`Hz`]; `Hz::new(0.0)` if unvoiced.
    pub frequency: Hz,
    /// Detection strength as a [`Confidence`] reading, `0.0..=1.0`. A
    /// measurement, not a blend — deliberately not `Mix`.
    pub confidence: Confidence,
}

impl PitchResult {
    /// Whether this reading is a real pitch: both a positive frequency and a
    /// non-zero [`Confidence`]. A default (unvoiced) result is neither.
    pub fn is_voiced(&self) -> bool {
        self.frequency > Hz::new(0.0) && self.confidence > Confidence::new(0.0)
    }
}

/// YIN pitch detector (de Cheveigné & Kawahara, 2002).
///
/// Uses FFT-based autocorrelation for O(n log n) performance. Everything a
/// call needs is sized and planned at construction, so [`detect`](Self::detect)
/// allocates nothing and a track reuses one detector for every frame.
pub(crate) struct PitchDetector {
    /// The validated range, rate and threshold this detector runs at.
    cfg: YinPlan,
    difference: Vec<f64>,
    cumulative_mean: Vec<f64>,
    fft: FftScratch<f64>,
    correlator: Correlator<f64>,
    /// The circular cross-correlation `r(τ) = Σ_j x[j]*x[j+τ]`, `fft_size`
    /// points.
    correlation: Vec<f64>,
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
    pub(crate) fn detect(&mut self, samples: &[f64]) -> PitchResult {
        let Some((min_period, max_period)) = self.normalized_difference(samples) else {
            return PitchResult::default();
        };
        let (period, aperiodicity) = self.find_best_period_full(min_period, max_period);

        if period == 0 {
            return PitchResult::default();
        }

        let refined_period = self.parabolic_interpolation(period, max_period);
        let frequency = Hz::new(self.cfg.sample_rate().get() / refined_period);
        // Clamped rather than wrapped raw: `Confidence` is a 0..=1 reading, and
        // this is the one place the aperiodicity inversion could leave the range.
        let confidence = Confidence::new_clamped(1.0 - aperiodicity);

        PitchResult {
            frequency,
            confidence,
        }
    }

    /// YIN steps 1-3 over `samples`: fills the cumulative mean normalized
    /// difference d'(τ), read by [`cumulative_mean`](Self::cumulative_mean),
    /// and returns the lag range `(min, max)` it holds, or `None` when
    /// `samples` is too short or the band leaves no lag.
    pub(crate) fn normalized_difference(&mut self, samples: &[f64]) -> Option<(usize, usize)> {
        let (min_period, max_period) = self.periods(samples.len());
        if samples.len() < max_period * 2 || max_period <= min_period {
            return None;
        }
        self.compute_difference(samples, max_period);
        self.compute_cumulative_mean(max_period);
        Some((min_period, max_period))
    }

    /// d'(τ) for `τ` in `0..=max` of the last [`normalized_difference`](Self::normalized_difference).
    pub(crate) fn cumulative_mean(&self) -> &[f64] {
        &self.cumulative_mean
    }

    /// The period of the minimum at lag `tau`, refined by the parabola through
    /// its neighbours (YIN step 5).
    pub(crate) fn refined_period(&self, tau: usize, max_period: usize) -> f64 {
        self.parabolic_interpolation(tau, max_period)
    }

    /// The rate the detector's lags are denominated at.
    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.cfg.sample_rate()
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
    fn compute_difference(&mut self, samples: &[f64], max_period: usize) {
        let window = max_period;
        let usable = samples.len().min(window + max_period);

        // Cumulative squared energy for running window sums
        let cum_sq = &mut self.cum_sq;
        cum_sq[0] = 0.0;
        for i in 0..usable {
            cum_sq[i + 1] = cum_sq[i] + samples[i] * samples[i];
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
            let autocorr = self.correlation[tau];
            let e_tau = energy(tau, window);
            self.difference[tau] = (e0 + e_tau - 2.0 * autocorr).max(0.0);
        }
    }

    /// YIN step 3: d'(τ) = d(τ) / ((1/τ) * Σ d(j)), d'(0) = 1
    fn compute_cumulative_mean(&mut self, max_period: usize) {
        self.cumulative_mean[0] = 1.0;

        let mut running_sum = 0.0f64;
        for tau in 1..=max_period {
            running_sum += self.difference[tau];
            if running_sum > 1e-10 {
                self.cumulative_mean[tau] = self.difference[tau] * tau as f64 / running_sum;
            } else {
                self.cumulative_mean[tau] = 1.0;
            }
        }
    }

    /// YIN step 4: return the FIRST local minimum below threshold (not the global
    /// minimum) to prevent octave errors from subharmonic detection.
    /// Returns (period, aperiodicity).
    fn find_best_period_full(&self, min_period: usize, max_period: usize) -> (usize, f64) {
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
        tau as f64 + peak.offset
    }
}

/// A pitch estimate: either a pitch, or the absence of one.
///
/// Two variants rather than a flat struct with placeholder fields. A flat shape
/// admits nonsense like `{frequency: 0.0, confidence: 0.9, note: Some(A4)}`, and
/// pushes callers to reconstruct "is this voiced?" from two float comparisons.
///
/// The note is a [`Note`] rather than a raw MIDI integer — a note is a musical
/// fact and MIDI one encoding of it. `tutti_ump::NoteNumber::try_from(pitch.note)` crosses into
/// that encoding where a caller actually needs it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PitchEstimate {
    /// No pitch found: the aperiodicity never fell below the threshold.
    #[default]
    Unvoiced,
    /// A pitch was found, with its frequency, confidence and nearest note.
    Voiced(Pitch),
}

/// A detected pitch. Every field is meaningful — that is what the enum above
/// buys over a flat struct.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pitch {
    /// The estimated fundamental in [`Hz`], parabolically interpolated for
    /// sub-sample period accuracy.
    pub frequency: Hz,
    /// How periodic the frame was, as a [`Confidence`] reading in `0..=1`. A
    /// measurement, not a blend — deliberately not `Mix`.
    pub confidence: Confidence,
    /// The nearest equal-tempered note.
    pub note: Note,
    /// Distance from that note in [`Cents`], −50..+50.
    pub cents_offset: Cents,
}

impl PitchEstimate {
    /// Whether a pitch was found at all.
    #[inline]
    #[must_use]
    pub fn is_voiced(&self) -> bool {
        matches!(self, Self::Voiced(_))
    }

    /// The [`Pitch`] if voiced, `None` otherwise.
    #[inline]
    pub fn pitch(&self) -> Option<&Pitch> {
        match self {
            Self::Voiced(p) => Some(p),
            Self::Unvoiced => None,
        }
    }

    /// The frequency in [`Hz`], or `Hz::new(0.0)` when unvoiced — field-read
    /// ergonomics for callers that treat silence as zero.
    #[inline]
    pub fn frequency(&self) -> Hz {
        self.pitch().map_or(Hz::new(0.0), |p| p.frequency)
    }

    /// The [`Confidence`], or [`Confidence::NONE`] when unvoiced.
    #[inline]
    pub fn confidence(&self) -> Confidence {
        self.pitch().map_or(Confidence::NONE, |p| p.confidence)
    }
}

impl Pitch {
    /// Sharp notation, e.g. `A4`, `C#5`.
    ///
    /// Delegates to [`Note`], which owns the twelve pitch classes and the
    /// octave convention — so neither spelling is a `[&str; 12]` table copied
    /// here and indexed by an open-coded `% 12`.
    pub fn note_name(&self) -> String {
        self.note.sharp_label()
    }

    /// Flat notation, e.g. `A4`, `Db5`.
    pub fn note_name_flat(&self) -> String {
        self.note.flat_label()
    }
}

/// Estimates the pitch of one buffer at `rate`.
///
/// Stateless: the same input always yields the same estimate.
///
/// # Errors
///
/// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`;
/// [`PitchError::InsufficientInput`] if `samples` is shorter than
/// [`YinConfig::buffer_size`], an explicit refusal rather than a silent
/// "unvoiced".
pub fn yin<S: Sample>(
    cfg: &YinConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<PitchEstimate, PitchError> {
    let plan = cfg.plan(rate.into())?;
    let needed = plan.buffer_size();
    if samples.frames() < needed {
        return Err(PitchError::InsufficientInput {
            needed,
            got: samples.frames(),
        });
    }
    Ok(estimate(&mut plan.detector(), &widen(samples.samples())))
}

/// Estimates the pitch at `hop` intervals across a buffer at `rate`.
///
/// # Errors
///
/// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`, and
/// [`PitchError::InsufficientInput`] if `samples` is shorter than
/// [`YinConfig::buffer_size`].
pub fn yin_track<S: Sample>(
    cfg: &YinConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
    hop: PitchHop,
) -> Result<Vec<PitchEstimate>, PitchError> {
    let plan = cfg.plan(rate.into())?;
    yin_track_at(&plan, &widen(samples.samples()), hop.get())
}

/// [`yin_track`] over a resolved config and `f64` samples.
pub(crate) fn yin_track_at(
    plan: &YinPlan,
    samples: &[f64],
    hop: Samples,
) -> Result<Vec<PitchEstimate>, PitchError> {
    let frame = plan.buffer_size();
    if samples.len() < frame.get() {
        return Err(PitchError::InsufficientInput {
            needed: frame,
            got: Samples::new(samples.len()),
        });
    }

    // One detector for the whole track: `detect` carries nothing between
    // calls, so reusing it is both sound and cheaper than replanning per frame.
    let mut detector = plan.detector();
    let frames = Framing::new(frame, hop).frames_for(Samples::new(samples.len()));
    Ok((0..frames.get())
        .map(|i| {
            let start = i * hop.get();
            estimate(&mut detector, &samples[start..start + frame.get()])
        })
        .collect())
}

fn estimate(detector: &mut PitchDetector, samples: &[f64]) -> PitchEstimate {
    let raw = detector.detect(samples);
    if !raw.is_voiced() {
        return PitchEstimate::Unvoiced;
    }
    let frequency = raw.frequency;
    // Derive the note from the frequency rather than trusting the detector's
    // own rounding: `Note::nearest_to` owns that conversion for the whole
    // engine, and returns the cent offset alongside it.
    let Some((note, cents_offset)) = Note::nearest_to(frequency) else {
        return PitchEstimate::Unvoiced;
    };
    PitchEstimate::Voiced(Pitch {
        frequency,
        confidence: raw.confidence,
        note,
        cents_offset,
    })
}

/// Median-filters a pitch track over `window` **frames**.
///
/// The count is frames, not samples: an FFT window size passed here would
/// smooth over three orders of magnitude too much, which is why the parameter
/// is a [`FrameCount`].
pub fn median_filter(window: FrameCount, pitches: &[PitchEstimate]) -> Vec<PitchEstimate> {
    let w = window.get();
    if w <= 1 || pitches.len() < w {
        return pitches.to_vec();
    }

    let half = w / 2;
    // One sort buffer for the whole track, refilled per frame.
    let mut freqs: Vec<f64> = Vec::with_capacity(2 * half + 1);
    (0..pitches.len())
        .map(|i| {
            if i < half || i + half >= pitches.len() {
                return pitches[i];
            }
            freqs.clear();
            freqs.extend((i - half..=i + half).map(|j| pitches[j].frequency().get()));
            freqs.sort_unstable_by(f64::total_cmp);
            // An odd count, so the median is one of the frequencies.
            let median = median_of_sorted(&freqs).unwrap_or_default();

            // Keep the neighbour whose frequency the median picked, so the
            // confidence and note travel with it.
            (i - half..=i + half)
                .map(|j| pitches[j])
                .find(|p| p.frequency().get() == median)
                .unwrap_or(pitches[i])
        })
        .collect()
}

/// Penalizes implausible pitch jumps between adjacent frames.
///
/// A melodic line rarely leaps more than a major third between consecutive
/// analysis frames, so a larger jump is usually an octave error rather than a
/// real interval. This attenuates the confidence of such frames instead of
/// deleting them — the estimate may still be right, and the caller decides
/// what confidence is enough.
///
/// `jump_penalty` scales the attenuation: 0 leaves the track untouched.
pub fn penalize_jumps(jump_penalty: Confidence, pitches: &[PitchEstimate]) -> Vec<PitchEstimate> {
    if pitches.len() < 2 {
        return pitches.to_vec();
    }

    // A major third either way, and ln(1.26) as the scale for how far past it
    // a jump reaches.
    const LOWER: f64 = 0.79;
    const UPPER: f64 = 1.26;
    const MAJOR_THIRD_LN: f64 = 0.23;

    let mut result = pitches.to_vec();
    for i in 1..result.len() {
        let (Some(current), Some(previous)) = (result[i].pitch(), result[i - 1].pitch()) else {
            continue;
        };
        let ratio = current.frequency / previous.frequency;
        if (LOWER..=UPPER).contains(&ratio) {
            continue;
        }

        let cost = ((libm::log(ratio).abs() / MAJOR_THIRD_LN) - 1.0).max(0.0);
        let attenuation = libm::exp(-jump_penalty.get() * cost);
        if let PitchEstimate::Voiced(pitch) = &mut result[i] {
            pitch.confidence = Confidence::new_clamped(pitch.confidence.get() * attenuation);
        }
    }
    result
}

#[cfg(test)]
mod detector_tests;
#[cfg(test)]
mod tests;
