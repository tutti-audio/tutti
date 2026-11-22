//! Onset detection.
//!
//! Unlike pitch, this genuinely is "pick a detection function, then threshold
//! and peak-pick" — four published functions over one pipeline. So the entry
//! point keeps a general name and the four functions are
//! [`tutti_dsp`]'s, named for themselves, because choosing between them is a
//! real choice with real trade-offs.
//!
//! An [`OnsetConfig`] describes the detection: the function, the framing in
//! samples, a threshold, a sensitivity and a minimum gap in seconds. The rate
//! binds where the audio does: [`detect_onsets`] takes a buffer and its rate,
//! and [`OnsetConfig::measure`] makes an [`OnsetMeter`] that takes the buffer
//! in chunks of any length and cuts its own frames. Spectral flux and
//! spectral distance both need the previous frame, which a meter
//! carries, so a run cannot diff its first frame against another run's last.

use core::marker::PhantomData;

use tutti_buffers::{Mono, Sample};
use tutti_units::{Amplitude, SampleRate, Samples, Seconds};

use crate::convert::{for_blocks, widen};
use crate::framing::{FrameCutter, Framing};
use crate::{Measure, Result, StreamMeter};
use tutti_dsp::Complex64;
use tutti_dsp::FftScratch;
use tutti_dsp::StftGeometry;
use tutti_dsp::{
    bin_magnitude, high_frequency_content, spectral_distance, spectral_energy, spectral_flux,
};
use tutti_units::ranged;

ranged! {
    /// The multiplier on the adaptive threshold: finite and not negative.
    /// Higher rejects more.
    OnsetThreshold(Amplitude), OnsetThresholdError, "an onset threshold of zero or more",
    |raw| raw.get() >= 0.0 && raw.get().is_finite()
}

ranged! {
    /// The gain applied to the detection function before thresholding: finite and above zero,
    /// since a gain of zero detects nothing.
    OnsetSensitivity(Amplitude), OnsetSensitivityError, "an onset sensitivity above zero",
    |raw| raw.get() > 0.0 && raw.get().is_finite()
}

ranged! {
    /// The shortest spacing between accepted onsets: within `0..=60` seconds.
    OnsetGap(Seconds), OnsetGapError, "an onset gap within 0..=60 s",
    |raw| raw.get() >= 0.0 && raw.get() <= 60.0
}

/// How novelty is measured frame to frame.
///
/// Each is sensitive to a different kind of change, and the differences are
/// audible in what they miss — see the per-function docs.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
pub enum DetectionFunction {
    /// Positive-rectified magnitude change. The general-purpose default.
    SpectralFlux,
    /// Magnitude weighted toward high bins. Favours percussive, bright onsets.
    HighFrequencyContent,
    /// Broadband level change. Cheapest, and the least sensitive — it misses
    /// short decaying spikes that the spectral functions catch.
    Energy,
    /// Euclidean distance between successive magnitude spectra
    /// ([`spectral_distance`]): decreases count as well as increases,
    /// so it catches energy moving between bins. It reads magnitudes only (no
    /// phase), so it is not the phase-predicted complex-domain function of
    /// Bello et al. (2004).
    SpectralDistance,
}

/// Onset detection parameters. Immutable once built, and free of a sample
/// rate: [`detect_onsets`] and [`OnsetConfig::measure`] take it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnsetConfig {
    function: DetectionFunction,
    framing: Framing,
    threshold: OnsetThreshold,
    sensitivity: OnsetSensitivity,
    min_gap: OnsetGap,
}

impl OnsetConfig {
    /// Detects with `function` over frames of 2048 samples every 512, with a
    /// 0.3 threshold multiplier, unity sensitivity and a 50 ms minimum gap.
    #[must_use]
    pub fn new(function: DetectionFunction) -> Self {
        Self {
            function,
            framing: Framing::new(Samples::new(2048), Samples::new(512)),
            threshold: OnsetThreshold::of(Amplitude::new(0.3)),
            sensitivity: OnsetSensitivity::of(Amplitude::UNITY),
            min_gap: OnsetGap::of(Seconds::new(0.05)),
        }
    }

    /// The window and hop frames are taken at. A zero window or hop is refused
    /// by the call that takes the audio.
    #[must_use]
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    /// Multiplier on the adaptive threshold. Higher rejects more.
    #[must_use]
    pub fn threshold(mut self, threshold: OnsetThreshold) -> Self {
        self.threshold = threshold;
        self
    }

    /// Gain applied to the detection function before thresholding.
    #[must_use]
    pub fn sensitivity(mut self, sensitivity: OnsetSensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    /// Minimum spacing between accepted onsets.
    ///
    /// Takes seconds, like every other time-valued setting in the engine:
    /// milliseconds on one setter and seconds on another of the same type is
    /// how a gap ends up a thousand times off.
    #[must_use]
    pub fn min_gap(mut self, gap: OnsetGap) -> Self {
        self.min_gap = gap;
        self
    }

    /// Which detection function measures novelty between frames.
    #[inline]
    pub(crate) fn function(&self) -> DetectionFunction {
        self.function
    }

    /// The config at `rate`.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroWindow`](crate::Error::ZeroWindow) or
    /// [`Error::ZeroHop`](crate::Error::ZeroHop) for a zero window or hop.
    pub(crate) fn plan(&self, rate: SampleRate) -> Result<OnsetPlan> {
        Ok(OnsetPlan {
            geometry: self.framing.geometry(rate)?,
            function: self.function,
            threshold: self.threshold.get().get(),
            sensitivity: self.sensitivity.get().get(),
            min_gap: self.min_gap.get().to_samples(rate),
        })
    }
}

impl<S: Sample> Measure<S> for OnsetConfig {
    type Context = SampleRate;
    type Meter = OnsetMeter<S>;

    /// A meter that detects onsets in audio at `rate`, chunk by chunk.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroWindow`](crate::Error::ZeroWindow) or
    /// [`Error::ZeroHop`](crate::Error::ZeroHop) for a zero window or hop.
    fn measure(&self, rate: SampleRate) -> Result<OnsetMeter<S>> {
        Ok(OnsetMeter::new(self.plan(rate)?))
    }
}

/// An [`OnsetConfig`] at one sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OnsetPlan {
    geometry: StftGeometry,
    function: DetectionFunction,
    threshold: f64,
    sensitivity: f64,
    min_gap: Samples,
}

impl OnsetPlan {
    /// The window, hop and sample rate onsets are detected on.
    #[inline]
    pub(crate) fn geometry(&self) -> StftGeometry {
        self.geometry
    }
}

/// One detected onset.
///
/// Position only, with the time derived from it on demand. Storing both invites
/// computing them against two different sample rates, so one struct describes
/// two different instants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Onset {
    /// Where the onset sits in the analyzed buffer, in [`Samples`].
    pub position: Samples,
    /// Novelty at that frame as an [`Amplitude`] — how far the detection
    /// function rose above its adaptive threshold.
    pub strength: Amplitude,
}

impl Onset {
    /// The onset's position in [`Seconds`], resolved against `sample_rate`.
    ///
    /// Derived rather than stored, so it cannot disagree with
    /// [`position`](Self::position).
    #[inline]
    pub fn time(&self, sample_rate: impl Into<SampleRate>) -> Seconds {
        Seconds::new(self.position.get() as f64 / sample_rate.into().get())
    }
}

/// One frame of an onset detection curve: the detection function after the
/// config's sensitivity, at the frame's first sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Novelty {
    /// The frame's first sample, in [`Samples`] of the stream.
    pub frame: Samples,
    /// The novelty at the frame, in the detection function's own scale.
    pub value: f64,
}

/// How far after the start of its frame the onset a spectral-flux peak marks
/// sits: `3 * window / 4 - hop / 2`.
///
/// Flux is the rise in the Hann-weighted magnitude from one frame to the next.
/// An onset at `p` into a frame was `p + hop` into the last, so the rise is
/// `w(p) - w(p + hop)`, about `-hop * w'` at `p + hop / 2`, and `w' = sin(2πp/W)
/// π/W` falls fastest at `3W/4`. The frame grid then places the peak to within
/// half a hop. (Not `window / 2`: that is right only when `hop = window / 2`.)
pub(crate) fn onset_lag(geometry: StftGeometry) -> Samples {
    (geometry.window() * 3 / 4).remaining_after(geometry.hop() / 2)
}

/// A running onset detection at one rate: fed chunks of the buffer in order,
/// of any length, it cuts the frames at the hop, yields each frame's novelty
/// as an item (its position and the value) and reads the onsets at
/// [`finish`](StreamMeter::finish).
///
/// Spectral flux and spectral distance both need the previous frame,
/// which the meter carries, so a run cannot diff its first frame against
/// another run's last.
///
/// Not for the audio thread: it keeps one novelty value per frame, so it grows
/// with the audio and allocates.
pub struct OnsetMeter<S: Sample> {
    plan: OnsetPlan,
    cutter: FrameCutter,
    fft: FftScratch<f64>,
    /// The analysis window of the plan's geometry.
    window: Vec<f64>,
    /// The previous frame's magnitudes.
    previous: Vec<f64>,
    /// The current frame's bins, reused across frames.
    spectrum: Vec<Complex64>,
    /// Novelty per frame, in analysis order. Thresholding is global, so peaks
    /// can only be picked once the run is complete.
    novelty: Vec<Novelty>,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> core::fmt::Debug for OnsetMeter<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OnsetMeter")
            .field("plan", &self.plan)
            .field("frames", &self.novelty.len())
            .finish_non_exhaustive()
    }
}

impl<S: Sample> OnsetMeter<S> {
    fn new(plan: OnsetPlan) -> Self {
        let bins = plan.geometry.bins_per_frame().get();
        Self {
            cutter: FrameCutter::new(Framing::new(plan.geometry.window(), plan.geometry.hop())),
            fft: FftScratch::new(),
            window: plan.geometry.window_coefficients(),
            previous: vec![0.0; bins],
            spectrum: vec![Complex64::default(); bins],
            novelty: Vec::new(),
            sample: PhantomData,
            plan,
        }
    }

    /// Takes samples and steps each frame they complete, calling `each` with
    /// the frame's position and novelty.
    fn feed(&mut self, block: &[f64], mut each: impl FnMut(Novelty)) {
        let Self {
            plan,
            cutter,
            fft,
            window,
            previous,
            spectrum,
            novelty,
            ..
        } = self;
        cutter.feed(block, |index, samples| {
            // Every spectral branch stores its magnitudes, including the ones
            // that do not read them. Skipping the store leaves `previous`
            // stale or zeroed, so the first frame after a switch measured
            // novelty against the wrong reference, which then dominates the
            // adaptive threshold for the whole run.
            let value = match plan.function {
                DetectionFunction::Energy => spectral_energy(Mono::new(samples)),
                DetectionFunction::SpectralFlux => {
                    fft.forward(Mono::new(samples), window, spectrum);
                    let v = spectral_flux(previous, spectrum);
                    store_magnitudes(previous, spectrum);
                    v
                }
                DetectionFunction::HighFrequencyContent => {
                    fft.forward(Mono::new(samples), window, spectrum);
                    let v = high_frequency_content(spectrum) * 0.01;
                    store_magnitudes(previous, spectrum);
                    v
                }
                DetectionFunction::SpectralDistance => {
                    fft.forward(Mono::new(samples), window, spectrum);
                    let v = spectral_distance(previous, spectrum);
                    store_magnitudes(previous, spectrum);
                    v
                }
            };
            let position = Samples::new(index * plan.geometry.hop().get());
            let item = Novelty {
                frame: position,
                value: value * plan.sensitivity,
            };
            novelty.push(item);
            each(item);
        });
    }

    /// The detection curve so far: one [`Novelty`] per frame
    /// analysed, in analysis order.
    ///
    /// The raw values [`finish`](Self::finish) thresholds, after the config's
    /// sensitivity and before any peak picking. The scale depends on the
    /// [`DetectionFunction`] and is only comparable within one run.
    #[inline]
    #[must_use]
    pub fn novelty(&self) -> &[Novelty] {
        &self.novelty
    }

    /// Forgets everything fed. Equivalent to a fresh meter at the same rate.
    pub fn reset(&mut self) {
        self.cutter.reset();
        self.previous.fill(0.0);
        self.novelty.clear();
    }
}

impl<S: Sample> StreamMeter for OnsetMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = Novelty;
    type Output = Vec<Onset>;

    /// Feeds the next chunk of the buffer, in order, and extends `out` with the
    /// [`Novelty`] of each frame the chunk completes.
    fn push(&mut self, chunk: Mono<'_, S>, out: &mut impl Extend<Novelty>) -> Result<()> {
        for_blocks(chunk.samples(), 1, |block| {
            self.feed(block, |item| out.extend([item]));
        });
        Ok(())
    }

    /// The onsets of the audio fed: peaks of the novelty above its adaptive
    /// threshold, closer than the minimum gap resolved to the stronger.
    fn finish(self) -> Vec<Onset> {
        let mut onsets = pick_peaks(&self.plan, &self.novelty);
        suppress_close_onsets(self.plan.min_gap, &mut onsets);
        onsets
    }
}

/// Detects onsets across a whole buffer at `rate`.
///
/// Feeds one [`OnsetMeter`] the buffer, so the two cannot drift. A buffer
/// shorter than one window yields no onsets.
///
/// # Errors
///
/// [`Error::ZeroWindow`](crate::Error::ZeroWindow) or
/// [`Error::ZeroHop`](crate::Error::ZeroHop) for a zero window or hop.
pub fn detect_onsets<S: Sample>(
    cfg: &OnsetConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Vec<Onset>> {
    let plan = cfg.plan(rate.into())?;
    Ok(run(plan, &widen(samples.samples())).finish())
}

/// A meter for `plan` fed `samples`: the whole-buffer run of the crate's own
/// callers (tempo, notes), which hold a resolved plan and `f64` samples.
pub(crate) fn run(plan: OnsetPlan, samples: &[f64]) -> OnsetMeter<f64> {
    let mut meter = OnsetMeter::<f64>::new(plan);
    meter.feed(samples, |_| {});
    meter
}

/// Drops onsets closer together than `min_gap`, keeping the stronger of a pair.
///
/// Walks the list once: each onset is compared with the last one kept, and
/// either replaces it (when stronger), is dropped, or is kept after it.
///
/// Order-independent: the gap is a distance, so an unsorted list is compared
/// correctly rather than underflowing. `pick_peaks` emits ascending positions,
/// but this is public and takes a plain `Vec`, so it cannot assume that.
pub fn suppress_close_onsets(min_gap: Samples, onsets: &mut Vec<Onset>) {
    if onsets.len() < 2 {
        return;
    }
    // `onsets[..kept]` is the result so far; `onsets[kept - 1]` the last kept.
    let mut kept = 1;
    for i in 1..onsets.len() {
        let next = onsets[i];
        let last = onsets[kept - 1];
        let too_close = next.position.get().abs_diff(last.position.get()) < min_gap.get();
        if !too_close {
            onsets[kept] = next;
            kept += 1;
        } else if next.strength > last.strength {
            onsets[kept - 1] = next;
        }
    }
    onsets.truncate(kept);
}

/// Strict local maxima above an adaptive `mean + k·σ` threshold.
///
/// Endpoints are never reported: a maximum needs both neighbours.
fn pick_peaks(cfg: &OnsetPlan, novelty: &[Novelty]) -> Vec<Onset> {
    if novelty.len() < 3 {
        return Vec::new();
    }

    let len = novelty.len() as f64;
    let max = novelty.iter().fold(0.0f64, |mx, n| mx.max(n.value));
    let mean = novelty.iter().map(|n| n.value).sum::<f64>() / len;
    // Two passes: the squared deviations are small whatever the level, where
    // `sum_sq / len - mean^2` cancels at a large one.
    let variance = novelty
        .iter()
        .map(|n| (n.value - mean).powi(2))
        .sum::<f64>()
        / len;
    let std_dev = variance.sqrt();
    let threshold = mean + std_dev * cfg.threshold * 3.0;

    (1..novelty.len() - 1)
        .filter_map(|i| {
            let Novelty {
                frame: position,
                value,
            } = novelty[i];
            let rising = value > novelty[i - 1].value;
            let falling = value > novelty[i + 1].value;
            (rising && falling && value > threshold).then(|| Onset {
                position,
                strength: Amplitude::new(if max > 0.0 {
                    (value / max).min(1.0)
                } else {
                    0.0
                }),
            })
        })
        .collect()
}

fn store_magnitudes(previous: &mut [f64], spectrum: &[Complex64]) {
    for (slot, bin) in previous.iter_mut().zip(spectrum) {
        *slot = bin_magnitude(*bin);
    }
}

#[cfg(test)]
mod tests;
