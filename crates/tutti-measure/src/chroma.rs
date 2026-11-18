//! Chroma: the energy of a signal folded into the twelve pitch classes.
//!
//! The harmonic pitch class profile of Gomez (2006). Each frame of a
//! [`StftGeometry`] is windowed and transformed, the local maxima of the
//! magnitude spectrum between [`band`](ChromaConfig::band) are located to a fraction of a bin by
//! parabolic interpolation of the log magnitude, and each peak's energy (its
//! squared amplitude) is added to the pitch class of its frequency and of its
//! subharmonics `f/2 .. f/n`, each weighted by [`harmonic_decay`] to the power
//! of its order less one. A peak thereby also votes for the notes it may be
//! a partial of. A subharmonic below the band's floor is not a candidate
//! fundamental and does not vote.
//!
//! Column `k` of a frame is `PitchClass::ALL[k]`, so column 0 is C. A frame is
//! divided by its largest column, so each reads `0..=1` with a peak of exactly
//! 1: a plain `f32` rather than a unit, because no unit covers a relative
//! energy and `Confidence` is an estimator's certainty. A frame whose strongest
//! in-band partial is under -90 dBFS holds no pitch and reads all zero,
//! never NaN; the floor is absolute because a relative one would call the
//! rounding noise of a silent buffer a signal. Within a frame, a peak under
//! [`peak_floor`](ChromaConfig::peak_floor) of the frame's strongest is
//! ignored, which keeps window sidelobes out of the profile.
//!
//! # Tuning
//!
//! A peak `f` is `s = 12 log2(f / 440)` semitones from A4; its distance from
//! the nearest equal-tempered pitch is `s - round(s)`. The tuning is the
//! energy-weighted circular mean of that distance (a full turn per semitone),
//! in [`Cents`] in `-50..=50`: a recording that sits 30 cents sharp reads +30,
//! and folding subtracts it, so its notes land on their own classes. It is
//! the estimate over the whole buffer ([`chroma`] runs a first pass to find
//! it), or a [fixed](Tuning::Fixed) value. A peak is folded to the nearest
//! class: a signal tuned near a quarter tone is ambiguous, whatever the
//! estimate.
//!
//! The partials of a harmonic tone are not all on the grid: the third sits 2
//! cents sharp of an equal-tempered fifth, the fifth 14 cents flat of a major
//! third. The estimate is therefore a few cents off for a tone rich in
//! partials, which only matters near the quarter-tone ambiguity.
//!
//! # Streaming
//!
//! [`ChromaConfig::measure`] makes a [`ChromaMeter`] that takes the buffer in
//! chunks of any length, cuts its frames at the hop and keeps a running mean.
//! Its buffers are built on the first frame, so a push allocates nothing after
//! the first, but the first transform of the meter's scratch plans lazily. With
//! [`Tuning::Estimate`] a meter can only use the peaks seen so far, so early
//! frames fold with a rougher tuning than [`chroma`]'s whole-buffer one; with a
//! fixed tuning the two agree to the bit.
//!
//! [`harmonic_decay`]: ChromaConfig::harmonic_decay

use crate::level::SILENCE_FLOOR;
use core::marker::PhantomData;

use tutti_buffers::{Mono, Sample};
use tutti_types::Note;
use tutti_units::{Cents, Db, Hz, SampleRate, Samples, Semitones};

use crate::convert::{all_finite, for_blocks, to_f32};
use crate::framing::{FrameCutter, Framing, check_nyquist};
use crate::{Error, Measure, Meter, Result};
use tutti_dsp::Complex32;
use tutti_dsp::FftScratch;
use tutti_dsp::StftGeometry;
use tutti_dsp::{BinCount, FrameCount, Grid};
use tutti_units::ranged;

/// The pitch classes in a chroma frame.
pub(crate) const CLASSES: usize = 12;

/// The smallest and largest window accepted. The FFT works on the window, so
/// an absurd one would ask for gigabytes, and a tiny one resolves no pitch.
const MIN_WINDOW: usize = 64;
const MAX_WINDOW: usize = 1 << 18;

/// The most frames one call analyses, bounding the output (48 bytes a frame).
pub(crate) const MAX_FRAMES: usize = 1 << 19;

/// The most hops in one window. Each frame is a whole transform, so this
/// bounds the cost of a tiny hop.
const MAX_OVERLAP: usize = 64;

/// Smallest positive magnitude, standing in for an empty neighbour bin before
/// its logarithm is taken.
const TINY: f64 = 1e-30;

ranged! {
    /// How many partials a peak may be, itself included: `1..=16`. 1 folds a
    /// peak to its own class only.
    Harmonics(usize), HarmonicsError, "a harmonic count within 1..=16",
    |raw| raw >= 1 && raw <= 16
}

ranged! {
    /// The weight of a subharmonic, per order past the first: finite and in
    /// `(0, 1]`.
    HarmonicDecay(f32), HarmonicDecayError, "a harmonic decay within (0, 1]",
    |raw| raw > 0.0 && raw <= 1.0
}

ranged! {
    /// How far under a frame's strongest peak a peak is still counted: within
    /// `-120..=0` dB, the f32 range of a magnitude ratio.
    PeakFloor(Db), PeakFloorError, "a peak floor within -120..=0 dB",
    |raw| raw.get() >= -120.0 && raw.get() <= 0.0
}

ranged! {
    /// A fixed offset from A4 = 440 Hz, within `-50..=50` cents: the range a
    /// tuning is read in, since a peak folds to its nearest class.
    TuningOffset(Cents), TuningOffsetError, "a tuning offset within -50..=50 cents",
    |raw| raw.get() >= -50.0 && raw.get() <= 50.0
}

impl TuningOffset {
    /// In tune: no offset.
    pub const NONE: TuningOffset = TuningOffset(Cents(0.0));
}

/// The frequencies a peak may sit between: `min` below `max`, both finite and
/// above zero. Whether the band suits a sample rate and a window (under
/// Nyquist, three bins wide) is checked by [`chroma`] and [`ChromaConfig::measure`], which
/// know the geometry. `TryFrom` over `min..=max` names the reason a band is
/// refused; [`new`](Self::new) is the same check as an `Option`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrequencyBand {
    min: Hz,
    max: Hz,
}

impl FrequencyBand {
    /// The 50 to 2000 Hz vocal and instrument range.
    pub(crate) const VOICE: Self = Self {
        min: Hz(50.0),
        max: Hz(2000.0),
    };

    /// The band `min..=max`, or `None` where [`TryFrom`] would refuse it.
    #[must_use]
    pub fn new(min: impl Into<Hz>, max: impl Into<Hz>) -> Option<Self> {
        Self::try_from(min.into()..=max.into()).ok()
    }

    /// The lower edge.
    #[inline]
    #[must_use]
    pub(crate) fn min(&self) -> Hz {
        self.min
    }

    /// The upper edge.
    #[inline]
    #[must_use]
    pub(crate) fn max(&self) -> Hz {
        self.max
    }
}

impl TryFrom<core::ops::RangeInclusive<Hz>> for FrequencyBand {
    type Error = Error;

    /// # Errors
    ///
    /// [`Error::EmptyFrequencyRange`] when a bound is not finite and positive,
    /// or `min` is not below `max`.
    fn try_from(range: core::ops::RangeInclusive<Hz>) -> Result<Self> {
        let (min, max) = range.into_inner();
        if !(min.get().is_finite() && max.get().is_finite() && min.get() > 0.0 && min < max) {
            return Err(Error::EmptyFrequencyRange { min, max });
        }
        Ok(Self { min, max })
    }
}

/// How the tuning a frame is folded with is found.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Tuning {
    /// The circular mean of the peaks' distance from the equal-tempered grid.
    Estimate,
    /// This offset from A4 = 440 Hz.
    Fixed(TuningOffset),
}

/// What [`chroma`] and a [`ChromaMeter`] fold with. Immutable once built, and
/// free of a sample rate: the call that takes the audio derives the geometry
/// and checks the band against it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromaConfig {
    framing: Framing,
    band: Option<FrequencyBand>,
    harmonics: Harmonics,
    harmonic_decay: HarmonicDecay,
    peak_floor: PeakFloor,
    tuning: Tuning,
}

impl ChromaConfig {
    /// Folds the peaks of frames of 4096 samples every 1024 between 100 Hz and
    /// 5 kHz (or Nyquist, if lower), each voting for itself and three
    /// subharmonics weighted `0.6^(order - 1)`, ignoring peaks 40 dB under the
    /// frame's strongest, with an estimated tuning.
    #[must_use]
    pub fn new() -> Self {
        Self {
            framing: Framing::new(Samples(4096), Samples(1024)),
            band: None,
            harmonics: Harmonics::of(4),
            harmonic_decay: HarmonicDecay::of(0.6),
            peak_floor: PeakFloor::of(Db(-40.0)),
            tuning: Tuning::Estimate,
        }
    }

    /// The window and hop frames are taken at.
    ///
    /// The call that takes the audio refuses a window under 64 or over 262144
    /// samples ([`Error::WindowOutOfRange`]) and a hop under a sixty-fourth of
    /// the window ([`Error::TooManyOverlaps`]).
    #[must_use]
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    /// Folds only peaks inside `band`.
    ///
    /// The call that takes the audio refuses a band that exceeds half the
    /// sample rate ([`Error::AboveNyquist`]) or holds fewer than three bins
    /// ([`Error::EmptyFrequencyRange`]).
    #[must_use]
    pub fn band(mut self, band: FrequencyBand) -> Self {
        self.band = Some(band);
        self
    }

    /// How many partials a peak may be, itself included.
    #[must_use]
    pub fn harmonics(mut self, harmonics: Harmonics) -> Self {
        self.harmonics = harmonics;
        self
    }

    /// The weight of a subharmonic, per order past the first.
    #[must_use]
    pub fn harmonic_decay(mut self, decay: HarmonicDecay) -> Self {
        self.harmonic_decay = decay;
        self
    }

    /// Peaks this far under the frame's strongest are ignored.
    #[must_use]
    pub fn peak_floor(mut self, floor: PeakFloor) -> Self {
        self.peak_floor = floor;
        self
    }

    /// Folds with a fixed or an estimated tuning.
    #[must_use]
    pub fn tuning(mut self, tuning: Tuning) -> Self {
        self.tuning = tuning;
        self
    }
    /// The config at `rate`.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroWindow`] or [`Error::ZeroHop`] for a zero window or hop;
    /// [`Error::WindowOutOfRange`]
    /// for a window under 64 or over 262144 samples;
    /// [`Error::TooManyOverlaps`] for a hop under a sixty-fourth of the window;
    /// [`Error::EmptyFrequencyRange`] when the band holds fewer than three bins;
    /// [`Error::AboveNyquist`] when it exceeds half `rate`.
    fn plan(&self, rate: SampleRate) -> Result<ChromaPlan> {
        let geometry = self.framing.geometry(rate)?;
        let window = geometry.window().get();
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&window) {
            return Err(Error::WindowOutOfRange {
                window: Samples(window),
                min: Samples(MIN_WINDOW),
                max: Samples(MAX_WINDOW),
            });
        }
        if window.div_ceil(geometry.hop().get()) > MAX_OVERLAP {
            return Err(Error::TooManyOverlaps {
                window: Samples(window),
                hop: geometry.hop(),
                max: MAX_OVERLAP,
            });
        }
        let nyquist = rate.get() as f32 / 2.0;
        let (min_freq, max_freq) = match self.band {
            Some(band) => (band.min, band.max),
            None => (Hz(100.0), Hz(5000.0f32.min(nyquist))),
        };
        let plan = ChromaPlan {
            geometry,
            min_freq,
            max_freq,
            harmonics: self.harmonics,
            harmonic_decay: self.harmonic_decay,
            peak_floor: self.peak_floor,
            tuning: self.tuning,
        };
        plan.band_bins()?;
        Ok(plan)
    }
}

impl<S: Sample> Measure<S> for ChromaConfig {
    type Context = SampleRate;
    type Meter = ChromaMeter<S>;

    /// A meter that folds audio at `rate`, chunk by chunk.
    ///
    /// # Errors
    ///
    /// As [`chroma`], for what depends on the framing and the band at `rate`.
    fn measure(&self, rate: SampleRate) -> Result<ChromaMeter<S>> {
        Ok(ChromaMeter::new(self.plan(rate)?))
    }
}

impl Default for ChromaConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`ChromaConfig`] at one sample rate, its framing and band checked.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ChromaPlan {
    geometry: StftGeometry,
    min_freq: Hz,
    max_freq: Hz,
    harmonics: Harmonics,
    harmonic_decay: HarmonicDecay,
    peak_floor: PeakFloor,
    tuning: Tuning,
}

impl ChromaPlan {
    /// The bins a peak may sit on, `lo..=hi`, each with both neighbours inside
    /// the half spectrum; an error unless the band is valid and holds three.
    fn band_bins(&self) -> Result<(usize, usize)> {
        let (min, max) = (self.min_freq, self.max_freq);
        let empty = Error::EmptyFrequencyRange { min, max };
        if !(min.get().is_finite() && max.get().is_finite() && min.get() > 0.0 && min < max) {
            return Err(empty);
        }
        check_nyquist(max, self.geometry.sample_rate())?;
        let width = self.bin_width();
        let lo = ((f64::from(min.get()) / width).ceil() as usize).max(1);
        let hi = ((f64::from(max.get()) / width).floor() as usize)
            .min(self.geometry.window().get() / 2 - 1);
        if hi < lo + 2 {
            return Err(empty);
        }
        Ok((lo, hi))
    }

    /// The spacing of the bins, in hertz.
    fn bin_width(&self) -> f64 {
        self.geometry.sample_rate().get() / self.geometry.window().get() as f64
    }

    /// Calls `visit(frequency, energy)` for each peak of `magnitudes` that
    /// counts, and returns false, calling nothing, for a frame with no pitch.
    fn peaks(&self, magnitudes: &[f32], mut visit: impl FnMut(f64, f32)) -> bool {
        // The terminals check the band before any frame is folded.
        let Ok((lo, hi)) = self.band_bins() else {
            return false;
        };
        let top = magnitudes[lo..=hi].iter().copied().fold(0.0f32, f32::max);
        // A sine of amplitude `a` reads `a * magnitude_gain / 2`.
        let amplitude = 2.0 * top / self.geometry.magnitude_gain();
        if amplitude.is_nan() || amplitude < SILENCE_FLOOR.to_amplitude().get() {
            return false;
        }
        let floor = top * self.peak_floor.get().to_amplitude().get();
        let width = self.bin_width();
        for i in lo..=hi {
            let m = magnitudes[i];
            if !(m > magnitudes[i - 1] && m >= magnitudes[i + 1] && m >= floor) {
                continue;
            }
            let (offset, peak) = interpolate(magnitudes[i - 1], m, magnitudes[i + 1]);
            let frequency = (i as f64 + offset) * width;
            if frequency >= f64::from(self.min_freq.get())
                && frequency <= f64::from(self.max_freq.get())
            {
                visit(frequency, peak * peak);
            }
        }
        true
    }

    /// Adds a peak's distance from the equal-tempered grid to `sums`.
    fn tune(frequency: f64, energy: f32, sums: &mut TuningSums) {
        let off = off_grid(frequency);
        let turn = f64::from(off) * std::f64::consts::TAU;
        let w = f64::from(energy);
        sums.cos += w * libm::cos(turn);
        sums.sin += w * libm::sin(turn);
    }

    /// Folds a peak into the twelve classes at `tuning` semitones sharp.
    fn fold(&self, frequency: f64, energy: f32, tuning: Semitones, frame: &mut [f32; CLASSES]) {
        let mut weight = energy;
        for order in 1..=self.harmonics.get() {
            let sub = frequency / order as f64;
            if sub < f64::from(self.min_freq.get()) {
                break;
            }
            let class = class_of(sub, tuning);
            frame[class] += weight;
            weight *= self.harmonic_decay.get();
        }
    }
}

/// The half-semitone-wide distance of `frequency` from the nearest
/// equal-tempered pitch, in semitones, `-0.5..=0.5`.
fn off_grid(frequency: f64) -> f32 {
    let s = semitones_from_a4(frequency).get();
    s - s.round()
}

/// `frequency`'s distance from A4.
fn semitones_from_a4(frequency: f64) -> Semitones {
    Semitones::from_pitch_ratio((frequency / f64::from(Note::A4.frequency().get())) as f32)
}

/// The class of `frequency` once `tuning` is taken out, as a column.
fn class_of(frequency: f64, tuning: Semitones) -> usize {
    let a = f32::from(Note::A4.class().semitones_above_c());
    let position = a + semitones_from_a4(frequency).get() - tuning.get();
    (position.round() as i64).rem_euclid(CLASSES as i64) as usize
}

/// Parabolic interpolation of the log magnitude through three bins, whose
/// middle one is the peak: its offset from the middle bin, within
/// `-0.5..=0.5`, and the magnitude there.
fn interpolate(below: f32, mid: f32, above: f32) -> (f64, f32) {
    let ln = |m: f32| libm::log(f64::from(m).max(TINY));
    let (a, b, c) = (ln(below), ln(mid), ln(above));
    let curvature = a - 2.0 * b + c;
    if curvature.is_nan() || curvature >= 0.0 {
        return (0.0, mid);
    }
    let offset = (0.5 * (a - c) / curvature).clamp(-0.5, 0.5);
    (offset, libm::exp(b - 0.25 * (a - c) * offset) as f32)
}

/// The energy-weighted circular mean of the peaks' off-grid distances, kept as
/// its two components.
#[derive(Debug, Clone, Copy, Default)]
struct TuningSums {
    cos: f64,
    sin: f64,
}

impl TuningSums {
    /// The mean distance, within `-50..=50` cents; 0 with no peak. `atan2` is
    /// within a half turn, so the offset is within a half semitone.
    fn offset(self) -> TuningOffset {
        let turns = libm::atan2(self.sin, self.cos) / std::f64::consts::TAU;
        TuningOffset::new(Semitones(turns as f32).to_cents()).unwrap_or(TuningOffset::NONE)
    }

    fn cents(self) -> Cents {
        self.offset().get()
    }
}

/// A chromagram: a `frames x 12` grid and the tuning it was folded with.
#[derive(Debug, Clone, PartialEq)]
pub struct Chroma {
    grid: Grid<f32>,
    tuning: Cents,
    framing: Framing,
}

impl Chroma {
    /// A chroma of `grid` taken at `framing`, with `tuning`.
    #[cfg(test)]
    pub(crate) fn new(grid: Grid<f32>, tuning: Cents, framing: Framing) -> Self {
        Self {
            grid,
            tuning,
            framing,
        }
    }

    /// One row per frame, one column per pitch class from C, each `0..=1`.
    #[inline]
    #[must_use]
    pub fn grid(&self) -> &Grid<f32> {
        &self.grid
    }

    /// The tuning the frames were folded with: the estimate over the whole
    /// buffer, or the fixed value the config named.
    #[inline]
    #[must_use]
    pub fn tuning(&self) -> Cents {
        self.tuning
    }

    /// The mean of the frames, per class; all zero for no frames.
    #[must_use]
    pub fn mean(&self) -> [f32; CLASSES] {
        mean_of_rows(&self.grid)
    }

    /// The window and hop the frames were taken at.
    #[inline]
    #[must_use]
    pub fn framing(&self) -> Framing {
        self.framing
    }
}

/// The per-column mean of a `frames x 12` grid; all zero for no frames.
pub(crate) fn mean_of_rows(grid: &Grid<f32>) -> [f32; CLASSES] {
    let mut sum = [0.0f64; CLASSES];
    for row in grid.rows() {
        for (s, &v) in sum.iter_mut().zip(row) {
            *s += f64::from(v);
        }
    }
    mean_of(&sum, grid.frames().get())
}

fn mean_of(sum: &[f64; CLASSES], frames: usize) -> [f32; CLASSES] {
    let n = frames.max(1) as f64;
    sum.map(|s| (s / n) as f32)
}

/// What a run of frames carries: a running mean and the tuning sums, and the
/// buffers a frame is transformed in.
#[derive(Debug, Clone)]
struct ChromaFold {
    plan: ChromaPlan,
    window: Vec<f32>,
    spectrum: Vec<Complex32>,
    magnitudes: Vec<f32>,
    sum: [f64; CLASSES],
    frames: usize,
    sums: TuningSums,
}

impl ChromaFold {
    /// A state with every buffer built for `plan`.
    fn new(plan: ChromaPlan) -> Self {
        let window = plan.geometry.window().get();
        Self {
            plan,
            window: plan.geometry.window_coefficients(),
            spectrum: vec![Complex32::default(); window / 2 + 1],
            magnitudes: vec![0.0; window / 2 + 1],
            sum: [0.0; CLASSES],
            frames: 0,
            sums: TuningSums::default(),
        }
    }

    /// Forgets the carried frames and tuning, keeping the buffers.
    fn reset(&mut self) {
        self.sum = [0.0; CLASSES];
        self.frames = 0;
        self.sums = TuningSums::default();
    }

    /// The mean of the frames folded so far, per class.
    fn mean(&self) -> [f32; CLASSES] {
        mean_of(&self.sum, self.frames)
    }

    /// The tuning in use: the plan's fixed value, or the estimate from the
    /// peaks seen so far (0 before any).
    fn tuning(&self) -> Cents {
        match self.plan.tuning {
            Tuning::Fixed(offset) => offset.get(),
            Tuning::Estimate => self.sums.cents(),
        }
    }

    /// Leaves the magnitudes of `samples`, one frame, in `self.magnitudes`.
    fn transform(&mut self, samples: &[f32], fft: &mut FftScratch) {
        fft.forward(Mono::new(samples), &self.window, &mut self.spectrum);
        for (m, bin) in self.magnitudes.iter_mut().zip(&self.spectrum) {
            *m = bin.norm();
        }
    }

    /// Folds one frame of `samples` (exactly one finite window) and adds it to
    /// the running mean. Returns the frame's chroma, `0..=1` per class, zero
    /// for a frame with no pitch.
    fn fold_frame(&mut self, samples: &[f32], fft: &mut FftScratch) -> [f32; CLASSES] {
        self.transform(samples, fft);
        let plan = self.plan;

        let mut frame = [0.0f32; CLASSES];
        let tuning = match plan.tuning {
            Tuning::Fixed(offset) => offset.get(),
            Tuning::Estimate => {
                let sums = &mut self.sums;
                plan.peaks(&self.magnitudes, |f, e| ChromaPlan::tune(f, e, sums));
                sums.cents()
            }
        };
        let tuning = tuning.to_semitones();
        plan.peaks(&self.magnitudes, |f, e| plan.fold(f, e, tuning, &mut frame));

        let max = frame.iter().copied().fold(0.0f32, f32::max);
        if max > 0.0 {
            frame.iter_mut().for_each(|v| *v /= max);
        } else {
            frame = [0.0; CLASSES];
        }
        for (s, &v) in self.sum.iter_mut().zip(&frame) {
            *s += f64::from(v);
        }
        self.frames += 1;
        frame
    }
}

/// One analysed frame of a [`ChromaMeter`]: its chroma and where it starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromaFrame {
    /// The frame's first sample, in [`Samples`] of the stream.
    pub frame: Samples,
    /// The frame's chroma, `0..=1` per pitch class from C; all zero for a frame
    /// with no pitch.
    pub bins: [f32; CLASSES],
}

/// The reading of a [`ChromaMeter`]: the mean of the frames folded and the
/// tuning in use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromaSummary {
    /// The mean of the frames, per pitch class from C; all zero before the
    /// first frame.
    pub mean: [f32; CLASSES],
    /// The tuning in use: the config's fixed value, or the estimate from the
    /// peaks seen.
    pub tuning: Cents,
}

/// A running chroma at one rate: fed chunks of the buffer in order, of any
/// length, it cuts the frames at the hop, yields each frame's chroma as an
/// item, folds each into a running mean and reports the mean and the tuning so
/// far.
///
/// The reading, [`mean`](Self::mean) and [`tuning`](Self::tuning), is
/// available after any push, and [`finish`](Meter::finish) returns both as a
/// [`ChromaSummary`]. With
/// [`Tuning::Estimate`] the tuning is the estimate from the peaks seen so far;
/// with a fixed tuning the frames equal [`chroma`]'s bit for bit.
///
/// A push allocates nothing once the meter is made (the transform is planned
/// there), and its cost is a transform per frame: fit for a consumer thread
/// that drains a tap, not for an audio callback.
#[derive(Debug)]
pub struct ChromaMeter<S: Sample = f32> {
    fold: ChromaFold,
    cutter: FrameCutter,
    fft: FftScratch,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> ChromaMeter<S> {
    fn new(plan: ChromaPlan) -> Self {
        let mut fft = FftScratch::new();
        let fold = ChromaFold::new(plan);
        // Plans the transform and sizes its scratch, so a push allocates
        // nothing.
        let window = plan.geometry.window().get();
        let mut spectrum = vec![Complex32::default(); window / 2 + 1];
        fft.forward(Mono::new(&vec![0.0; window]), &fold.window, &mut spectrum);
        Self {
            cutter: FrameCutter::new(Framing::new(plan.geometry.window(), plan.geometry.hop())),
            fold,
            fft,
            sample: PhantomData,
        }
    }

    /// Takes finite samples and folds each frame they complete, calling `each`
    /// with the frame's chroma.
    fn feed(&mut self, block: &[f32], mut each: impl FnMut(ChromaFrame)) {
        let Self {
            fold, cutter, fft, ..
        } = self;
        let hop = fold.plan.geometry.hop().get();
        cutter.feed(block, |index, samples| {
            each(ChromaFrame {
                frame: Samples(index * hop),
                bins: fold.fold_frame(samples, fft),
            });
        });
    }

    /// The mean of the frames folded so far, per class; all zero before the
    /// first.
    #[must_use]
    pub fn mean(&self) -> [f32; CLASSES] {
        self.fold.mean()
    }

    /// The tuning in use: the config's fixed value, or the estimate from the
    /// peaks seen so far (0 before any).
    #[must_use]
    pub fn tuning(&self) -> Cents {
        self.fold.tuning()
    }

    /// Forgets the carried frames and tuning, keeping the buffers: the next
    /// push allocates nothing. Reads like a fresh meter.
    pub fn reset(&mut self) {
        self.fold.reset();
        self.cutter.reset();
    }
}

impl<S: Sample> Meter for ChromaMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ChromaFrame;
    type Output = ChromaSummary;

    /// Feeds the next chunk of the buffer, in order, folds each frame it
    /// completes and extends `out` with that frame's chroma.
    ///
    /// # Errors
    ///
    /// [`Error::NonFiniteInput`] when the chunk holds a NaN or infinity. The
    /// meter is untouched on error.
    fn push(&mut self, chunk: Mono<'_, S>, out: &mut impl Extend<ChromaFrame>) -> Result<()> {
        if !all_finite(chunk.samples()) {
            return Err(Error::NonFiniteInput);
        }
        for_blocks(chunk.samples(), 1, |block| {
            self.feed(block, |frame| out.extend([frame]));
        });
        Ok(())
    }

    /// The mean of the frames folded and the tuning in use.
    fn finish(self) -> ChromaSummary {
        ChromaSummary {
            mean: self.mean(),
            tuning: self.tuning(),
        }
    }
}

/// The chromagram of a whole buffer at `rate`.
///
/// With a fixed tuning this feeds one [`ChromaMeter`] the buffer, so streaming
/// it gives these frames bit for bit. With [`Tuning::Estimate`] it is the one
/// exception to "the one-shot is a meter fed the buffer": the buffer is
/// transformed twice, once to find the tuning of all its peaks and once, in a
/// meter with that tuning fixed, to fold with it, so a streaming estimate (which
/// can only use the peaks seen so far) differs from it in its early frames.
/// Frame `i` covers samples `i * hop .. i * hop + window`.
///
/// # Errors
///
/// [`Error::EmptyFrequencyRange`] or [`Error::AboveNyquist`] when the band does
/// not suit the framing and the rate (see [`ChromaConfig::band`]);
/// [`Error::WindowOutOfRange`] or [`Error::TooManyOverlaps`] when the framing
/// is not one a chroma accepts;
/// [`Error::NonFiniteInput`] for a NaN or infinite sample;
/// [`Error::InsufficientInput`] for less than one window;
/// [`Error::TooManyFrames`] for more than 524288 frames.
pub fn chroma<S: Sample>(
    cfg: &ChromaConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
    fft: &mut FftScratch,
) -> Result<Chroma> {
    let plan = cfg.plan(rate.into())?;
    if !all_finite(samples.samples()) {
        return Err(Error::NonFiniteInput);
    }
    let samples = to_f32(samples.samples());
    let window = plan.geometry.window().get();
    if samples.len() < window {
        return Err(Error::InsufficientInput {
            needed: Samples(window),
            got: Samples(samples.len()),
        });
    }
    let frames = plan.geometry.frames_for(Samples(samples.len()));
    if frames.get() > MAX_FRAMES {
        return Err(Error::TooManyFrames {
            frames,
            max: FrameCount(MAX_FRAMES),
        });
    }
    let frame_at = |i: usize| {
        let at = i * plan.geometry.hop().get();
        &samples[at..at + window]
    };

    let tuning = match plan.tuning {
        Tuning::Fixed(offset) => offset,
        Tuning::Estimate => {
            let mut state = ChromaFold::new(plan);
            for i in 0..frames.get() {
                state.transform(frame_at(i), fft);
                let sums = &mut state.sums;
                plan.peaks(&state.magnitudes, |f, e| ChromaPlan::tune(f, e, sums));
            }
            state.sums.offset()
        }
    };

    let fixed = ChromaPlan {
        tuning: Tuning::Fixed(tuning),
        ..plan
    };
    let mut meter: ChromaMeter = ChromaMeter::new(fixed);
    core::mem::swap(&mut meter.fft, fft);
    let mut data = Vec::with_capacity(frames.get() * CLASSES);
    meter.feed(&samples, |frame| data.extend_from_slice(&frame.bins));
    core::mem::swap(&mut meter.fft, fft);
    Ok(Chroma {
        grid: Grid::new(data, frames, BinCount(CLASSES))?,
        tuning: tuning.get(),
        framing: cfg.framing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold_uneven;

    const RATE: SampleRate = SampleRate::SR_22K05;

    fn geometry(window: usize, hop: usize) -> StftGeometry {
        StftGeometry::new(RATE, Samples(window), Samples(hop)).unwrap()
    }

    fn framing(window: usize, hop: usize) -> Framing {
        Framing::new(Samples(window), Samples(hop))
    }

    fn sine(freq: f64, len: usize, amp: f64) -> Vec<f32> {
        (0..len)
            .map(|i| (amp * (std::f64::consts::TAU * freq * i as f64 / 22_050.0).sin()) as f32)
            .collect()
    }

    /// Largest error of the interpolated peak position, in bins, over a sweep
    /// of a sine across one bin.
    fn worst_offset_error(interp: impl Fn(f32, f32, f32) -> (f64, f32)) -> f64 {
        let g = geometry(4096, 4096);
        let (window, mut fft) = (g.window_coefficients(), FftScratch::new());
        let mut worst = 0.0f64;
        for step in 0..40 {
            let bin = 100.0 + f64::from(step) / 40.0;
            let x = sine(bin * 22_050.0 / 4096.0, 4096, 0.5);
            let mut spectrum = vec![Complex32::default(); 2049];
            fft.forward(Mono::new(&x), &window, &mut spectrum);
            let mags: Vec<f32> = spectrum.iter().map(|b| b.norm()).collect();
            let top = (1..2048).fold(1, |b, i| if mags[i] > mags[b] { i } else { b });
            let (offset, _) = interp(mags[top - 1], mags[top], mags[top + 1]);
            worst = worst.max((top as f64 + offset - bin).abs());
        }
        worst
    }

    /// Parabolic interpolation of the log magnitude places a Hann peak within
    /// 0.02 bin (measured worst 0.016) wherever it falls in a bin; the raw
    /// peak bin is up to half a bin off.
    #[test]
    fn interpolation_places_a_peak_to_a_fiftieth_of_a_bin() {
        assert!(worst_offset_error(interpolate) <= INTERP_BIAS_BINS);
        assert!(worst_offset_error(|_, m, _| (0.0, m)) > 10.0 * INTERP_BIAS_BINS);
    }

    /// The bin error bound above, as a fraction of a bin.
    const INTERP_BIAS_BINS: f64 = 0.02;

    fn run(x: &[f32], cfg: &ChromaConfig) -> Chroma {
        chroma(cfg, Mono::new(x), RATE, &mut FftScratch::new()).unwrap()
    }

    fn cfg() -> ChromaConfig {
        ChromaConfig::new()
    }

    /// Silence is all zero and finite, a sine under -90 dBFS is silence and one
    /// above is not, and no frame is NaN.
    #[test]
    fn a_silent_frame_is_zero_not_nan() {
        let silent = run(&vec![0.0; 8192], &cfg());
        assert!(silent.grid().as_slice().iter().all(|&v| v == 0.0));
        assert_eq!(silent.tuning(), Cents(0.0));
        let quiet = run(&sine(440.0, 8192, 1e-5), &cfg());
        assert!(quiet.grid().as_slice().iter().all(|&v| v == 0.0));
        let audible = run(&sine(440.0, 8192, 1e-3), &cfg());
        assert!(audible.grid().as_slice().iter().any(|&v| v > 0.0));
    }

    /// A pitched frame's largest column is exactly 1, whatever the level.
    #[test]
    fn a_frame_is_normalized_by_its_largest_column() {
        for level in [0.1, 0.9] {
            let c = run(&sine(440.0, 8192, level), &cfg());
            for row in c.grid().rows() {
                assert_eq!(row.iter().copied().fold(0.0, f32::max), 1.0);
                assert!(row.iter().all(|&v| (0.0..=1.0).contains(&v)));
            }
        }
    }

    /// A sine at G4 (392 Hz) votes for G (itself and its octave, 196 Hz) and
    /// C (a third of it, 130.7 Hz); the fourth subharmonic, 98 Hz, is under the
    /// band's 100 Hz floor and does not vote. With decay 0.6 the energies are
    /// G = 1 + 0.6, C = 0.6^2, so C reads `0.36 / 1.6 = 0.225`.
    #[test]
    fn a_peak_votes_for_its_subharmonics_down_to_the_band_floor() {
        let c = run(&sine(392.0, 8192, 0.5), &cfg()).mean();
        assert!((c[7] - 1.0).abs() < 1e-6);
        assert!((c[0] - 0.225).abs() < 0.01, "{c:?}");
        for (i, &v) in c.iter().enumerate() {
            if i != 0 && i != 7 {
                assert!(v < 0.01, "{c:?}");
            }
        }
    }

    /// Partials outside the band do not fold.
    #[test]
    fn peaks_outside_the_band_are_not_folded() {
        for freq in [80.0, 6000.0] {
            let c = run(&sine(freq, 8192, 0.5), &cfg());
            assert!(c.grid().as_slice().iter().all(|&v| v == 0.0), "{freq}");
        }
    }

    /// A subharmonic's weight is the decay per order: at 0.3 the G4 sine's C
    /// (a third of it) reads `0.09 / 1.3`.
    #[test]
    fn the_harmonic_decay_weighs_the_subharmonic_votes() {
        let cfg = cfg().harmonic_decay(HarmonicDecay::of(0.3));
        let c = run(&sine(392.0, 8192, 0.5), &cfg).mean();
        assert!((c[7] - 1.0).abs() < 1e-6);
        assert!((c[0] - 0.09 / 1.3).abs() < 0.01, "{c:?}");
    }

    /// A peak under the floor, relative to the frame's strongest, is ignored:
    /// a sine 50 dB under another votes at a -80 dB floor and not at -40.
    #[test]
    fn peaks_under_the_floor_do_not_vote() {
        let strong = sine(440.0, 8192, 0.5);
        let x: Vec<f32> = strong
            .iter()
            .zip(sine(330.0, 8192, 0.5 * 10f64.powf(-50.0 / 20.0)))
            .map(|(a, b)| a + b)
            .collect();
        let only = cfg().harmonics(Harmonics::of(1));
        let ignored = run(&x, &only.peak_floor(PeakFloor::of(Db(-40.0)))).mean();
        assert_eq!(ignored[4], 0.0);
        let heard = run(&x, &only.peak_floor(PeakFloor::of(Db(-80.0)))).mean();
        assert!(heard[4] > 0.0, "{heard:?}");
    }

    /// The band limits which peaks fold: a 440 Hz sine is heard in a band
    /// around it and not in one above it.
    #[test]
    fn the_band_selects_the_peaks_that_fold() {
        let x = sine(440.0, 8192, 0.5);
        let inside = cfg().band(FrequencyBand::new(Hz(300.0), Hz(600.0)).unwrap());
        assert_eq!(run(&x, &inside).mean()[9], 1.0);
        let above = cfg().band(FrequencyBand::new(Hz(600.0), Hz(2000.0)).unwrap());
        assert!(run(&x, &above).grid().as_slice().iter().all(|&v| v == 0.0));
    }

    /// A sine's tuning is its distance from the grid, within the interpolation
    /// bias at its frequency: 0.02 bin is `1200 log2(1 + 0.02 w / f)` cents,
    /// `w = 22050 / 4096` Hz. The mean wraps: +45 and -55 are the same.
    #[test]
    fn the_tuning_of_a_sine_is_its_distance_from_the_grid() {
        let width = 22_050.0 / 4096.0;
        let tolerance = 1200.0 * (1.0 + INTERP_BIAS_BINS * width / 440.0).log2();
        for cents in [-40.0, -20.0, 0.0, 20.0, 40.0, 55.0] {
            let freq = 440.0 * (cents / 1200.0f64).exp2();
            let got = run(&sine(freq, 8192, 0.5), &cfg()).tuning().get();
            let want = if cents > 50.0 { cents - 100.0 } else { cents };
            assert!((f64::from(got) - want).abs() <= tolerance, "{cents}: {got}");
        }
    }

    /// The tuning is subtracted before a peak is rounded to a class: a sine
    /// 30 cents above A folds to A at +30 and to A# at -30 (60 cents up).
    #[test]
    fn a_fixed_tuning_moves_the_fold() {
        let x = sine(440.0 * (30.0f64 / 1200.0).exp2(), 8192, 0.5);
        let at = |cents: f32| {
            let cfg = cfg().tuning(Tuning::Fixed(TuningOffset::new(Cents(cents)).unwrap()));
            let c = run(&x, &cfg);
            assert_eq!(c.tuning(), Cents(cents));
            c.mean()
        };
        assert_eq!(at(30.0)[9], 1.0);
        assert_eq!(at(-30.0)[10], 1.0);
    }

    /// A meter fed chunks of 1, an odd count, a window, a hop-and-a-bit and the
    /// whole buffer at the batch's tuning gives the batch's last frame, mean
    /// and frame count to the bit.
    #[test]
    fn a_meter_at_a_fixed_tuning_is_the_one_shot() {
        let x: Vec<f32> = sine(440.0, 12_000, 0.5)
            .iter()
            .zip(sine(660.0, 12_000, 0.3))
            .map(|(a, b)| a + b)
            .collect();
        let batch = run(&x, &cfg());
        let fixed = cfg().tuning(Tuning::Fixed(TuningOffset::new(batch.tuning()).unwrap()));
        // At the batch's tuning, the one-shot is the meter's reference.
        assert_eq!(run(&x, &fixed).grid(), batch.grid());
        for chunk in [1usize, 777, 4096, 5000, x.len()] {
            let mut meter = fixed.measure(RATE).unwrap();
            let mut items = Vec::new();
            for part in x.chunks(chunk) {
                meter.push(Mono::new(part), &mut items).unwrap();
            }
            assert_eq!(items.len(), batch.grid().frames().get(), "chunk {chunk}");
            assert!(
                items
                    .iter()
                    .zip(batch.grid().rows())
                    .all(|(a, b)| a.bins == b)
            );
            assert_eq!(meter.mean(), batch.mean(), "chunk {chunk}");
            assert_eq!(meter.tuning(), batch.tuning(), "chunk {chunk}");
        }
        let mut meter = fixed.measure(RATE).unwrap();
        meter.push(Mono::new(&x), &mut Vec::new()).unwrap();
        meter.reset();
        assert_eq!(meter.mean(), [0.0; CLASSES]);
        assert_eq!(meter.tuning(), batch.tuning());
    }

    /// Under an estimated tuning a meter's reading depends on what it has
    /// seen, never on how the stream was cut: chunks of 1, an odd count and the
    /// whole buffer give the same mean and tuning bit for bit.
    #[test]
    fn an_estimating_meter_does_not_depend_on_the_chunking() {
        let x: Vec<f32> = sine(440.0 * (25.0f64 / 1200.0).exp2(), 12_000, 0.5)
            .iter()
            .zip(sine(660.0, 12_000, 0.3))
            .map(|(a, b)| a + b)
            .collect();
        let read = |chunk: usize| {
            let mut meter = cfg().measure(RATE).unwrap();
            for part in x.chunks(chunk) {
                meter.push(Mono::new(part), &mut Vec::new()).unwrap();
            }
            (meter.mean(), meter.tuning())
        };
        let whole = read(x.len());
        assert!(whole.1.get() != 0.0, "the estimate read the peaks");
        for chunk in [1usize, 777] {
            assert_eq!(read(chunk), whole, "chunk {chunk}");
        }
    }

    /// With an estimated tuning the meter reads the peaks seen so far.
    #[test]
    fn a_running_estimate_follows_the_peaks_seen() {
        let x = sine(440.0 * (25.0f64 / 1200.0).exp2(), 4096, 0.5);
        let mut meter = cfg().measure(RATE).unwrap();
        assert_eq!(meter.tuning(), Cents(0.0));
        let mut frames = Vec::new();
        meter.push(Mono::new(&x), &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert!((meter.tuning().get() - 25.0).abs() < 1.0);
    }

    /// A refused chunk leaves the carry alone, and `reset` reads like a fresh
    /// meter.
    #[test]
    fn a_refused_chunk_keeps_the_carry_and_reset_reads_fresh() {
        let x = sine(440.0 * (25.0f64 / 1200.0).exp2(), 4096, 0.5);
        let mut meter = cfg().measure(RATE).unwrap();
        meter.push(Mono::new(&x), &mut Vec::new()).unwrap();
        let (mean, tuning) = (meter.mean(), meter.tuning());
        let mut nan = x.clone();
        nan[7] = f32::NAN;
        assert_eq!(
            meter.push(Mono::new(&nan), &mut Vec::new()),
            Err(Error::NonFiniteInput)
        );
        assert_eq!((meter.mean(), meter.tuning()), (mean, tuning));

        meter.reset();
        assert_eq!(meter.mean(), [0.0; CLASSES]);
        assert_eq!(meter.tuning(), Cents(0.0));
    }

    /// The framing is validated against what the fold can afford, and the band
    /// against the bins it holds, where the rate binds.
    #[test]
    fn the_framing_and_band_are_validated() {
        let at = |window, hop| Measure::<f32>::measure(&cfg().framing(framing(window, hop)), RATE);
        assert!(matches!(at(32, 8), Err(Error::WindowOutOfRange { .. })));
        assert!(matches!(
            at(1 << 19, 1 << 17),
            Err(Error::WindowOutOfRange { .. })
        ));
        assert!(matches!(at(4096, 32), Err(Error::TooManyOverlaps { .. })));
        assert!(at(4096, 64).is_ok());
        assert!(matches!(at(0, 8), Err(Error::ZeroWindow | Error::ZeroHop)));
        assert!(matches!(
            at(4096, 0),
            Err(Error::ZeroWindow | Error::ZeroHop)
        ));
        for (lo, hi) in [
            (500.0, 100.0),
            (0.0, 1000.0),
            (f32::NAN, 1000.0),
            (100.0, f32::NAN),
            (100.0, f32::INFINITY),
        ] {
            assert!(
                matches!(
                    FrequencyBand::try_from(Hz(lo)..=Hz(hi)),
                    Err(Error::EmptyFrequencyRange { .. })
                ),
                "{lo} {hi}"
            );
        }
        // The rate decides what the terminals accept: a band of under three
        // bins, or one above Nyquist.
        let x = sine(440.0, 8192, 0.5);
        let mut fft = FftScratch::new();
        let narrow = cfg().band(FrequencyBand::new(Hz(100.0), Hz(110.0)).unwrap());
        assert!(matches!(
            chroma(&narrow, Mono::new(&x), RATE, &mut fft),
            Err(Error::EmptyFrequencyRange { .. })
        ));
        let wide = cfg().band(FrequencyBand::new(Hz(100.0), Hz(12_000.0)).unwrap());
        assert!(matches!(
            chroma(&wide, Mono::new(&x), RATE, &mut fft),
            Err(Error::AboveNyquist { .. })
        ));
        assert!(matches!(
            Measure::<f32>::measure(&wide, RATE),
            Err(Error::AboveNyquist { .. })
        ));
        // A low rate clamps the default band to Nyquist instead of failing.
        let low = cfg()
            .framing(framing(1024, 256))
            .plan(SampleRate::SR_8K)
            .unwrap();
        assert_eq!(low.max_freq, Hz(4000.0));
    }

    /// Bad input is an error, and a refused call computes nothing.
    #[test]
    fn bad_input_is_refused() {
        let mut fft = FftScratch::new();
        let mut x = sine(440.0, 8192, 0.5);
        let one_shot = |x: &[f32], cfg: &ChromaConfig, fft: &mut FftScratch| {
            chroma(cfg, Mono::new(x), RATE, fft)
        };
        x[100] = f32::NAN;
        assert_eq!(
            one_shot(&x, &cfg(), &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
        x[100] = f32::INFINITY;
        assert_eq!(
            one_shot(&x, &cfg(), &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
        x[100] = 0.25;
        assert_eq!(
            one_shot(&x[..4095], &cfg(), &mut fft).unwrap_err(),
            Error::InsufficientInput {
                needed: Samples(4096),
                got: Samples(4095)
            }
        );
        let tiny = cfg().framing(framing(64, 1));
        let long = vec![0.0; MAX_FRAMES + 64];
        assert!(matches!(
            one_shot(&long, &tiny, &mut fft),
            Err(Error::TooManyFrames { .. })
        ));
        // A NaN between two frames is in no window but is still refused.
        let sparse = cfg().framing(framing(64, 128));
        let mut gap = sine(440.0, 512, 0.5);
        gap[100] = f32::NAN;
        assert_eq!(
            one_shot(&gap, &sparse, &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
    }

    /// A 16-bit buffer folds at its nominal level: the chroma of the integer
    /// copy of a sine peaks on the sine's class.
    #[test]
    fn an_integer_buffer_folds_at_its_nominal_level() {
        let x: Vec<i16> = sine(440.0, 8192, 0.5)
            .iter()
            .map(|&v| (v * f32::from(i16::MAX)) as i16)
            .collect();
        let c = chroma(&cfg(), Mono::new(&x), RATE, &mut FftScratch::new()).unwrap();
        assert_eq!(c.mean()[9], 1.0);
    }

    /// The trait fold over uneven chunks (1, 7, 64 samples and the rest) yields
    /// the chroma rows of `chroma` as its items, and their mean and the
    /// tuning as its output, at a fixed tuning.
    #[test]
    fn the_trait_fold_over_uneven_chunks_equals_the_chroma_rows() {
        let x = sine(440.0, 30_000, 0.5);
        let fixed = cfg().tuning(Tuning::Fixed(TuningOffset::new(Cents(0.0)).unwrap()));
        let batch = run(&x, &fixed);
        let (items, summary) = fold_uneven(&fixed, RATE, x.len(), |meter, at, out| {
            meter.push(Mono::new(&x[at]), out)
        })
        .unwrap();
        assert_eq!(items.len(), batch.grid().frames().get());
        assert!(items.len() > 3);
        assert!(
            items
                .iter()
                .zip(batch.grid().rows())
                .all(|(a, b)| a.bins == b)
        );
        let hop = batch.framing().hop().get();
        assert!(
            items
                .iter()
                .enumerate()
                .all(|(i, f)| f.frame == Samples(i * hop)),
            "each frame names the sample it starts at"
        );
        assert_eq!(summary.mean, batch.mean());
        assert_eq!(summary.tuning, batch.tuning());
    }
}
