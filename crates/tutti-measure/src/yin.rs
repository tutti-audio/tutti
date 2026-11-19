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

use tutti_buffers::{Mono, Sample};
use tutti_music::Note;
use tutti_units::{Cents, Confidence, Hz, SampleRate, Samples};

use crate::chroma::FrequencyBand;
use crate::convert::widen;
use crate::framing::{Framing, check_nyquist};
use crate::notes::PitchHop;
use crate::pitch::PitchDetector;
use crate::{Error, Result};
use tutti_dsp::FrameCount;
use tutti_dsp::median_of_sorted;
use tutti_units::ranged;

ranged! {
    /// YIN's absolute threshold on the cumulative mean difference: within
    /// `0.01..=0.5`.
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
            threshold: Confidence(0.1),
        }
    }

    /// Searches `band`. The band holds an ordered, finite, positive pair, so an
    /// inverted range (which would invert the period bounds and report
    /// "unvoiced" forever) cannot be written. A band whose top exceeds half
    /// the buffer's sample rate is refused by the call that takes the buffer
    /// ([`Error::AboveNyquist`]).
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
    /// [`Error::AboveNyquist`] when the band's top exceeds half `rate`.
    pub(crate) fn plan(&self, rate: SampleRate) -> Result<YinPlan> {
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
    /// [`Error::InsufficientInput`] rather than a silent "unvoiced".
    ///
    /// # Errors
    ///
    /// [`Error::AboveNyquist`] when the band's top exceeds half `rate`.
    pub fn buffer_size(&self, rate: impl Into<SampleRate>) -> Result<Samples> {
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
        Samples((self.sample_rate.get() / self.max_freq.get()) as usize)
    }

    /// Longest period: set by the *lowest* frequency.
    #[inline]
    pub(crate) fn max_period(&self) -> Samples {
        Samples((self.sample_rate.get() / self.min_freq.get()) as usize)
    }

    /// Minimum input length.
    #[inline]
    pub(crate) fn buffer_size(&self) -> Samples {
        Samples(self.max_period().get() * 2)
    }

    fn detector(&self) -> PitchDetector {
        PitchDetector::new(*self)
    }
}

/// A pitch estimate: either a pitch, or the absence of one.
///
/// Two variants rather than a flat struct with placeholder fields. A flat shape
/// admits nonsense like `{frequency: 0.0, confidence: 0.9, note: Some(A4)}`, and
/// pushes callers to reconstruct "is this voiced?" from two float comparisons.
///
/// The note is a [`Note`] rather than a raw MIDI integer — a note is a musical
/// fact and MIDI one encoding of it. `u8::try_from(pitch.note)` crosses into
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

    /// The frequency in [`Hz`], or `Hz(0.0)` when unvoiced — field-read
    /// ergonomics for callers that treat silence as zero.
    #[inline]
    pub fn frequency(&self) -> Hz {
        self.pitch().map_or(Hz(0.0), |p| p.frequency)
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
/// [`Error::AboveNyquist`] when the band's top exceeds half `rate`;
/// [`Error::InsufficientInput`] if `samples` is shorter than
/// [`YinConfig::buffer_size`], an explicit refusal rather than a silent
/// "unvoiced".
pub fn yin<S: Sample>(
    cfg: &YinConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<PitchEstimate> {
    let plan = cfg.plan(rate.into())?;
    let needed = plan.buffer_size();
    if samples.frames() < needed {
        return Err(Error::InsufficientInput {
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
/// [`Error::AboveNyquist`] when the band's top exceeds half `rate`, and
/// [`Error::InsufficientInput`] if `samples` is shorter than
/// [`YinConfig::buffer_size`].
pub fn yin_track<S: Sample>(
    cfg: &YinConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
    hop: PitchHop,
) -> Result<Vec<PitchEstimate>> {
    let plan = cfg.plan(rate.into())?;
    yin_track_at(&plan, &widen(samples.samples()), hop.get())
}

/// [`yin_track`] over a resolved config and `f64` samples.
pub(crate) fn yin_track_at(
    plan: &YinPlan,
    samples: &[f64],
    hop: Samples,
) -> Result<Vec<PitchEstimate>> {
    let frame = plan.buffer_size();
    if samples.len() < frame.get() {
        return Err(Error::InsufficientInput {
            needed: frame,
            got: Samples(samples.len()),
        });
    }

    // One detector for the whole track: `detect` carries nothing between
    // calls, so reusing it is both sound and cheaper than replanning per frame.
    let mut detector = plan.detector();
    let frames = Framing::new(frame, hop).frames_for(Samples(samples.len()));
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
pub fn median_filter(pitches: &[PitchEstimate], window: FrameCount) -> Vec<PitchEstimate> {
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
pub fn penalize_jumps(pitches: &[PitchEstimate], jump_penalty: Confidence) -> Vec<PitchEstimate> {
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
mod tests;
