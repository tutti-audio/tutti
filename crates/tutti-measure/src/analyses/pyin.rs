//! pYIN probabilistic pitch tracking.
//!
//! Mauch and Dixon, 2014, "pYIN: A fundamental frequency estimator using
//! probabilistic threshold distributions". It keeps YIN's difference function
//! (stages 1 to 3 of [`yin`](super::yin), run by the same detector) and
//! replaces the single absolute threshold and the frame-by-frame decision by
//! two stages, both run off the audio thread (they allocate):
//!
//! 1. **Candidates.** The threshold on the cumulative mean normalized
//!    difference d'(τ) is a random variable with a Beta prior of shape
//!    α = 2 and β = α(1 − m) / m, whose mean is `m`
//!    ([`PyinConfig::threshold_mean`]; the paper uses 0.1, 0.15 and 0.2). The
//!    prior is taken at 100 thresholds, 0.01 to 1.00, each weighted by its
//!    density and the weights normalised to sum to 1. A threshold selects what
//!    YIN selects under it: the first lag where d' falls below it, walked down
//!    to the local minimum and refined by the parabola through its neighbours.
//!    A frame's candidates are the pitches so selected, each with the summed
//!    weight of the thresholds that selected it; the weight of the thresholds
//!    that selected nothing (or a pitch off the grid) is the frame's
//!    probability of being unvoiced.
//! 2. **Decoding.** A hidden Markov model over pitch states on a grid of
//!    10 cents, 481 bins for the default four octaves (the paper's 480 and the
//!    top bin), in a voiced and an unvoiced copy, so 962 states. A candidate's
//!    weight is the emission of its nearest voiced bin; the unvoiced mass
//!    `1 − Σ weights` is spread evenly over the unvoiced bins. A move changes
//!    the pitch by at most [`PyinConfig::transition_width`] with a triangular
//!    weight, and the voiced/unvoiced copy with probability
//!    [`PyinConfig::switch`] (a row of the transition matrix sums to 1). The
//!    best path is found by [`viterbi`] over a
//!    [`BandedTransition`].
//!
//! A voiced frame reports the centre of its decoded bin, so it is within 5
//! cents of the candidate that put the path there. Continuity is what the
//! decoding adds: a frame whose candidates split between a pitch and its
//! octave follows the neighbouring frames, because an octave jump is farther
//! than a frame's transition width and costs the emissions of the bins it
//! crosses.
//!
//! A bin that no candidate reaches has the emission [`EMISSION_FLOOR`], so a
//! path may cross it at a fixed cost and never at probability zero. A
//! candidate outside the grid is not a state: its weight goes to the unvoiced
//! mass.

use core::marker::PhantomData;

use tutti_buffers::{Mono, Sample};
use tutti_dsp::{BandedTransition, FrameCount, viterbi};
use tutti_units::{Cents, Confidence, Hz, SampleRate, Samples, ranged};

use crate::analyses::notes::PitchHop;
use crate::analyses::yin::{PitchDetector, YinConfig, YinPlan};
use crate::convert::Gathered;
use crate::framing::Framing;
use crate::measure::measure_whole;
use crate::meters::chroma::FrequencyBand;
use crate::{Measure, MeterError, PitchError, StreamMeter};

ranged! {
    /// The mean of pYIN's Beta prior on the threshold: within `0.02..=0.5`.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Confidence", into = "Confidence")
    )]
    PyinThresholdMean(Confidence), PyinThresholdMeanError,
    "a mean threshold within 0.02..=0.5",
    |raw| raw.get() >= 0.02 && raw.get() <= 0.5
}

ranged! {
    /// The probability of changing between voiced and unvoiced from one frame
    /// to the next: within `(0, 0.5]`.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Confidence", into = "Confidence")
    )]
    PyinSwitch(Confidence), PyinSwitchError,
    "a voiced/unvoiced switch probability within (0, 0.5]",
    |raw| raw.get() > 0.0 && raw.get() <= 0.5
}

ranged! {
    /// The largest pitch change from one frame to the next: within
    /// `10..=1200` cents, from one grid step to an octave.
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "Cents", into = "Cents")
    )]
    PyinTransitionWidth(Cents), PyinTransitionWidthError,
    "a transition width within 10..=1200 cents",
    |raw| raw.get() >= 10.0 && raw.get() <= 1200.0
}

/// The pitch grid's step: the paper's tenth of a semitone.
const BIN: Cents = Cents::new(10.0);
/// The Beta prior's first shape parameter (the paper's α).
const ALPHA: f64 = 2.0;
/// The thresholds the prior is taken at: `1/100` to `100/100`.
const THRESHOLDS: usize = 100;
/// The emission of a state no candidate reaches, so a path may cross it at a
/// cost (about -27.6 in the log) and never at probability zero.
const EMISSION_FLOOR: f64 = 1e-12;
/// The copies of the pitch grid: voiced then unvoiced.
const LAYERS: u16 = 2;
/// The most frames one call decodes, bounding the back pointers (about 2 KB a
/// frame at the default grid).
const MAX_FRAMES: FrameCount = FrameCount(1 << 16);
/// The most pitch bins: the grid's states (`bins * LAYERS`) are counted in a
/// `u16`.
const MAX_BINS: usize = (u16::MAX / LAYERS) as usize;

/// pYIN parameters: the band the grid spans, the prior's mean and the
/// transition model. Immutable once built, and in hertz and cents, so it holds
/// no sample rate: [`pyin_track`] takes the buffer's rate and checks the band
/// against its Nyquist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PyinConfig {
    band: FrequencyBand,
    threshold_mean: Confidence,
    switch: Confidence,
    transition_width: Cents,
    hop: Option<PitchHop>,
}

impl PyinConfig {
    /// The paper's four octaves from 55 Hz to 880 Hz, a prior mean of 0.1, a
    /// switch probability of 0.01 and a transition width of 250 cents.
    #[must_use]
    pub fn new() -> Self {
        Self {
            band: FrequencyBand::PYIN,
            threshold_mean: Confidence::new(0.1),
            switch: Confidence::new(0.01),
            transition_width: Cents::new(250.0),
            hop: None,
        }
    }

    /// Searches and tracks `band`: the lowest pitch is the first bin of the
    /// grid, and the grid runs in 10 cent steps to the last bin within the
    /// top. A band whose top exceeds half the buffer's sample rate is refused
    /// by [`pyin_track`] ([`AboveNyquist`](crate::AboveNyquist)), and one spanning more
    /// than 32767 bins of 10 cents ([`PitchError::PitchGridTooLarge`]).
    #[must_use]
    pub fn band(mut self, band: FrequencyBand) -> Self {
        self.band = band;
        self
    }

    /// The mean of the Beta prior on the threshold (the paper uses 0.1, 0.15
    /// and 0.2): lower trusts only deep dips, higher accepts shallower ones.
    #[must_use]
    pub fn threshold_mean(mut self, mean: PyinThresholdMean) -> Self {
        self.threshold_mean = mean.get();
        self
    }

    /// The probability of the track switching between voiced and unvoiced
    /// from one frame to the next.
    #[must_use]
    pub fn switch(mut self, switch: PyinSwitch) -> Self {
        self.switch = switch.get();
        self
    }

    /// The largest pitch change between frames, in whole grid steps (the
    /// width rounded down to a multiple of 10 cents).
    #[must_use]
    pub fn transition_width(mut self, width: PyinTransitionWidth) -> Self {
        self.transition_width = width.get();
        self
    }

    /// The hop the track advances by over frames of `frame` samples.
    fn hop_for(&self, frame: Samples) -> Samples {
        self.hop
            .map_or(Samples::new((frame.get() / 8).max(1)), PitchHop::get)
    }

    /// The band's YIN analysis at `rate`.
    fn plan(&self, rate: SampleRate) -> Result<YinPlan, PitchError> {
        YinConfig::new().band(self.band).plan(rate)
    }

    /// How far the track advances per frame: an eighth of the YIN frame by
    /// default. A hop wider than the frame leaves samples no frame reads.
    #[must_use]
    pub fn hop(mut self, hop: PitchHop) -> Self {
        self.hop = Some(hop);
        self
    }

    /// The shortest buffer [`pyin_track`] reads at `rate`.
    ///
    /// # Errors
    ///
    /// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`.
    pub fn buffer_size(&self, rate: impl Into<SampleRate>) -> Result<Samples, PitchError> {
        YinConfig::new().band(self.band).buffer_size(rate.into())
    }

    /// The grid's bins, the lowest pitch and every 10 cents up to the top, and
    /// the transition matrix over them in a voiced and an unvoiced layer.
    fn model(&self) -> Result<(usize, BandedTransition<f64>), PitchError> {
        let span = libm::log2(self.band.max().get() / self.band.min().get()) * 1200.0;
        let bins = (span / BIN.get()) as usize + 1;
        let too_large = PitchError::PitchGridTooLarge {
            bins,
            max: MAX_BINS,
        };
        let half_width = (self.transition_width.get() / BIN.get()) as u16;
        let switch = self.switch.get();
        let transition = u16::try_from(bins)
            .ok()
            .and_then(|pitches| {
                BandedTransition::triangular(
                    pitches,
                    LAYERS,
                    half_width,
                    libm::log(1.0 - switch),
                    libm::log(switch),
                )
            })
            .ok_or(too_large)?;
        Ok((bins, transition))
    }
}

impl Default for PyinConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One frame of a pYIN track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PyinFrame {
    /// The decoded pitch, the centre of its grid bin, or `None` when the
    /// track is unvoiced here.
    pub frequency: Option<Hz>,
    /// The frame's own probability of being voiced under the threshold prior:
    /// the summed weight of the thresholds that found a pitch on the grid.
    /// A reading of the frame alone, independent of the decoded path.
    pub voiced: Confidence,
}

/// The weights of the Beta prior at the thresholds `1/100..=100/100`, summing
/// to 1: the density `s^(α-1) (1-s)^(β-1)` with α = 2 and β = α(1 − mean) /
/// mean.
fn threshold_weights(mean: f64) -> [f64; THRESHOLDS] {
    let beta = ALPHA * (1.0 - mean) / mean;
    let mut weights: [f64; THRESHOLDS] = core::array::from_fn(|i| {
        let s = (i + 1) as f64 / THRESHOLDS as f64;
        libm::pow(s, ALPHA - 1.0) * libm::pow(1.0 - s, beta - 1.0)
    });
    let total: f64 = weights.iter().sum();
    for w in &mut weights {
        *w /= total;
    }
    weights
}

/// Tracks the pitch at the config's hop across a buffer at `rate`.
///
/// Every frame is a YIN buffer of [`PyinConfig::buffer_size`] samples starting
/// at a multiple of the hop, so a frame reads `buffer_size` samples from its
/// start. It allocates the decoding's back pointers and runs off the audio
/// thread; a frame costs about `1000 * (transition width / 10 cents) * 2`
/// moves scored.
///
/// # Errors
///
/// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`;
/// [`PitchError::PitchGridTooLarge`] for a band of too many bins;
/// [`PitchError::InsufficientInput`] if `samples` is shorter than
/// [`PyinConfig::buffer_size`]; [`PitchError::TooManyFrames`] when the track would
/// hold more than 65536 frames.
pub fn pyin_track<S: Sample>(
    cfg: &PyinConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Vec<PyinFrame>, PitchError> {
    measure_whole::<S, _, _, _>(cfg, rate.into(), samples)
}

impl<S: Sample> Measure<S> for PyinConfig {
    type Context = SampleRate;
    type Meter = PyinMeter<S>;

    /// A meter that tracks the pitch of the audio at `rate`, once it has all
    /// of it.
    ///
    /// # Errors
    ///
    /// [`MeterError::Pitch`] for what [`PyinConfig`] refuses at `rate`: a band
    /// whose top exceeds half `rate` ([`PitchError::Nyquist`]) or one of too
    /// many bins ([`PitchError::PitchGridTooLarge`]).
    fn measure(&self, rate: SampleRate) -> Result<PyinMeter<S>, MeterError> {
        let plan = self.plan(rate)?;
        Ok(PyinMeter {
            cfg: *self,
            audio: Gathered::new(
                Framing::new(plan.buffer_size(), self.hop_for(plan.buffer_size())),
                MAX_FRAMES,
            ),
            plan,
            model: self.model()?,
            sample: PhantomData,
        })
    }
}

/// A running pYIN track at one rate: gathers the audio and decodes the track
/// at [`finish`](StreamMeter::finish), as [`pyin_track`] does.
///
/// Not for the audio thread: it holds the whole stream, so it grows with the
/// audio and allocates.
#[derive(Debug)]
pub struct PyinMeter<S: Sample> {
    cfg: PyinConfig,
    plan: YinPlan,
    model: (usize, BandedTransition<f64>),
    audio: Gathered,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> StreamMeter for PyinMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Result<Vec<PyinFrame>, PitchError>;

    /// Gathers the next chunk of the audio; no frame is decoded before the end.
    ///
    /// # Errors
    ///
    /// [`MeterError::NonFiniteInput`] for a NaN or infinity;
    /// [`MeterError::TooManyFrames`] when the audio would make more than 65536
    /// frames.
    fn push(&mut self, chunk: Mono<'_, S>, _: &mut impl Extend<()>) -> Result<(), MeterError> {
        self.audio.push(chunk)
    }

    /// The track of the audio gathered, or what [`pyin_track`] refuses it for.
    fn finish(self) -> Result<Vec<PyinFrame>, PitchError> {
        pyin_at(&self.cfg, self.plan, self.model, self.audio.samples())
    }
}

/// [`pyin_track`] over a resolved band and `f64` samples.
fn pyin_at(
    cfg: &PyinConfig,
    plan: YinPlan,
    (bins, transition): (usize, BandedTransition<f64>),
    samples: &[f64],
) -> Result<Vec<PyinFrame>, PitchError> {
    let frame = plan.buffer_size();
    let hop = cfg.hop_for(frame);
    if samples.len() < frame.get() {
        return Err(PitchError::InsufficientInput {
            needed: frame,
            got: Samples::new(samples.len()),
        });
    }
    let frames = Framing::new(frame, hop).frames_for(Samples::new(samples.len()));

    let weights = threshold_weights(cfg.threshold_mean.get());
    let lowest = cfg.band.min().get();
    let mut detector = plan.detector();
    let mut voiced = Vec::with_capacity(frames.get());
    let mut candidates = vec![0.0f64; bins];
    let path = viterbi(transition, frames.get(), |t, row| {
        let start = t * hop.get();
        candidates.fill(0.0);
        let found = candidate_weights(
            &mut detector,
            &samples[start..start + frame.get()],
            &weights,
            lowest,
            &mut candidates,
        );
        voiced.push(found);
        let unvoiced = (1.0 - found).max(0.0) / bins as f64;
        let (voiced_row, unvoiced_row) = row.split_at_mut(bins);
        for (slot, &c) in voiced_row.iter_mut().zip(&candidates) {
            *slot = libm::log(c.max(EMISSION_FLOOR));
        }
        unvoiced_row.fill(libm::log(unvoiced.max(EMISSION_FLOOR)));
    });

    Ok(path
        .iter()
        .zip(&voiced)
        .map(|(&state, &found)| PyinFrame {
            frequency: (usize::from(state) < bins).then(|| {
                let cents = usize::from(state) as f64 * BIN.get();
                Hz::new(lowest * libm::exp2(cents / 1200.0))
            }),
            // The weights sum to 1 up to rounding, which may leave the sum a
            // rounding step above it.
            voiced: Confidence::new_clamped(found),
        })
        .collect())
}

/// Adds to `into` (one slot per grid bin) the weight of each threshold's
/// candidate in the frame `samples`, and returns the total weight placed.
fn candidate_weights(
    detector: &mut PitchDetector,
    samples: &[f64],
    weights: &[f64; THRESHOLDS],
    lowest: f64,
    into: &mut [f64],
) -> f64 {
    let Some((min_period, max_period)) = detector.normalized_difference(samples) else {
        return 0.0;
    };
    let rate = detector.sample_rate().get();
    let mut placed = 0.0;
    for (i, &weight) in weights.iter().enumerate() {
        let threshold = (i + 1) as f64 / THRESHOLDS as f64;
        let mean = detector.cumulative_mean();
        let mut tau = min_period;
        while tau < max_period && mean[tau] >= threshold {
            tau += 1;
        }
        if tau >= max_period {
            continue;
        }
        // The local minimum the dip leads to, as `YIN` selects it.
        while tau + 1 < max_period && mean[tau + 1] < mean[tau] {
            tau += 1;
        }
        let frequency = rate / detector.refined_period(tau, max_period);
        let bin = libm::round(libm::log2(frequency / lowest) * 1200.0 / BIN.get());
        if bin >= 0.0 && (bin as usize) < into.len() {
            into[bin as usize] += weight;
            placed += weight;
        }
    }
    placed
}

#[cfg(test)]
mod tests;
