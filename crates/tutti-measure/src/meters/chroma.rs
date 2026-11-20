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
//! 1: a plain `f64` rather than a unit, because no unit covers a relative
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
use core::ops::RangeInclusive;
use tutti_dsp::local_maxima;

use tutti_buffers::{Mono, Sample};
use tutti_music::Note;
use tutti_units::{Cents, Db, Hz, SampleRate, Samples, Semitones};

use crate::convert::{all_finite, for_blocks, widen};
use crate::framing::{
    FrameCutter, Framing, check_frame_count, check_nyquist, check_overlap, check_window,
};
use crate::{Error, Measure, Meter, Result};
use tutti_dsp::Complex64;
use tutti_dsp::FftScratch;
use tutti_dsp::StftGeometry;
use tutti_dsp::parabolic_peak;
use tutti_dsp::{BinCount, FrameCount, Grid};
use tutti_units::ranged;

/// The pitch classes in a chroma frame.
pub(crate) const CLASSES: usize = 12;

/// The smallest and largest window accepted. The FFT works on the window, so
/// an absurd one would ask for gigabytes, and a tiny one resolves no pitch.
const WINDOWS: RangeInclusive<Samples> = Samples::new(64)..=Samples::new(1 << 18);

/// The most frames one call analyses, bounding the output (48 bytes a frame).
pub(crate) const MAX_FRAMES: FrameCount = FrameCount(1 << 19);

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
    HarmonicDecay(f64), HarmonicDecayError, "a harmonic decay within (0, 1]",
    |raw| raw > 0.0 && raw <= 1.0
}

ranged! {
    /// How far under a frame's strongest peak a peak is still counted: within
    /// `-120..=0` dB, a practical bound: a peak a million times under the
    /// strongest is below any spectrum's resolution.
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
    pub const NONE: TuningOffset = TuningOffset(Cents::new(0.0));
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
        min: Hz::new(50.0),
        max: Hz::new(2000.0),
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
            framing: Framing::new(Samples::new(4096), Samples::new(1024)),
            band: None,
            harmonics: Harmonics::of(4),
            harmonic_decay: HarmonicDecay::of(0.6),
            peak_floor: PeakFloor::of(Db::new(-40.0)),
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
        check_window(geometry.window(), &WINDOWS)?;
        check_overlap(geometry.window(), geometry.hop())?;
        let (min_freq, max_freq) = match self.band {
            Some(band) => (band.min, band.max),
            None => (Hz::new(100.0), Hz::new(5000.0).min(rate.nyquist())),
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
        let lo = (self.geometry.bin_of(min).ceil() as usize).max(1);
        let hi =
            (self.geometry.bin_of(max).floor() as usize).min(self.geometry.window().get() / 2 - 1);
        if hi < lo + 2 {
            return Err(empty);
        }
        Ok((lo, hi))
    }

    /// Calls `visit(frequency, energy)` for each peak of `magnitudes` that
    /// counts, and returns false, calling nothing, for a frame with no pitch.
    fn peaks(&self, magnitudes: &[f64], mut visit: impl FnMut(f64, f64)) -> bool {
        // The terminals check the band before any frame is folded.
        let Ok((lo, hi)) = self.band_bins() else {
            return false;
        };
        let top = magnitudes[lo..=hi].iter().copied().fold(0.0f64, f64::max);
        // A sine of amplitude `a` reads `a * magnitude_gain / 2`.
        let amplitude = 2.0 * top / self.geometry.magnitude_gain::<f64>();
        if amplitude.is_nan() || amplitude < SILENCE_FLOOR.to_amplitude().get() {
            return false;
        }
        let floor = top * self.peak_floor.get().to_amplitude().get();
        // A floor that compares false with a peak (a NaN) keeps none.
        for i in local_maxima(magnitudes, lo..hi + 1).filter(|&i| magnitudes[i] >= floor) {
            let m = magnitudes[i];
            let (offset, peak) = interpolate(magnitudes[i - 1], m, magnitudes[i + 1]);
            let frequency = self.geometry.frequency_of(i as f64 + offset);
            if frequency >= self.min_freq && frequency <= self.max_freq {
                visit(frequency.get(), peak * peak);
            }
        }
        true
    }

    /// Adds a peak's distance from the equal-tempered grid to `sums`.
    fn tune(frequency: f64, energy: f64, sums: &mut TuningSums) {
        let off = off_grid(frequency);
        let turn = off * std::f64::consts::TAU;
        let w = energy;
        sums.cos += w * libm::cos(turn);
        sums.sin += w * libm::sin(turn);
    }

    /// Folds a peak into the twelve classes at `tuning` semitones sharp.
    fn fold(&self, frequency: f64, energy: f64, tuning: Semitones, frame: &mut [f64; CLASSES]) {
        let mut weight = energy;
        for order in 1..=self.harmonics.get() {
            let sub = frequency / order as f64;
            if sub < self.min_freq.get() {
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
fn off_grid(frequency: f64) -> f64 {
    let s = semitones_from_a4(frequency).get();
    s - s.round()
}

/// `frequency`'s distance from A4.
fn semitones_from_a4(frequency: f64) -> Semitones {
    Semitones::from_pitch_ratio(frequency / Note::A4.frequency().get())
}

/// The class of `frequency` once `tuning` is taken out, as a column.
fn class_of(frequency: f64, tuning: Semitones) -> usize {
    let a = f64::from(Note::A4.class().semitones_above_c());
    let position = a + semitones_from_a4(frequency).get() - tuning.get();
    (position.round() as i64).rem_euclid(CLASSES as i64) as usize
}

/// Parabolic interpolation of the log magnitude through three bins, whose
/// middle one is the peak: its offset from the middle bin, within
/// `-0.5..=0.5`, and the magnitude there.
fn interpolate(below: f64, mid: f64, above: f64) -> (f64, f64) {
    let ln = |m: f64| libm::log(m.max(TINY));
    let peak = parabolic_peak(ln(below), ln(mid), ln(above));
    (peak.offset, libm::exp(peak.value))
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
        TuningOffset::new(Semitones::new(turns).to_cents()).unwrap_or(TuningOffset::NONE)
    }

    fn cents(self) -> Cents {
        self.offset().get()
    }
}

/// A chromagram: a `frames x 12` grid and the tuning it was folded with.
#[derive(Debug, Clone, PartialEq)]
pub struct Chroma {
    grid: Grid<f64>,
    tuning: Cents,
    framing: Framing,
}

impl Chroma {
    /// A chroma of `grid` taken at `framing`, with `tuning`.
    #[cfg(test)]
    pub(crate) fn new(grid: Grid<f64>, tuning: Cents, framing: Framing) -> Self {
        Self {
            grid,
            tuning,
            framing,
        }
    }

    /// One row per frame, one column per pitch class from C, each `0..=1`.
    #[inline]
    #[must_use]
    pub fn grid(&self) -> &Grid<f64> {
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
    pub fn mean(&self) -> [f64; CLASSES] {
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
pub(crate) fn mean_of_rows(grid: &Grid<f64>) -> [f64; CLASSES] {
    let mut sum = [0.0f64; CLASSES];
    for row in grid.rows() {
        for (s, &v) in sum.iter_mut().zip(row) {
            *s += v;
        }
    }
    mean_of(&sum, grid.frames())
}

fn mean_of(sum: &[f64; CLASSES], frames: FrameCount) -> [f64; CLASSES] {
    let n = frames.get().max(1) as f64;
    sum.map(|s| s / n)
}

/// What a run of frames carries: a running mean and the tuning sums, and the
/// buffers a frame is transformed in.
#[derive(Debug, Clone)]
struct ChromaFold {
    plan: ChromaPlan,
    window: Vec<f64>,
    spectrum: Vec<Complex64>,
    magnitudes: Vec<f64>,
    sum: [f64; CLASSES],
    frames: FrameCount,
    sums: TuningSums,
}

impl ChromaFold {
    /// A state with every buffer built for `plan`.
    fn new(plan: ChromaPlan) -> Self {
        let window = plan.geometry.window().get();
        Self {
            plan,
            window: plan.geometry.window_coefficients(),
            spectrum: vec![Complex64::default(); window / 2 + 1],
            magnitudes: vec![0.0; window / 2 + 1],
            sum: [0.0; CLASSES],
            frames: FrameCount(0),
            sums: TuningSums::default(),
        }
    }

    /// Forgets the carried frames and tuning, keeping the buffers.
    fn reset(&mut self) {
        self.sum = [0.0; CLASSES];
        self.frames = FrameCount(0);
        self.sums = TuningSums::default();
    }

    /// The mean of the frames folded so far, per class.
    fn mean(&self) -> [f64; CLASSES] {
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
    fn transform(&mut self, samples: &[f64], fft: &mut FftScratch<f64>) {
        fft.forward(Mono::new(samples), &self.window, &mut self.spectrum);
        for (m, bin) in self.magnitudes.iter_mut().zip(&self.spectrum) {
            *m = bin.norm();
        }
    }

    /// Folds one frame of `samples` (exactly one finite window) and adds it to
    /// the running mean. Returns the frame's chroma, `0..=1` per class, zero
    /// for a frame with no pitch.
    fn fold_frame(&mut self, samples: &[f64], fft: &mut FftScratch<f64>) -> [f64; CLASSES] {
        self.transform(samples, fft);
        let plan = self.plan;

        let mut frame = [0.0f64; CLASSES];
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

        let max = frame.iter().copied().fold(0.0f64, f64::max);
        if max > 0.0 {
            frame.iter_mut().for_each(|v| *v /= max);
        } else {
            frame = [0.0; CLASSES];
        }
        for (s, &v) in self.sum.iter_mut().zip(&frame) {
            *s += v;
        }
        self.frames = FrameCount(self.frames.get() + 1);
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
    pub bins: [f64; CLASSES],
}

/// The reading of a [`ChromaMeter`]: the mean of the frames folded and the
/// tuning in use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromaSummary {
    /// The mean of the frames, per pitch class from C; all zero before the
    /// first frame.
    pub mean: [f64; CLASSES],
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
pub struct ChromaMeter<S: Sample> {
    fold: ChromaFold,
    cutter: FrameCutter,
    fft: FftScratch<f64>,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> ChromaMeter<S> {
    fn new(plan: ChromaPlan) -> Self {
        let mut fft = FftScratch::new();
        let fold = ChromaFold::new(plan);
        // Plans the transform and sizes its scratch, so a push allocates
        // nothing.
        let window = plan.geometry.window().get();
        let mut spectrum = vec![Complex64::default(); window / 2 + 1];
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
    fn feed(&mut self, block: &[f64], mut each: impl FnMut(ChromaFrame)) {
        let Self {
            fold, cutter, fft, ..
        } = self;
        let hop = fold.plan.geometry.hop().get();
        cutter.feed(block, |index, samples| {
            each(ChromaFrame {
                frame: Samples::new(index * hop),
                bins: fold.fold_frame(samples, fft),
            });
        });
    }

    /// The mean of the frames folded so far, per class; all zero before the
    /// first.
    #[must_use]
    pub fn mean(&self) -> [f64; CLASSES] {
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
) -> Result<Chroma> {
    let plan = cfg.plan(rate.into())?;
    let samples = widen(samples.samples());
    if !samples.iter().all(|v| v.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    let window = plan.geometry.window().get();
    if samples.len() < window {
        return Err(Error::InsufficientInput {
            needed: Samples::new(window),
            got: Samples::new(samples.len()),
        });
    }
    let frames = plan.geometry.frames_for(Samples::new(samples.len()));
    check_frame_count(frames, MAX_FRAMES)?;
    let frame_at = |i: usize| {
        let at = i * plan.geometry.hop().get();
        &samples[at..at + window]
    };

    let tuning = match plan.tuning {
        Tuning::Fixed(offset) => offset,
        Tuning::Estimate => {
            let mut state = ChromaFold::new(plan);
            // The meter built below owns a separate scratch.
            let mut fft = FftScratch::new();
            for i in 0..frames.get() {
                state.transform(frame_at(i), &mut fft);
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
    let mut meter = ChromaMeter::<f64>::new(fixed);
    let mut data = Vec::with_capacity(frames.get() * CLASSES);
    meter.feed(&samples, |frame| data.extend_from_slice(&frame.bins));
    Ok(Chroma {
        grid: Grid::try_from((data, frames, BinCount(CLASSES)))?,
        tuning: tuning.get(),
        framing: cfg.framing,
    })
}

#[cfg(test)]
mod tests;
