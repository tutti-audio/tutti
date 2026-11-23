//! Tempo estimation and beat tracking.
//!
//! The tempo is the strongest peak of the novelty curve's autocorrelation,
//! weighted by a log-Gaussian prior over tempo (Ellis 2007; the same shape as
//! librosa's `beat_track`), refined to a fraction of a frame by a parabola
//! through the peak. The beats come from [`tutti_dsp::track_beat_frames`]'s
//! dynamic program, run at that tempo.
//!
//! # Time resolution
//!
//! Everything works on the novelty curve, one value per hop. The default
//! [`TempoConfig::framing`] is a 1024-sample window with a 512-sample hop:
//! 11.6 ms at 44.1 kHz. Beats sit on the hop grid, shifted by where flux peaks in a frame, so each is
//! within one hop of the onset it marks. A tempo is a period of whole frames
//! refined by a parabola, so its error grows with tempo: measured, 0.3 BPM at
//! 90 BPM and 1.1 at 174 at this hop. A smaller hop buys resolution at the cost
//! of more frames.
//!
//! # Octaves
//!
//! A pulse at `T` also repeats at `2T`, so the autocorrelation peaks at every
//! multiple of the beat period. The prior picks among them: it centres on
//! 120 BPM one octave wide, so a train that only repeats at 60 BPM is reported
//! as 60, while one with a half-strength pulse between its beats is reported as
//! 120 and 60 appears among the [`Tempo::alternatives`]. Move the centre or
//! narrow the range to settle it for a known genre.
//!
//! These are offline or UI-thread calls: each allocates the novelty curve, its
//! autocorrelation and the tracker's tables.

use crate::level::SILENCE_FLOOR;
use crate::{EmptyTempoRange, FramingError, Measure, MeterError, StreamMeter, TempoEstimateError};
use tutti_buffers::{Float, Mono, Sample};
use tutti_units::{Bpm, Confidence, SampleRate, Samples, Seconds};

use crate::analyses::xcorr::cross_correlate;
use core::marker::PhantomData;

use crate::convert::{Gathered, widen};
use crate::framing::Framing;
use crate::measure::measure_whole;
use crate::meters::onset::{DetectionFunction, OnsetConfig, onset_lag, run};

use tutti_dsp::FrameCount;
use tutti_dsp::StftGeometry;
use tutti_dsp::{local_maxima, parabolic_peak, track_beat_frames};
use tutti_units::ranged;

/// The default analysis window, in samples.
const WINDOW: usize = 1024;
/// The default hop, in samples.
const HOP: usize = 512;

/// The slowest and fastest tempo [`TempoRange`] accepts. Outside
/// them a period is not a musical beat, and the lag it maps to is absurd.
const MIN_BPM: f64 = 1.0;
const MAX_BPM: f64 = 1000.0;

/// The most samples of audio one call gathers: 2 GiB as `f64`, about 93 minutes
/// at 48 kHz, which bounds the novelty curve too.
const MAX_SAMPLES: FrameCount = FrameCount(1 << 28);

/// The most runner-up tempi kept.
const MAX_ALTERNATIVES: usize = 16;

/// A curve with less relative spread than this is flat: `hi - lo` is compared
/// with `FLAT * max(|hi|, |lo|)`, and one unit of `f64` epsilon is one ulp of
/// the level, so a curve that is constant up to the last-bit rounding of the
/// transform and the detection function reads flat.
const FLAT: f64 = f64::EPSILON;

ranged! {
    /// How many runner-up tempi a tempo estimate holds: within `0..=16`.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "usize", into = "usize")
    )]
    Alternatives(usize), AlternativesError, "an alternatives count within 0..=16",
    |raw| raw <= MAX_ALTERNATIVES
}

ranged! {
    /// How wide the tempo prior is, in octaves (a factor of two in tempo per
    /// octave): finite and above zero. A ratio between two tempi, which no unit
    /// type covers.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "f64", into = "f64")
    )]
    PriorSpread(f64), PriorSpreadError, "a prior spread above zero",
    |raw| raw > 0.0 && raw.is_finite()
}

ranged! {
    /// How hard the beat tracker holds to the tempo: finite and not negative.
    /// The weight of a penalty against a novelty score of unit standard
    /// deviation, which no unit type covers.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "f64", into = "f64")
    )]
    Tightness(f64), TightnessError, "a tightness of zero or more",
    |raw| raw >= 0.0 && raw.is_finite()
}

/// The tempi searched: `min` below `max`, each within `1..=1000` BPM.
///
/// `TryFrom` over `min..=max` names the reason a range is refused;
/// [`new`](Self::new) is the same check as an `Option`. Whether a whole-frame
/// lag falls in the range at the hop in use, and whether the curve is long
/// enough for it, is checked when the tempo is estimated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoRange {
    min: Bpm,
    max: Bpm,
}

impl TempoRange {
    /// The range `min..=max`, or `None` where [`TryFrom`] would refuse it.
    #[must_use]
    pub fn new(min: Bpm, max: Bpm) -> Option<Self> {
        Self::try_from(min..=max).ok()
    }
}

impl TryFrom<core::ops::RangeInclusive<Bpm>> for TempoRange {
    type Error = EmptyTempoRange;

    /// # Errors
    ///
    /// [`TempoEstimateError::EmptyTempoRange`] unless `1 <= min < max <= 1000`.
    fn try_from(range: core::ops::RangeInclusive<Bpm>) -> Result<Self, EmptyTempoRange> {
        let (min, max) = range.into_inner();
        let ok = |b: Bpm| (MIN_BPM..=MAX_BPM).contains(&b.get());
        if !(ok(min) && ok(max) && min < max) {
            return Err(EmptyTempoRange { min, max });
        }
        Ok(Self { min, max })
    }
}

/// What [`estimate_tempo`] and [`track_beats`] search and prefer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoConfig {
    min: Bpm,
    max: Bpm,
    centre: Bpm,
    spread: PriorSpread,
    tightness: Tightness,
    alternatives: usize,
    framing: Framing,
}

impl Default for TempoConfig {
    /// 60 to 200 BPM, a prior centred on 120 BPM one octave wide, a tightness
    /// of 100 and two alternatives.
    fn default() -> Self {
        Self {
            min: Bpm::of(60.0),
            max: Bpm::of(200.0),
            centre: Bpm::of(120.0),
            spread: PriorSpread::of(1.0),
            tightness: Tightness::of(100.0),
            alternatives: 2,
            framing: Framing::new(Samples::new(WINDOW), Samples::new(HOP)),
        }
    }
}

impl TempoConfig {
    /// The defaults; see [`Default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The window and hop the audio entry points analyse with, 1024 and 512
    /// samples by default (11.6 ms at 44.1 kHz). A zero window or hop is
    /// refused by the call that takes the audio.
    #[must_use]
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    /// Search tempi in `range`. The estimate never leaves the range. A range
    /// that holds no whole-frame lag at the hop in use, or needs more curve
    /// than the input has, is refused when the tempo is estimated.
    #[must_use]
    pub fn range(mut self, range: TempoRange) -> Self {
        self.min = range.min;
        self.max = range.max;
        self
    }

    /// Prefer tempi near `centre`: the prior's weight is highest there.
    #[must_use]
    pub fn prior_centre(mut self, centre: Bpm) -> Self {
        self.centre = centre;
        self
    }

    /// How wide the tempo prior is, in octaves: the weight falls to `exp(-1/2)`
    /// one `spread` from the centre.
    #[must_use]
    pub fn prior_spread(mut self, spread: PriorSpread) -> Self {
        self.spread = spread;
        self
    }

    /// How hard the beat tracker holds to the tempo: the weight of the penalty
    /// on a gap's log ratio to the beat period, against a novelty score of unit
    /// standard deviation. Zero ignores the period and picks the strongest
    /// onsets; large values give an evenly spaced grid.
    #[must_use]
    pub fn tightness(mut self, tightness: Tightness) -> Self {
        self.tightness = tightness;
        self
    }

    /// How many runner-up tempi [`Tempo::alternatives`] holds, at most 16.
    #[must_use]
    pub fn alternatives(mut self, count: Alternatives) -> Self {
        self.alternatives = count.get();
        self
    }
}

impl TempoConfig {
    /// A gather for audio of this framing, once the framing is checked
    /// against `rate`, as the analyses do before reading audio.
    fn gather(&self, rate: SampleRate) -> Result<Gathered, MeterError> {
        OnsetConfig::new(DetectionFunction::SpectralFlux)
            .framing(self.framing)
            .plan(rate)?;
        Ok(Gathered::new(
            Framing::new(Samples::new(1), Samples::new(1)),
            MAX_SAMPLES,
        ))
    }
}

impl<S: Sample> Measure<S> for TempoConfig {
    type Context = SampleRate;
    type Meter = TempoMeter<S>;

    /// A meter that estimates the tempo of the audio at `rate`, once it has
    /// all of it.
    ///
    /// # Errors
    ///
    /// [`MeterError::Framing`] ([`FramingError::ZeroWindow`] or
    /// [`FramingError::ZeroHop`]) for a zero window or hop.
    fn measure(&self, rate: SampleRate) -> Result<TempoMeter<S>, MeterError> {
        Ok(TempoMeter {
            cfg: *self,
            rate,
            audio: self.gather(rate)?,
            sample: PhantomData,
        })
    }
}

/// A running tempo estimate at one rate: gathers the audio and estimates the
/// tempo at [`finish`](StreamMeter::finish), as [`estimate_tempo`] does.
///
/// Not for the audio thread: it holds the whole stream, so it grows with the
/// audio and allocates.
#[derive(Debug)]
pub struct TempoMeter<S: Sample> {
    cfg: TempoConfig,
    rate: SampleRate,
    audio: Gathered,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> StreamMeter for TempoMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Result<Tempo, TempoEstimateError>;

    /// Gathers the next chunk of the audio; no tempo is read before the end.
    fn push(&mut self, chunk: Mono<'_, S>, _: &mut impl Extend<()>) -> Result<(), MeterError> {
        self.audio.push(chunk)
    }

    /// The tempo of the audio gathered, or what [`estimate_tempo`] refuses it for.
    fn finish(self) -> Result<Tempo, TempoEstimateError> {
        estimate_tempo_at(&self.cfg, self.audio.samples(), self.rate)
    }
}

/// What [`track_beats`] reads: a [`TempoConfig`], whose beat grid a meter
/// places where [`TempoConfig`]'s own meter reads the tempo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeatConfig(TempoConfig);

impl BeatConfig {
    /// Tracks beats at the tempo `tempo` estimates.
    #[must_use]
    pub fn new(tempo: TempoConfig) -> Self {
        Self(tempo)
    }
}

impl<S: Sample> Measure<S> for BeatConfig {
    type Context = SampleRate;
    type Meter = BeatMeter<S>;

    /// A meter that places the beats of the audio at `rate`, once it has all
    /// of it.
    ///
    /// # Errors
    ///
    /// [`MeterError::Framing`] ([`FramingError::ZeroWindow`] or
    /// [`FramingError::ZeroHop`]) for a zero window or hop.
    fn measure(&self, rate: SampleRate) -> Result<BeatMeter<S>, MeterError> {
        Ok(BeatMeter {
            cfg: self.0,
            rate,
            audio: self.0.gather(rate)?,
            sample: PhantomData,
        })
    }
}

/// A running beat grid at one rate: gathers the audio and places the beats at
/// [`finish`](StreamMeter::finish), as [`track_beats`] does.
///
/// Not for the audio thread: it holds the whole stream, so it grows with the
/// audio and allocates.
#[derive(Debug)]
pub struct BeatMeter<S: Sample> {
    cfg: TempoConfig,
    rate: SampleRate,
    audio: Gathered,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> StreamMeter for BeatMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Result<BeatGrid, TempoEstimateError>;

    /// Gathers the next chunk of the audio; no beat is placed before the end.
    fn push(&mut self, chunk: Mono<'_, S>, _: &mut impl Extend<()>) -> Result<(), MeterError> {
        self.audio.push(chunk)
    }

    /// The beats of the audio gathered, or what [`track_beats`] refuses it for.
    fn finish(self) -> Result<BeatGrid, TempoEstimateError> {
        track_beats_at(&self.cfg, self.audio.samples(), self.rate)
    }
}

/// A tempo other than the winner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoCandidate {
    /// The tempo, in [`Bpm`].
    pub bpm: Bpm,
    /// How periodic the novelty is at this tempo, as for [`Tempo::confidence`].
    pub strength: Confidence,
}

/// A tempo estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct Tempo {
    /// The estimated tempo, in [`Bpm`], inside the configured range.
    pub bpm: Bpm,
    /// How periodic the novelty is at that tempo: the autocorrelation
    /// coefficient of the mean-removed curve at the tempo's lag, in `0..=1`
    /// (a negative one reads zero). One is a curve that repeats exactly over
    /// the whole clip, and noise reads near zero. It is not a probability, and
    /// it does not measure octave ambiguity: a train that repeats at 60 and
    /// 120 BPM reads high for both, so check
    /// [`alternatives`](Self::alternatives) when the octave matters. It is
    /// normalised by the whole curve, not the overlap at the lag, so a clip
    /// short against the period reads lower, not higher.
    pub confidence: Confidence,
    /// The next strongest tempi after the prior's weighting, best first. Each
    /// is a separate peak of the autocorrelation, so a half or double of
    /// [`bpm`](Self::bpm) shows up here when the signal supports it.
    pub alternatives: Vec<TempoCandidate>,
}

/// Beat positions and the tempo they were placed at.
#[derive(Debug, Clone, PartialEq)]
pub struct BeatGrid {
    /// The beats, ascending, in [`Samples`] from the start of the analysed
    /// buffer, on the hop grid.
    pub beats: Vec<Samples>,
    /// The tempo the beats were placed at, in [`Bpm`]: the estimate, or the
    /// one the caller supplied.
    pub bpm: Bpm,
}

impl BeatGrid {
    /// Beat `index` in [`Seconds`], resolved against `sample_rate`, or `None`
    /// past the last beat.
    #[must_use]
    pub fn time(&self, index: usize, sample_rate: impl Into<SampleRate>) -> Option<Seconds> {
        self.beats
            .get(index)
            .map(|b| Seconds::new(b.get() as f64 / sample_rate.into().get()))
    }
}

/// Estimates the tempo of `samples`.
///
/// Takes the spectral novelty of `samples` at `rate` on the config's framing
/// and runs [`estimate_tempo_from_novelty`] on it.
/// The first frame has no predecessor to differ from, so its novelty is zero.
///
/// # Errors
///
/// [`TempoEstimateError::NonFiniteInput`] for a NaN or infinite sample; [`TempoEstimateError::NoSignal`]
/// for input whose peak is under -90 dBFS or whose novelty is flat; [`TempoEstimateError::InsufficientInput`]
/// when the input is shorter than one window, or holds fewer than two periods
/// of the slowest tempo; [`TempoEstimateError::EmptyTempoRange`] when no whole-frame lag
/// falls in the range at this hop.
pub fn estimate_tempo<S: Sample>(
    cfg: &TempoConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Tempo, TempoEstimateError> {
    measure_whole::<S, _, _, _>(cfg, rate.into(), samples)
}

/// [`estimate_tempo`] over `f64` samples.
fn estimate_tempo_at(
    cfg: &TempoConfig,
    samples: &[f64],
    rate: SampleRate,
) -> Result<Tempo, TempoEstimateError> {
    let novelty = novelty(cfg, samples, rate)?.0;
    estimate_tempo_from_novelty(cfg, &novelty, rate, cfg.framing.hop())
}

/// Estimates the tempo from a novelty curve of one value per `hop` samples.
///
/// For a caller's own detection function. The curve is read as strength per
/// frame, and only its shape matters: any positive gain gives the same
/// result. Its level is not judged: only an exactly flat curve (or one flat to
/// rounding) is [`TempoEstimateError::NoSignal`], so a curve from near-silent audio is the
/// caller's to gate.
///
/// # Errors
///
/// As [`estimate_tempo`] (except that a silence floor applies to samples
/// only), with [`TempoEstimateError::NonFiniteInput`] for a NaN or infinite
/// value in `novelty` and [`FramingError::ZeroHop`] for a zero `hop`.
///
/// `novelty` is a curve of readings, not audio, and `hop` is the spacing of
/// its values in samples.
pub fn estimate_tempo_from_novelty<F: Float>(
    cfg: &TempoConfig,
    novelty: &[F],
    sample_rate: impl Into<SampleRate>,
    hop: Samples,
) -> Result<Tempo, TempoEstimateError> {
    let rate = frame_rate(sample_rate.into(), hop)?;
    let curve = Periodicity::new(cfg, &widen(novelty), rate, hop)?;
    Ok(curve.tempo(cfg))
}

/// Places the beats of `samples` at its estimated tempo.
///
/// Takes the novelty as [`estimate_tempo`] does, and places beats with
/// [`track_beats_from_novelty`]; each beat is `frame * hop` plus the lag that spectral
/// flux peaks behind its onset, `3 * window / 4 - hop / 2`.
///
/// # Errors
///
/// As [`estimate_tempo`].
pub fn track_beats<S: Sample>(
    cfg: &TempoConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<BeatGrid, TempoEstimateError> {
    measure_whole::<S, _, _, _>(&BeatConfig::new(*cfg), rate.into(), samples)
}

/// [`track_beats`] over `f64` samples.
fn track_beats_at(
    cfg: &TempoConfig,
    samples: &[f64],
    rate: SampleRate,
) -> Result<BeatGrid, TempoEstimateError> {
    let (novelty, geometry) = novelty(cfg, samples, rate)?;
    let mut grid = track_beats_from_novelty(cfg, &novelty, rate, geometry.hop(), None)?;
    let shift = onset_lag(geometry);
    for beat in &mut grid.beats {
        *beat += shift;
    }
    Ok(grid)
}

/// Places beats on a novelty curve of one value per `hop` samples.
///
/// With `tempo` of `None` the tempo is [`estimate_tempo_from_novelty`]'s;
/// with `Some` it is that tempo, whatever the range, and the curve need only
/// be long enough to track. Beat `i` is at `frame * hop` samples, the position
/// of its novelty value.
///
/// # Errors
///
/// As [`estimate_tempo_from_novelty`] when estimating. With a supplied tempo,
/// [`TempoEstimateError::NonFiniteInput`] for a NaN or infinite value in `novelty`, and
/// [`TempoEstimateError::UntrackableTempo`] for a tempo whose period is not between two
/// frames and the curve's length; a flat curve gives no beats.
pub fn track_beats_from_novelty<F: Float>(
    cfg: &TempoConfig,
    novelty: &[F],
    sample_rate: impl Into<SampleRate>,
    hop: Samples,
    tempo: Option<Bpm>,
) -> Result<BeatGrid, TempoEstimateError> {
    let sample_rate = sample_rate.into();
    let novelty = &widen(novelty);
    let rate = frame_rate(sample_rate, hop)?;
    let bpm = match tempo {
        Some(bpm) => {
            if novelty.iter().any(|v| !v.is_finite()) {
                return Err(TempoEstimateError::NonFiniteInput);
            }
            // A period under two frames cannot be tracked, and one longer than
            // the curve holds at most one beat.
            let period = lag_of(bpm, rate);
            if !(2.0..=novelty.len() as f64).contains(&period) {
                return Err(TempoEstimateError::UntrackableTempo { bpm });
            }
            bpm
        }
        None => estimate_tempo_from_novelty(cfg, novelty, sample_rate, hop)?.bpm,
    };
    let period = lag_of(bpm, rate);
    let beats = track_beat_frames(novelty, period, cfg.tightness.get())
        .into_iter()
        .map(|frame| Samples::new(frame * hop.get()))
        .collect();
    Ok(BeatGrid { beats, bpm })
}

/// Frames per second of a curve with one value per `hop`: the curve's own
/// sampling rate, in the unit that holds an `f64` rate.
fn frame_rate(sample_rate: SampleRate, hop: Samples) -> Result<SampleRate, TempoEstimateError> {
    if hop.is_zero() {
        return Err(FramingError::ZeroHop.into());
    }
    // Positive and finite for a nonzero hop, short of an `f64` underflow, which
    // a hop that large reports as a zero one.
    SampleRate::new(sample_rate.get() / hop.get() as f64).ok_or(FramingError::ZeroHop.into())
}

/// The lag in frames of one beat at `bpm` on a curve at `rate`: `60 * rate / bpm`,
/// in `f64` because a lag is a fractional frame count.
fn lag_of(bpm: Bpm, rate: SampleRate) -> f64 {
    60.0 * rate.get() / bpm.get()
}

/// The tempo of a lag of `lag` frames on a curve at `rate`: `60 * rate / lag`.
fn bpm_of(lag: f64, rate: SampleRate) -> f64 {
    60.0 * rate.get() / lag
}

/// The detection curve of `samples` at `rate` on the config's framing, with
/// the first frame zeroed, and the geometry it was taken on.
fn novelty(
    cfg: &TempoConfig,
    samples: &[f64],
    rate: SampleRate,
) -> Result<(Vec<f64>, StftGeometry), TempoEstimateError> {
    let onset = OnsetConfig::new(DetectionFunction::SpectralFlux)
        .framing(cfg.framing)
        .plan(rate)?;
    let geometry = onset.geometry();
    let window = geometry.window().get();
    if samples.len() < window {
        return Err(TempoEstimateError::InsufficientInput {
            needed: Samples::new(window),
            got: Samples::new(samples.len()),
        });
    }
    let peak = samples.iter().fold(0.0f64, |m, s| m.max(s.abs()));
    if peak < SILENCE_FLOOR.to_amplitude().get() {
        return Err(TempoEstimateError::NoSignal);
    }
    let mut curve: Vec<f64> = run(onset, samples)
        .novelty()
        .iter()
        .map(|n| n.value)
        .collect();
    if let Some(first) = curve.first_mut() {
        *first = 0.0;
    }
    Ok((curve, geometry))
}

/// The normalised autocorrelation of a novelty curve over the searched lags.
struct Periodicity {
    /// Normalised autocorrelation at lags `first_lag..=last_lag`, including
    /// the one lag either side that the refinement reads.
    values: Vec<f64>,
    /// The first lag inside the tempo range, in novelty frames.
    first_lag: FrameCount,
    /// The last lag inside the tempo range, in novelty frames.
    last_lag: FrameCount,
    frame_rate: SampleRate,
}

impl Periodicity {
    fn new(
        cfg: &TempoConfig,
        novelty: &[f64],
        frame_rate: SampleRate,
        hop: Samples,
    ) -> Result<Self, TempoEstimateError> {
        if novelty.iter().any(|v| !v.is_finite()) {
            return Err(TempoEstimateError::NonFiniteInput);
        }
        // Lag 1 has no neighbour below it for the refinement, so start at 2.
        let first_lag = (lag_of(cfg.max, frame_rate).ceil() as usize).max(2);
        // In floating point first: a slow tempo at a fast frame rate maps to a
        // lag that does not fit a `usize` after the arithmetic below.
        let longest = lag_of(cfg.min, frame_rate).floor();
        if longest < first_lag as f64 {
            return Err(EmptyTempoRange {
                min: cfg.min,
                max: cfg.max,
            }
            .into());
        }
        let n = novelty.len();
        // Two periods of the slowest tempo, so the longest lag still overlaps
        // half the curve.
        let needed = 2.0 * (longest + 2.0);
        if (n as f64) < needed {
            return Err(TempoEstimateError::InsufficientInput {
                needed: Samples::new((needed * hop.get() as f64).min(usize::MAX as f64) as usize),
                got: Samples::new(n * hop.get()),
            });
        }
        let last_lag = FrameCount(longest as usize);
        let first_lag = FrameCount(first_lag);

        let (lo, hi) = novelty
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        if hi - lo <= FLAT * hi.abs().max(lo.abs()) {
            return Err(TempoEstimateError::NoSignal);
        }
        let mean = novelty.iter().copied().sum::<f64>() / n as f64;
        let centred: Vec<f64> = novelty.iter().map(|&v| v - mean).collect();
        let correlation = cross_correlate(Mono::new(&centred), Mono::new(&centred));
        let power = correlation.at(0).unwrap_or(0.0) / n as f64;
        if power <= 0.0 {
            return Err(TempoEstimateError::NoSignal);
        }
        // Over the whole curve's power, not the overlap at the lag: a coefficient
        // in -1..=1 (Cauchy-Schwarz), where a long lag has fewer terms and
        // reads lower.
        let values = (first_lag.get() - 1..=last_lag.get() + 1)
            .map(|lag| correlation.at(lag as isize).unwrap_or(0.0) / n as f64 / power)
            .collect();
        Ok(Self {
            values,
            first_lag,
            last_lag,
            frame_rate,
        })
    }

    /// The tempo of lag `lag`, in BPM.
    fn bpm(&self, lag: f64) -> f64 {
        bpm_of(lag, self.frame_rate)
    }

    /// The normalised autocorrelation at a lag in `first_lag - 1..=last_lag + 1`
    /// (the searched lags and the one either side that refinement reads);
    /// `values[0]` is lag `first_lag - 1`.
    fn at(&self, lag: FrameCount) -> f64 {
        let lag = lag.get();
        debug_assert!((self.first_lag.get() - 1..=self.last_lag.get() + 1).contains(&lag));
        self.values[lag - (self.first_lag.get() - 1)]
    }

    /// The prior's weight at `lag`, from 1 at the centre down.
    fn weight(&self, cfg: &TempoConfig, lag: FrameCount) -> f64 {
        let octaves = libm::log2(self.bpm(lag.get() as f64) / cfg.centre.get()) / cfg.spread.get();
        libm::exp(-0.5 * octaves * octaves)
    }

    /// The lag refined to a fraction of a frame by a parabola through its
    /// value and its neighbours', or `lag` when they do not bend down.
    fn refined(&self, lag: FrameCount) -> f64 {
        let (below, above) = (FrameCount(lag.get() - 1), FrameCount(lag.get() + 1));
        let peak = parabolic_peak(self.at(below), self.at(lag), self.at(above));
        lag.get() as f64 + peak.offset
    }

    fn tempo(&self, cfg: &TempoConfig) -> Tempo {
        // Local maxima of the weighted score; an edge of the range counts when
        // the score rises towards it.
        let first = self.first_lag.get();
        let scores: Vec<f64> = (first..=self.last_lag.get())
            .map(|lag| self.at(FrameCount(lag)).max(0.0) * self.weight(cfg, FrameCount(lag)))
            .collect();
        let mut peaks: Vec<usize> = local_maxima(&scores, 0..scores.len()).collect();
        // Best first; on a tie, the shorter lag (higher tempo).
        peaks.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
        let candidate = |at: usize| {
            let lag = FrameCount(first + at);
            TempoCandidate {
                bpm: Bpm::new(self.bpm(self.refined(lag)))
                    .unwrap_or(cfg.min)
                    .clamp(cfg.min, cfg.max),
                strength: Confidence::new_clamped(self.at(lag)),
            }
        };
        let best = candidate(peaks[0]);
        Tempo {
            bpm: best.bpm,
            confidence: best.strength,
            alternatives: peaks[1..]
                .iter()
                .take(cfg.alternatives)
                .map(|&at| candidate(at))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
