//! YIN monophonic pitch estimation.
//!
//! de Cheveigné & Kawahara, 2002. Named for the algorithm rather than for what
//! a DAW does with it: YIN has specific failure modes — octave errors on
//! strong harmonics, and a buffer-length floor set by the lowest frequency it
//! is asked to find — that a caller reaching for "pitch detection" would not
//! know it was choosing. A second estimator later gets its own name instead of
//! displacing this one.
//!
//! The estimate is a pure function of its input: the underlying detector's
//! scratch is fully overwritten on every call, so nothing carries between
//! them. [`yin`] builds a fresh detector regardless; [`yin_track`] reuses one
//! across frames, which relies on that property — and
//! `a_reused_detector_matches_fresh_ones` pins it.

use tutti_buffers::{Mono, Sample};
use tutti_types::{Cents, Confidence, Hz, Note, SampleRate, Samples};

use crate::chroma::FrequencyBand;
use crate::convert::to_f32;
use crate::framing::{Framing, check_nyquist};
use crate::notes::PitchHop;
use crate::pitch::PitchDetector;
use crate::sequence::median_of_sorted;
use crate::{Error, Result};
use tutti_dsp::FrameCount;
use tutti_types::ranged;

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
        Samples((self.sample_rate.get() / self.max_freq.get() as f64) as usize)
    }

    /// Longest period: set by the *lowest* frequency.
    #[inline]
    pub(crate) fn max_period(&self) -> Samples {
        Samples((self.sample_rate.get() / self.min_freq.get() as f64) as usize)
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
    Ok(estimate(&mut plan.detector(), &to_f32(samples.samples())))
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
    yin_track_at(&plan, &to_f32(samples.samples()), hop.get())
}

/// [`yin_track`] over a resolved config and `f32` samples.
pub(crate) fn yin_track_at(
    plan: &YinPlan,
    samples: &[f32],
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

fn estimate(detector: &mut PitchDetector, samples: &[f32]) -> PitchEstimate {
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
    let mut freqs: Vec<f32> = Vec::with_capacity(2 * half + 1);
    (0..pitches.len())
        .map(|i| {
            if i < half || i + half >= pitches.len() {
                return pitches[i];
            }
            freqs.clear();
            freqs.extend((i - half..=i + half).map(|j| pitches[j].frequency().get()));
            freqs.sort_unstable_by(f32::total_cmp);
            // An odd count, so the median is one of the frequencies.
            let median = median_of_sorted(&freqs).unwrap_or_default() as f32;

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
    const LOWER: f32 = 0.79;
    const UPPER: f32 = 1.26;
    const MAJOR_THIRD_LN: f32 = 0.23;

    let mut result = pitches.to_vec();
    for i in 1..result.len() {
        let (Some(current), Some(previous)) = (result[i].pitch(), result[i - 1].pitch()) else {
            continue;
        };
        let ratio = current.frequency.get() / previous.frequency.get();
        if (LOWER..=UPPER).contains(&ratio) {
            continue;
        }

        let cost = ((libm::logf(ratio).abs() / MAJOR_THIRD_LN) - 1.0).max(0.0);
        let attenuation = libm::expf(-jump_penalty.get() * cost);
        if let PitchEstimate::Voiced(pitch) = &mut result[i] {
            pitch.confidence = Confidence::new_clamped(pitch.confidence.get() * attenuation);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: SampleRate = SampleRate::SR_44K1;

    fn standard() -> YinConfig {
        YinConfig::new()
    }

    fn frame(cfg: &YinConfig) -> usize {
        cfg.buffer_size(SR).unwrap().get()
    }

    fn pitch_of(cfg: &YinConfig, x: &[f32]) -> Result<PitchEstimate> {
        yin(cfg, Mono::new(x), SR)
    }

    fn track_of(cfg: &YinConfig, x: &[f32], hop: usize) -> Result<Vec<PitchEstimate>> {
        yin_track(cfg, Mono::new(x), SR, PitchHop::of(Samples(hop)))
    }

    fn sine(sample_rate: f64, freq: f32, seconds: f32) -> Vec<f32> {
        let n = (sample_rate as f32 * seconds) as usize;
        tutti_fixtures::signal::sine(
            tutti_types::Hz(freq),
            tutti_types::Amplitude(0.5),
            tutti_types::SampleRate::of(sample_rate),
            tutti_types::Samples(n),
        )
    }

    /// No band is written from an inverted, empty, non-positive or NaN pair,
    /// so there is no silent runtime nothing.
    #[test]
    fn an_inverted_range_is_not_a_band() {
        assert_eq!(
            FrequencyBand::try_from(Hz(2000.0)..=Hz(50.0)),
            Err(Error::EmptyFrequencyRange {
                min: Hz(2000.0),
                max: Hz(50.0),
            })
        );
        for (lo, hi) in [(50.0, 50.0), (-10.0, 2000.0), (f32::NAN, 2000.0)] {
            assert!(FrequencyBand::new(Hz(lo), Hz(hi)).is_none(), "{lo} {hi}");
        }
    }

    #[test]
    fn a_maximum_above_nyquist_is_refused() {
        assert_eq!(
            yin(
                &YinConfig::new().band(FrequencyBand::new(Hz(50.0), Hz(5000.0)).unwrap()),
                Mono::new(&[0.0f32; 4096]),
                SampleRate::SR_8K,
            ),
            Err(Error::AboveNyquist {
                freq: Hz(5000.0),
                nyquist: Hz(4000.0),
            })
        );
    }

    /// A short buffer is an error, not a silent "unvoiced".
    #[test]
    fn a_short_buffer_reports_why() {
        let cfg = standard();
        let samples = vec![0.0f32; 64];

        assert_eq!(
            pitch_of(&cfg, &samples),
            Err(Error::InsufficientInput {
                needed: cfg.buffer_size(SR).unwrap(),
                got: Samples(64),
            })
        );
    }

    #[test]
    fn silence_is_unvoiced() {
        let cfg = standard();
        let samples = vec![0.0f32; frame(&cfg) * 2];

        let estimate = pitch_of(&cfg, &samples).unwrap();
        assert!(!estimate.is_voiced());
        assert_eq!(estimate.frequency(), Hz(0.0));
        assert_eq!(estimate.confidence(), Confidence::NONE);
        assert!(estimate.pitch().is_none());
    }

    /// A non-finite sample in the window is an unvoiced frame, never a panic.
    #[test]
    fn a_non_finite_sample_is_unvoiced_not_a_panic() {
        let cfg = standard();
        for bad in [f32::NAN, f32::INFINITY] {
            let mut samples = sine(44100.0, 440.0, 1.0);
            samples.truncate(frame(&cfg) + 10);
            samples[3] = bad;
            let estimate = pitch_of(&cfg, &samples).unwrap();
            assert!(!estimate.is_voiced(), "{bad} produced a pitch");
        }
    }

    /// The property `yin_track` relies on: one detector driven across many
    /// frames agrees with a fresh detector per frame.
    ///
    /// `yin` builds a fresh detector every call, so the idempotence test below
    /// does *not* exercise the reused path — this one does. Without it, the
    /// module doc's soundness argument for reusing a detector across a track
    /// was unpinned.
    #[test]
    fn a_reused_detector_matches_fresh_ones() {
        let cfg = standard();
        let frame = frame(&cfg);

        // A sweep, so successive frames differ and a leaked carry would show.
        let samples: Vec<f32> = (0..frame * 6)
            .map(|i| {
                let t = i as f32 / 44100.0;
                let freq = 220.0 + 440.0 * t;
                (2.0 * core::f32::consts::PI * freq * t).sin() * 0.5
            })
            .collect();

        let reused = track_of(&cfg, &samples, frame).unwrap();

        for (i, expected) in reused.iter().enumerate() {
            let start = i * frame;
            let fresh = pitch_of(&cfg, &samples[start..start + frame]).unwrap();
            assert_eq!(fresh, *expected, "frame {i} differs from a fresh detector");
        }
    }

    #[test]
    fn the_same_input_always_gives_the_same_estimate() {
        let cfg = standard();
        let a = sine(44100.0, 440.0, 0.1);
        let b = sine(44100.0, 220.0, 0.1);

        let first = pitch_of(&cfg, &a).unwrap();
        let _ = pitch_of(&cfg, &b).unwrap();
        assert_eq!(first, pitch_of(&cfg, &a).unwrap());
    }

    #[test]
    fn a_track_is_voiced_throughout_a_steady_tone() {
        let cfg = standard();
        let samples = sine(44100.0, 440.0, 0.5);

        let track = track_of(&cfg, &samples, 512).unwrap();
        assert!(!track.is_empty());

        let voiced = track.iter().filter(|p| p.is_voiced()).count();
        assert!(
            voiced > track.len() / 2,
            "{voiced} of {} voiced",
            track.len()
        );
    }

    #[test]
    fn periods_derive_from_the_range_and_cannot_contradict_it() {
        let cfg = YinConfig::new()
            .band(FrequencyBand::new(Hz(100.0), Hz(1000.0)).unwrap())
            .plan(SR)
            .unwrap();
        assert_eq!(cfg.min_period(), Samples(44)); // 44100 / 1000
        assert_eq!(cfg.max_period(), Samples(441)); // 44100 / 100
        assert_eq!(cfg.buffer_size(), Samples(882));
        assert!(cfg.min_period() < cfg.max_period());
    }

    #[test]
    fn note_names_use_both_spellings() {
        let pitch = Pitch {
            frequency: Hz(277.18),
            confidence: Confidence(0.9),
            note: Note::new(tutti_types::PitchClass::CSharp, 4),
            cents_offset: Cents(0.0),
        };
        assert_eq!(pitch.note_name(), "C#4");
        assert_eq!(pitch.note_name_flat(), "Db4");
    }

    #[test]
    fn median_filter_removes_a_lone_outlier() {
        let voiced = |f: f32| {
            PitchEstimate::Voiced(Pitch {
                frequency: Hz(f),
                confidence: Confidence(0.9),
                note: Note::A4,
                cents_offset: Cents(0.0),
            })
        };
        let track = vec![
            voiced(440.0),
            voiced(440.0),
            voiced(880.0), // octave error
            voiced(440.0),
            voiced(440.0),
        ];

        let smoothed = median_filter(&track, FrameCount(3));
        assert_eq!(smoothed[2].frequency(), Hz(440.0), "outlier replaced");

        // A window of 1 or wider than the track is a no-op.
        assert_eq!(median_filter(&track, FrameCount(1)), track);
        assert_eq!(median_filter(&track, FrameCount(99)), track);
    }

    /// Accuracy across the range, not just at A440.
    #[test]
    fn tracks_frequencies_across_the_range() {
        let cfg = standard();

        for freq in [100.0f32, 220.0, 440.0, 880.0, 1000.0] {
            let samples = sine(44100.0, freq, 0.1);
            let estimate = pitch_of(&cfg, &samples).unwrap();
            let pitch = estimate
                .pitch()
                .unwrap_or_else(|| panic!("{freq} Hz unvoiced"));

            let error = ((pitch.frequency.get() - freq) / freq).abs() * 100.0;
            assert!(
                error < 2.0,
                "expected {freq} Hz, got {} ({error}% off)",
                pitch.frequency.get()
            );
        }
    }

    /// The extremes a configured range has to reach: a low guitar E and a high
    /// soprano C. Both need the range widened past the standard default.
    #[test]
    fn reaches_the_ends_of_a_widened_range() {
        let cfg = YinConfig::new().band(FrequencyBand::new(Hz(40.0), Hz(2000.0)).unwrap());

        // E2, the guitar's low string.
        let low = pitch_of(&cfg, &sine(44100.0, 82.41, 0.2)).unwrap();
        let pitch = low.pitch().expect("E2 should be voiced");
        assert!(
            ((pitch.frequency.get() - 82.41) / 82.41).abs() * 100.0 < 3.0,
            "expected ~82.41 Hz, got {}",
            pitch.frequency.get()
        );
        assert_eq!(pitch.note, Note::new(tutti_types::PitchClass::E, 2));

        // C6.
        let high = pitch_of(&cfg, &sine(44100.0, 1046.5, 0.1)).unwrap();
        let pitch = high.pitch().expect("C6 should be voiced");
        assert!(
            ((pitch.frequency.get() - 1046.5) / 1046.5).abs() * 100.0 < 3.0,
            "expected ~1046.5 Hz, got {}",
            pitch.frequency.get()
        );
    }

    /// Jump penalization attenuates an octave error without deleting it.
    #[test]
    fn penalize_jumps_attenuates_implausible_leaps() {
        let voiced = |f: f32, c: f32| {
            let (note, cents) = Note::nearest_to(Hz(f)).unwrap();
            PitchEstimate::Voiced(Pitch {
                frequency: Hz(f),
                confidence: Confidence(c),
                note,
                cents_offset: cents,
            })
        };

        let track = vec![voiced(440.0, 0.9), voiced(880.0, 0.9), voiced(440.0, 0.9)];
        let smoothed = penalize_jumps(&track, Confidence(1.0));

        // The octave leap loses confidence; the frames around it do not.
        assert!(smoothed[1].confidence() < track[1].confidence());
        assert_eq!(smoothed[0].confidence(), track[0].confidence());
        // But it survives — the estimate may still be right.
        assert!(smoothed[1].is_voiced());

        // A zero penalty is the identity, and a steady track is untouched.
        assert_eq!(penalize_jumps(&track, Confidence::NONE), track);
        let steady = vec![voiced(440.0, 0.9), voiced(445.0, 0.9)];
        assert_eq!(penalize_jumps(&steady, Confidence(1.0)), steady);
    }
}
