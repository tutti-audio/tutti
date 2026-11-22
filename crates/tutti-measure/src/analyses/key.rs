//! Musical key by the Krumhansl-Schmuckler method.
//!
//! The mean chroma of a piece is correlated (Pearson) with a major and a minor
//! key profile rotated to each of the twelve tonics: 24 correlations, the
//! highest of which names the key. The profile is a [`KeyProfile`]: the
//! probe-tone ratings of Krumhansl & Kessler (1982), or Temperley's (2001),
//! which weighs the scale degrees more evenly and reads minor keys better.
//!
//! Pearson correlation ignores the level and the offset of the chroma, so
//! loudness and a noise floor do not move the result. It is undefined for a
//! chroma with no spread, which is why silence and a flat chroma are errors
//! rather than an arbitrary key.

use tutti_buffers::{Mono, Sample};
use tutti_music::PitchClass;
use tutti_units::{Confidence, SampleRate};

use crate::convert::Gathered;
use crate::meters::chroma::{CLASSES, ChromaConfig, chroma, mean_of_rows};
use crate::{KeyEstimateError, Measure, MeterError, StreamMeter};

use tutti_dsp::Grid;

/// A chroma whose spread is under this fraction of its mean is flat: the
/// 24 profiles would correlate alike, down to rounding.
const FLAT: f64 = 1e-6;

const KRUMHANSL_MAJOR: [f64; CLASSES] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const KRUMHANSL_MINOR: [f64; CLASSES] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];
const TEMPERLEY_MAJOR: [f64; CLASSES] =
    [5.0, 2.0, 3.5, 2.0, 4.5, 4.0, 2.0, 4.5, 2.0, 3.5, 1.5, 4.0];
const TEMPERLEY_MINOR: [f64; CLASSES] =
    [5.0, 2.0, 3.5, 4.5, 2.0, 4.0, 2.0, 4.5, 3.5, 2.0, 1.5, 4.0];

/// Major or minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// A major key.
    Major,
    /// A minor key.
    Minor,
}

/// A tonic and a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The tonic.
    pub tonic: PitchClass,
    /// The mode.
    pub mode: Mode,
}

impl core::fmt::Display for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mode = match self.mode {
            Mode::Major => "major",
            Mode::Minor => "minor",
        };
        write!(f, "{} {mode}", self.tonic.sharp_name())
    }
}

/// The ratings a chroma is correlated with.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum KeyProfile {
    /// Krumhansl & Kessler (1982).
    #[default]
    Krumhansl,
    /// Temperley (2001).
    Temperley,
}

impl KeyProfile {
    fn ratings(self, mode: Mode) -> &'static [f64; CLASSES] {
        match (self, mode) {
            (Self::Krumhansl, Mode::Major) => &KRUMHANSL_MAJOR,
            (Self::Krumhansl, Mode::Minor) => &KRUMHANSL_MINOR,
            (Self::Temperley, Mode::Major) => &TEMPERLEY_MAJOR,
            (Self::Temperley, Mode::Minor) => &TEMPERLEY_MINOR,
        }
    }
}

/// The best key, how sure the estimate is, and the second best.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyEstimate {
    /// The key whose profile correlates best.
    pub key: Key,
    /// How far the best correlation `r1` is ahead of the runner-up's `r2`,
    /// as the share of the distance from `r2` to a perfect fit that it closes:
    /// `(r1 - r2) / (1 - r2)`. 1 for a chroma that is exactly a key's profile,
    /// 0 for a tie, and low for a pair of closely related keys (a major key and
    /// its relative minor share seven notes) or a chroma that fits nothing.
    pub confidence: Confidence,
    /// The second best key.
    pub runner_up: Key,
}

/// What a [`KeyMeter`] reads a key with: the [`KeyProfile`] and the
/// [`ChromaConfig`] whose mean it is correlated with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyConfig {
    profile: KeyProfile,
    chroma: ChromaConfig,
}

impl KeyConfig {
    /// Reads the key of the default [`ChromaConfig`] against `profile`.
    #[must_use]
    pub fn new(profile: KeyProfile) -> Self {
        Self {
            profile,
            chroma: ChromaConfig::new(),
        }
    }

    /// The chroma the key is read from.
    #[must_use]
    pub fn chroma(mut self, chroma: ChromaConfig) -> Self {
        self.chroma = chroma;
        self
    }
}

impl<S: Sample> Measure<S> for KeyConfig {
    type Context = SampleRate;
    type Meter = KeyMeter<S>;

    /// A meter that reads the key of the audio at `rate`, once it has all of it.
    ///
    /// # Errors
    ///
    /// What [`ChromaConfig`] refuses at `rate`.
    fn measure(&self, rate: SampleRate) -> Result<KeyMeter<S>, MeterError> {
        self.chroma.plan(rate)?;
        Ok(KeyMeter {
            cfg: *self,
            rate,
            audio: Gathered::new(),
        })
    }
}

/// A running key estimate at one rate: gathers the audio and, at
/// [`finish`](StreamMeter::finish), takes its [`chroma()`] and reads the key of
/// it with [`estimate_key`].
///
/// Not for the audio thread: it holds the whole stream, so it grows with the
/// audio and allocates.
#[derive(Debug)]
pub struct KeyMeter<S: Sample> {
    cfg: KeyConfig,
    rate: SampleRate,
    audio: Gathered<S>,
}

impl<S: Sample> StreamMeter for KeyMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Result<KeyEstimate, KeyEstimateError>;

    /// Gathers the next chunk of the audio; no key is read before the end.
    fn push(&mut self, chunk: Mono<'_, S>, _: &mut impl Extend<()>) -> Result<(), MeterError> {
        self.audio.push(chunk);
        Ok(())
    }

    /// The key of the audio gathered, or what its chroma or [`estimate_key`]
    /// refuses it for.
    fn finish(self) -> Result<KeyEstimate, KeyEstimateError> {
        let made = chroma(&self.cfg.chroma, Mono::new(self.audio.samples()), self.rate)?;
        estimate_key(self.cfg.profile, made.grid())
    }
}

/// The key of a chromagram's mean.
///
/// # Errors
///
/// [`KeyEstimateError::NotTwelveClasses`] when the grid is not 12 columns wide;
/// [`KeyEstimateError::NegativeInput`] when it holds a negative value; [`KeyEstimateError::NonFiniteInput`] for a NaN or infinity;
/// [`KeyEstimateError::NoTonalContent`] when it has no frames, is silent or is flat.
pub fn estimate_key(
    profile: KeyProfile,
    chroma: &Grid<f64>,
) -> Result<KeyEstimate, KeyEstimateError> {
    if chroma.bins().get() != CLASSES {
        return Err(KeyEstimateError::NotTwelveClasses {
            columns: chroma.bins().get(),
        });
    }
    let mean = mean_of_rows(chroma);
    estimate_key_from_mean(profile, &mean)
}

/// The key of one chroma vector, such as a mean read from a
/// [`ChromaMeter`](crate::ChromaMeter).
///
/// # Errors
///
/// As [`estimate_key`], for a vector.
pub fn estimate_key_from_mean(
    profile: KeyProfile,
    mean: &[f64; CLASSES],
) -> Result<KeyEstimate, KeyEstimateError> {
    if mean.iter().any(|v| !v.is_finite()) {
        return Err(KeyEstimateError::NonFiniteInput);
    }
    if mean.iter().any(|&v| v < 0.0) {
        return Err(KeyEstimateError::NegativeInput);
    }
    let x = mean.map(f64::from);
    let (centered, norm) = center(&x);
    let level = x.iter().sum::<f64>() / CLASSES as f64;
    if norm.is_nan() || norm <= FLAT * level * (CLASSES as f64).sqrt() {
        return Err(KeyEstimateError::NoTonalContent);
    }

    // `scores[i]` for `i < 12` is major on tonic `i`, then minor.
    let mut scores = [0.0f64; 2 * CLASSES];
    for (mode_index, mode) in [Mode::Major, Mode::Minor].into_iter().enumerate() {
        let (p, p_norm) = center(profile.ratings(mode));
        for tonic in 0..CLASSES {
            let dot: f64 = (0..CLASSES)
                .map(|i| centered[i] * p[(i + CLASSES - tonic) % CLASSES])
                .sum();
            scores[mode_index * CLASSES + tonic] = dot / (norm * p_norm);
        }
    }

    // Ties go to the lower index: major before minor, then the lower tonic.
    let (mut best, mut second) = (0, 1);
    if scores[1] > scores[0] {
        (best, second) = (1, 0);
    }
    for i in 2..scores.len() {
        if scores[i] > scores[best] {
            (best, second) = (i, best);
        } else if scores[i] > scores[second] {
            second = i;
        }
    }
    let (r1, r2) = (scores[best], scores[second]);
    let confidence = if r2 < 1.0 {
        Confidence::new_clamped((r1 - r2) / (1.0 - r2))
    } else {
        Confidence::NONE
    };
    Ok(KeyEstimate {
        key: key_at(best),
        confidence,
        runner_up: key_at(second),
    })
}

/// `v` less its mean, and that vector's length.
fn center(v: &[f64; CLASSES]) -> ([f64; CLASSES], f64) {
    let mean = v.iter().sum::<f64>() / CLASSES as f64;
    let c = v.map(|x| x - mean);
    let norm = c.iter().map(|x| x * x).sum::<f64>().sqrt();
    (c, norm)
}

fn key_at(index: usize) -> Key {
    Key {
        tonic: PitchClass::from_semitones_above_c((index % CLASSES) as u8),
        mode: if index < CLASSES {
            Mode::Major
        } else {
            Mode::Minor
        },
    }
}

#[cfg(test)]
mod tests;
