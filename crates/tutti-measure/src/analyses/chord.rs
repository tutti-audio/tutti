//! Chord detection: major and minor triads from a chromagram.
//!
//! A frame is read in three steps.
//!
//! 1. **Floor removal.** The frame's median column is subtracted and what is
//!    left clamped at zero, so a triad standing over a diffuse floor of
//!    harmonics, leakage and noise reads as the triad. A frame with nothing
//!    left (silent, flat, or all but a few columns equal) has no pitch.
//! 2. **Templates.** Each of 24 binary triad templates (root, third, fifth)
//!    scores the cosine similarity with what is left.
//! 3. **No chord.** A fixed score of 0.85 stands for "no chord" in a frame that
//!    has pitch, and 1 in a frame that has none. A frame must therefore match
//!    a triad better than 0.85 to read as a chord. A single pitch class, which
//!    every triad containing it matches at `1/sqrt(3) = 0.577`, and a bare
//!    fifth or third, which the triads that contain it match at
//!    `sqrt(2/3) = 0.816`, are under it: they have no root or mode to name. A
//!    triad with a third at 0.15 of its root and fifth still reads, at 0.87.
//!
//! The 25 states are then smoothed by a Viterbi search in the manner of Mauch
//! (2010): an emission of `sharpness * score` per frame, a probability
//! [`self_transition`](ChordConfig::self_transition) of staying in a state and
//! the rest shared evenly among the other 24. A short flicker costs two
//! switches and a short chord pays for them, so the prior sets how many frames
//! a chord must last, which depends on the hop.
//!
//! The search is O(frames x states): with an even switch probability the best
//! way into a state is a stay or a switch from the best other state, found
//! from the two highest scores of the last frame ([`sticky_viterbi`]), and the
//! backpointers are two bytes per frame and state. Scores are `f64` and each
//! frame's are shifted so the best is 0, so a long input costs no precision:
//! unshifted, they grow by the emission each frame, and a few million frames
//! at a high sharpness would leave the stay cost under one unit in the last
//! place.
//!
//! The scoring is in `f64` and divides by the frame's own norm, so the result
//! does not depend on the chroma's scale, to the extremes of `f64`.
//!
//! # Time
//!
//! Frame `i` of a [`Framing`] covers `i * hop .. i * hop + window`, and
//! owns the samples up to the midpoints between its centre and its neighbours':
//! a change between frames `j - 1` and `j` is at `j * hop + (window - hop) / 2`.
//! A span that starts at frame 0 starts at sample 0, and one that ends at the
//! last frame ends at the end of the last window, `(frames - 1) * hop +
//! window`. A change at sample `T` between two sustained chords puts the end
//! of the old one and the start of the new one each within `(window + hop) / 2`
//! of it: frames whose windows hold only one chord, which are those centred
//! more than `window / 2` from `T`, read that chord and no other, so the old
//! chord's last frame is centred within `hop` of `T - window / 2` or later and
//! before `T + window / 2`, the new one's first likewise, and a boundary is
//! half a hop from a centre. Frames between may read neither, which leaves a
//! gap. Only triads are named: a seventh reads as its triad.
//!
//! The spans are placed on the framing the [`Chroma`] carries, so they sit at
//! the samples its frames were taken at whatever framing that was.
//!
//! Where no chord is detected there is no span.

use tutti_music::PitchClass;
use tutti_units::Samples;

use tutti_units::ranged;

use tutti_buffers::{Mono, Sample};
use tutti_units::SampleRate;

use crate::convert::Gathered;
use crate::framing::{Framing, check_frame_count};
use crate::meters::chroma::{CLASSES, Chroma, ChromaConfig, chroma};
use crate::{ChordError, Measure, MeterError, Region, StreamMeter};
use tutti_dsp::{FrameCount, FrameIndex, Grid, median, sticky_viterbi};

/// The most frames one call smooths, bounding the backpointers (50 bytes a
/// frame).
const MAX_FRAMES: FrameCount = FrameCount(1 << 19);

/// The states: 12 major roots, 12 minor roots, and "no chord".
const STATES: usize = 2 * CLASSES + 1;
/// [`STATES`] as the `u16` [`sticky_viterbi`] counts states in.
const STATE_COUNT: u16 = {
    assert!(STATES <= u16::MAX as usize);
    STATES as u16
};
const NO_CHORD: usize = 2 * CLASSES;

/// What the Viterbi scores are held in; wide, because they are summed over
/// every frame.
type Score = f64;

/// The score of "no chord" in a frame with pitch: the cosine similarity a frame
/// must beat to read as a triad. Between the 0.816 of a bare fifth and the 0.87
/// of a triad with a third at 0.15 of the others, and clear of the 0.577 of a
/// single pitch class.
const TRIAD_FLOOR: f64 = 0.85;

/// The notes of a triad, as offsets above its root.
const MAJOR: [usize; 3] = [0, 4, 7];
const MINOR: [usize; 3] = [0, 3, 7];

/// What kind of triad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChordQuality {
    /// Root, major third, fifth.
    Major,
    /// Root, minor third, fifth.
    Minor,
}

/// One chord and where it sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordSpan {
    /// The chord's root.
    pub root: PitchClass,
    /// Whether it is major or minor.
    pub quality: ChordQuality,
    /// The samples the chord sounds over.
    pub span: Region,
}

ranged! {
    /// The probability of staying in a chord from one frame to the next: finite
    /// and in `(0, 1)`. 1/25 is the even share, which leaves each frame to its
    /// own best state.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "f64", into = "f64")
    )]
    SelfTransition(f64), SelfTransitionError,
    "a self-transition probability within (0, 1)",
    |raw| raw > 0.0 && raw < 1.0
}

ranged! {
    /// How strongly a frame's similarity weighs against the transition prior:
    /// finite and in `(0, 100]`. The Viterbi scores are summed over every frame,
    /// so the upper bound keeps their sum within what `f64` resolves.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "f64", into = "f64")
    )]
    Sharpness(f64), SharpnessError, "a sharpness within (0, 100]",
    |raw| raw > 0.0 && raw <= 100.0
}

/// What [`detect_chords`] smooths with, and the chroma a [`ChordMeter`] reads the
/// chords from. Immutable once built. The framing the spans are placed on is
/// the chroma's own ([`Chroma::framing`](crate::Chroma::framing)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordConfig {
    chroma: ChromaConfig,
    self_transition: SelfTransition,
    sharpness: Sharpness,
}

impl ChordConfig {
    /// The default [`ChromaConfig`], a self-transition of 0.9 and a sharpness
    /// of 8: a chord must last about three frames to be kept.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chroma: ChromaConfig::new(),
            self_transition: SelfTransition::of(0.9),
            sharpness: Sharpness::of(8.0),
        }
    }

    /// The chroma a meter reads the chords from. [`detect_chords`] takes a
    /// chroma already made and ignores it.
    #[must_use]
    pub fn chroma(mut self, chroma: ChromaConfig) -> Self {
        self.chroma = chroma;
        self
    }

    /// The probability of staying in a state from one frame to the next.
    #[must_use]
    pub fn self_transition(mut self, probability: SelfTransition) -> Self {
        self.self_transition = probability;
        self
    }

    /// How strongly a frame's similarity weighs against the transition prior.
    #[must_use]
    pub fn sharpness(mut self, sharpness: Sharpness) -> Self {
        self.sharpness = sharpness;
        self
    }
}

impl Default for ChordConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Measure<S> for ChordConfig {
    type Context = SampleRate;
    type Meter = ChordMeter<S>;

    /// A meter that reads the chords of the audio at `rate`, once it has all
    /// of it.
    ///
    /// # Errors
    ///
    /// What [`ChromaConfig`] refuses at `rate`.
    fn measure(&self, rate: SampleRate) -> Result<ChordMeter<S>, MeterError> {
        self.chroma.plan(rate)?;
        Ok(ChordMeter {
            cfg: *self,
            rate,
            audio: Gathered::new(),
        })
    }
}

/// A running chord detection at one rate: gathers the audio and, at
/// [`finish`](StreamMeter::finish), takes its [`chroma()`] and reads the chords
/// from it with [`detect_chords`].
///
/// Not for the audio thread: it holds the whole stream, so it grows with the
/// audio and allocates.
#[derive(Debug)]
pub struct ChordMeter<S: Sample> {
    cfg: ChordConfig,
    rate: SampleRate,
    audio: Gathered<S>,
}

impl<S: Sample> StreamMeter for ChordMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Result<Vec<ChordSpan>, ChordError>;

    /// Gathers the next chunk of the audio; no chord is read before the end.
    fn push(&mut self, chunk: Mono<'_, S>, _: &mut impl Extend<()>) -> Result<(), MeterError> {
        self.audio.push(chunk);
        Ok(())
    }

    /// The chords of the audio gathered, or what its chroma or
    /// [`detect_chords`] refuses it for.
    fn finish(self) -> Result<Vec<ChordSpan>, ChordError> {
        let made = chroma(&self.cfg.chroma, Mono::new(self.audio.samples()), self.rate)?;
        detect_chords(&self.cfg, &made)
    }
}

/// The chords of a chromagram, as spans in order, placed at the samples of the
/// chroma's own framing. Where none is detected there is a gap.
///
/// # Errors
///
/// [`FramingError::ZeroWindow`](crate::FramingError::ZeroWindow) or [`FramingError::ZeroHop`](crate::FramingError::ZeroHop) for a zero window or hop in the
/// chroma's framing; [`ChordError::NotTwelveClasses`] when the grid is not 12 columns wide;
/// [`ChordError::NegativeInput`] when it holds a negative value;
/// [`ChordError::TooManyFrames`] for more than 524288 frames;
/// [`ChordError::SpanBeyondSampleCount`] when a span lies beyond the range of a
/// sample count; [`ChordError::NonFiniteInput`] for a NaN or infinity.
pub fn detect_chords(cfg: &ChordConfig, chroma: &Chroma) -> Result<Vec<ChordSpan>, ChordError> {
    detect(chroma.grid(), chroma.framing(), cfg)
}

/// [`detect_chords`] over a grid and the framing it was taken at.
pub(crate) fn detect(
    chroma: &Grid<f64>,
    framing: Framing,
    cfg: &ChordConfig,
) -> Result<Vec<ChordSpan>, ChordError> {
    framing.check()?;
    if chroma.bins().get() != CLASSES {
        return Err(ChordError::NotTwelveClasses {
            columns: chroma.bins().get(),
        });
    }
    check_frame_count(chroma.frames(), MAX_FRAMES)?;
    let frames = chroma.frames().get();
    if chroma.as_slice().iter().any(|v| !v.is_finite()) {
        return Err(ChordError::NonFiniteInput);
    }
    if chroma.as_slice().iter().any(|&v| v < 0.0) {
        return Err(ChordError::NegativeInput);
    }
    if frames == 0 {
        return Ok(Vec::new());
    }
    let path = viterbi(chroma, cfg);
    spans(&path, framing)
}

/// How well `frame` fits each state, in state order: the cosine similarity of
/// what stands above the frame's median column with each triad, and
/// [`TRIAD_FLOOR`] for "no chord". A frame with nothing above its median
/// scores 1 for "no chord" and 0 for the rest.
fn similarities(frame: &[f64]) -> [f64; STATES] {
    let mut sims = [0.0f64; STATES];
    let mut x: [f64; CLASSES] = core::array::from_fn(|i| frame[i]);
    let median = median(&x).unwrap_or_default();
    x.iter_mut().for_each(|v| *v = (*v - median).max(0.0));
    let norm = x.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm.is_nan() || norm <= 0.0 {
        sims[NO_CHORD] = 1.0;
        return sims;
    }
    for (state, sim) in sims.iter_mut().take(NO_CHORD).enumerate() {
        let (root, notes) = if state < CLASSES {
            (state, MAJOR)
        } else {
            (state - CLASSES, MINOR)
        };
        let dot: f64 = notes.iter().map(|n| x[(root + n) % CLASSES]).sum();
        *sim = dot / (norm * 3.0f64.sqrt());
    }
    sims[NO_CHORD] = TRIAD_FLOOR;
    sims
}

/// The best state per frame under the emission and transition model.
fn viterbi(chroma: &Grid<f64>, cfg: &ChordConfig) -> Vec<u16> {
    let stay = libm::log(Score::from(cfg.self_transition.get()));
    let switch = libm::log((1.0 - Score::from(cfg.self_transition.get())) / (STATES - 1) as Score);
    sticky_viterbi(
        STATE_COUNT,
        chroma.frames().get(),
        stay,
        switch,
        |t, row| {
            let scores = similarities(chroma.row(FrameIndex::new(t)));
            for (slot, s) in row.iter_mut().zip(scores) {
                *slot = s as Score * Score::from(cfg.sharpness.get());
            }
        },
    )
}

/// Runs of one chord in `path`, placed on `framing`'s samples.
fn spans(path: &[u16], framing: Framing) -> Result<Vec<ChordSpan>, ChordError> {
    let (window, hop) = (framing.window().get() as u128, framing.hop().get() as u128);
    let frames = path.len() as u128;
    let sample = |v: u128| usize::try_from(v).map_err(|_| ChordError::SpanBeyondSampleCount);
    // The change before frame `j`: the midpoint of two centres.
    let boundary = |j: usize| -> u128 { (2 * j as u128 * hop + window - hop) / 2 };
    let end = (frames - 1) * hop + window;
    sample(end)?;

    let mut out = Vec::new();
    let mut start = 0;
    for t in 1..=path.len() {
        if t < path.len() && path[t] == path[start] {
            continue;
        }
        let state = usize::from(path[start]);
        if state != NO_CHORD {
            let (root, quality) = if state < CLASSES {
                (state, ChordQuality::Major)
            } else {
                (state - CLASSES, ChordQuality::Minor)
            };
            out.push(ChordSpan {
                root: PitchClass::from_semitones_above_c(root as u8),
                quality,
                span: Region::new(
                    Samples::new(if start == 0 {
                        0
                    } else {
                        sample(boundary(start))?
                    }),
                    Samples::new(if t == path.len() {
                        sample(end)?
                    } else {
                        sample(boundary(t))?
                    }),
                )
                .ok_or(ChordError::SpanBeyondSampleCount)?,
            });
        }
        start = t;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
