//! Onset detection.
//!
//! Unlike pitch, this genuinely is "pick a detection function, then threshold
//! and peak-pick" — four published functions over one pipeline. So the entry
//! point keeps a general name and the four functions are exported
//! individually, named for themselves, because choosing between them is a real
//! choice with real trade-offs.
//!
//! An [`OnsetConfig`] describes the detection: the function, the framing in
//! samples, a threshold, a sensitivity and a minimum gap in seconds. The rate
//! binds where the audio does: [`detect_onsets`] takes a buffer and its rate,
//! and [`OnsetConfig::measure`] makes an [`OnsetMeter`] that takes the buffer
//! in chunks of any length and cuts its own frames. Spectral flux and
//! complex-domain deviation both need the previous frame, which a meter
//! carries, so a run cannot diff its first frame against another run's last.

use core::marker::PhantomData;

use tutti_buffers::{Mono, Sample};
use tutti_units::{Amplitude, SampleRate, Samples, Seconds};

use crate::convert::{for_blocks, to_f32};
use crate::framing::{FrameCutter, Framing};
use crate::{Measure, Meter, Result};
use tutti_dsp::Complex32;
use tutti_dsp::FftScratch;
use tutti_dsp::StftGeometry;
use tutti_units::ranged;

ranged! {
    /// The multiplier on the adaptive threshold: finite and not negative.
    /// Higher rejects more.
    OnsetThreshold(Amplitude), OnsetThresholdError, "an onset threshold of zero or more",
    |raw| raw.get() >= 0.0 && raw.get() <= f32::MAX
}

ranged! {
    /// The gain applied to the detection function before thresholding: finite
    /// and above zero, since a gain of zero detects nothing.
    OnsetSensitivity(Amplitude), OnsetSensitivityError, "an onset sensitivity above zero",
    |raw| raw.get() > 0.0 && raw.get() <= f32::MAX
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
    /// ([`complex_domain_deviation`]): decreases count as well as increases,
    /// so it catches energy moving between bins. Despite the name it reads
    /// magnitudes only; the phase prediction of the textbook complex-domain
    /// function is not computed.
    ComplexDomain,
}

/// Positive-rectified sum of magnitude increases.
///
/// Only *growth* counts: a bin that quietens contributes nothing, which is
/// what makes this an onset detector rather than a change detector.
pub fn spectral_flux(previous: &[f32], current: &[Complex32]) -> f32 {
    current
        .iter()
        .zip(previous)
        .map(|(bin, &prev)| (bin.norm() - prev).max(0.0))
        .sum()
}

/// Magnitude energy weighted by bin index.
///
/// Bright transients concentrate energy at high frequencies, so weighting by
/// bin favours them over low-frequency rumble.
pub fn high_frequency_content(current: &[Complex32]) -> f32 {
    current
        .iter()
        .enumerate()
        .map(|(i, bin)| (i + 1) as f32 * bin.norm_sqr())
        .sum::<f32>()
        .sqrt()
}

/// Broadband level of a time-domain frame: its L2 norm, the square root of
/// the summed squares.
///
/// Not an RMS: the sum is not divided by the frame's length, so the value
/// grows with the window (by `sqrt(len)` for a steady signal). Onset
/// detection compares frames of one length, where the scale cancels.
pub fn spectral_energy<S: Sample>(frame: Mono<'_, S>) -> f32 {
    energy(&to_f32(frame.samples()))
}

fn energy(frame: &[f32]) -> f32 {
    frame.iter().map(|s| s * s).sum::<f32>().sqrt()
}

/// Euclidean distance between successive magnitude spectra.
///
/// Unlike [`spectral_flux`] this counts decreases too, which is what makes it
/// sensitive to tonal onsets where energy moves between bins rather than
/// simply arriving. It compares magnitudes only (`previous` holds the last
/// frame's magnitudes); no phase enters it.
pub fn complex_domain_deviation(previous: &[f32], current: &[Complex32]) -> f32 {
    current
        .iter()
        .zip(previous)
        .map(|(bin, &prev)| {
            let diff = bin.norm() - prev;
            diff * diff
        })
        .sum::<f32>()
        .sqrt()
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
            framing: Framing::new(Samples(2048), Samples(512)),
            threshold: OnsetThreshold::of(Amplitude(0.3)),
            sensitivity: OnsetSensitivity::of(Amplitude::UNITY),
            min_gap: OnsetGap::of(Seconds(0.05)),
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
            threshold: self.threshold.get(),
            sensitivity: self.sensitivity.get(),
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
    threshold: Amplitude,
    sensitivity: Amplitude,
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
        Seconds(self.position.get() as f64 / sample_rate.into().get())
    }
}

/// One frame of an onset detection curve: the detection function after the
/// config's sensitivity, at the frame's first sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Novelty {
    /// The frame's first sample, in [`Samples`] of the stream.
    pub frame: Samples,
    /// The novelty at the frame, in the detection function's own scale.
    pub value: f32,
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
/// [`finish`](Meter::finish).
///
/// Spectral flux and complex-domain deviation both need the previous frame,
/// which the meter carries, so a run cannot diff its first frame against
/// another run's last.
///
/// Not for the audio thread: it keeps one novelty value per frame, so it grows
/// with the audio and allocates.
pub struct OnsetMeter<S: Sample = f32> {
    plan: OnsetPlan,
    cutter: FrameCutter,
    fft: FftScratch,
    /// The analysis window of the plan's geometry.
    window: Vec<f32>,
    /// The previous frame's magnitudes.
    previous: Vec<f32>,
    /// The current frame's bins, reused across frames.
    spectrum: Vec<Complex32>,
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
            spectrum: vec![Complex32::default(); bins],
            novelty: Vec::new(),
            sample: PhantomData,
            plan,
        }
    }

    /// Takes samples and steps each frame they complete, calling `each` with
    /// the frame's position and novelty.
    fn feed(&mut self, block: &[f32], mut each: impl FnMut(Novelty)) {
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
                DetectionFunction::Energy => energy(samples),
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
                DetectionFunction::ComplexDomain => {
                    fft.forward(Mono::new(samples), window, spectrum);
                    let v = complex_domain_deviation(previous, spectrum);
                    store_magnitudes(previous, spectrum);
                    v
                }
            };
            let position = Samples(index * plan.geometry.hop().get());
            let item = Novelty {
                frame: position,
                value: value * plan.sensitivity.get(),
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

impl<S: Sample> Meter for OnsetMeter<S> {
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
        suppress_close_onsets(&mut onsets, self.plan.min_gap);
        onsets
    }
}

/// Detects onsets across a whole buffer at `rate`.
///
/// Feeds one [`OnsetMeter`] the buffer, so the two cannot drift. `fft` is the
/// caller's scratch, lent to the meter for the call so its plans are reused. A
/// buffer shorter than one window yields no onsets.
///
/// # Errors
///
/// [`Error::ZeroWindow`](crate::Error::ZeroWindow) or
/// [`Error::ZeroHop`](crate::Error::ZeroHop) for a zero window or hop.
pub fn detect_onsets<S: Sample>(
    cfg: &OnsetConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
    fft: &mut FftScratch,
) -> Result<Vec<Onset>> {
    let plan = cfg.plan(rate.into())?;
    Ok(run(plan, &to_f32(samples.samples()), fft).finish())
}

/// A meter for `plan` fed `samples`, with the caller's scratch lent to it: the
/// whole-buffer run of the crate's own callers (tempo, notes), which hold a
/// resolved plan and `f32` samples.
pub(crate) fn run(plan: OnsetPlan, samples: &[f32], fft: &mut FftScratch) -> OnsetMeter {
    let mut meter = OnsetMeter::new(plan);
    core::mem::swap(&mut meter.fft, fft);
    meter.feed(samples, |_| {});
    core::mem::swap(&mut meter.fft, fft);
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
pub fn suppress_close_onsets(onsets: &mut Vec<Onset>, min_gap: Samples) {
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

    let len = novelty.len() as f32;
    let (sum, sum_sq, max) = novelty
        .iter()
        .fold((0.0f32, 0.0f32, 0.0f32), |(s, sq, mx), n| {
            let v = n.value;
            (s + v, sq + v * v, mx.max(v))
        });
    let mean = sum / len;
    let std_dev = (sum_sq / len - mean * mean).max(0.0).sqrt();
    let threshold = mean + std_dev * cfg.threshold.get() * 3.0;

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
                strength: Amplitude(if max > 0.0 {
                    (value / max).min(1.0)
                } else {
                    0.0
                }),
            })
        })
        .collect()
}

fn store_magnitudes(previous: &mut [f32], spectrum: &[Complex32]) {
    for (slot, bin) in previous.iter_mut().zip(spectrum) {
        *slot = bin.norm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold_uneven;

    const SAMPLE_RATE: f64 = 44100.0;

    fn rate() -> SampleRate {
        SampleRate::of(SAMPLE_RATE)
    }

    fn plan(cfg: OnsetConfig) -> OnsetPlan {
        cfg.plan(rate()).unwrap()
    }

    fn flux() -> OnsetConfig {
        OnsetConfig::new(DetectionFunction::SpectralFlux)
    }

    fn detect_at(cfg: &OnsetConfig, samples: &[f32], fft: &mut FftScratch) -> Vec<Onset> {
        detect_onsets(cfg, Mono::new(samples), rate(), fft).unwrap()
    }

    fn amplitude(a: f32) -> OnsetThreshold {
        OnsetThreshold::of(Amplitude(a))
    }

    fn gain(a: f32) -> OnsetSensitivity {
        OnsetSensitivity::of(Amplitude(a))
    }

    fn gap(seconds: f64) -> OnsetGap {
        OnsetGap::of(Seconds(seconds))
    }

    /// The curve has one entry per frame, at the frame's start sample, scaled by
    /// the config's sensitivity, and a click raises the frame that holds it.
    #[test]
    fn the_novelty_curve_is_readable_per_frame_and_scaled_by_sensitivity() {
        let samples = signal(0.5, &[0.2]);
        let run = |factor: f32| {
            let mut meter = OnsetConfig::new(DetectionFunction::Energy)
                .sensitivity(gain(factor))
                .measure(rate())
                .unwrap();
            meter.push(Mono::new(&samples), &mut Vec::new()).unwrap();
            meter
        };
        let (one, three) = (run(1.0), run(3.0));
        for (i, n) in one.novelty().iter().enumerate() {
            assert_eq!(n.frame, Samples(i * 512));
        }
        for (a, b) in one.novelty().iter().zip(three.novelty()) {
            assert!((b.value - 3.0 * a.value).abs() <= 1e-4 * a.value.max(1.0));
        }
        // Frames holding the click (samples 8820..8870) carry its energy; a
        // frame wholly before it carries none.
        let at = |s: usize| one.novelty()[s / 512].value;
        assert!(at(8820) > 1.0 && at(2048) == 0.0);

        let mut cleared = one;
        cleared.reset();
        assert!(cleared.novelty().is_empty());
    }

    fn signal(seconds: f64, onsets: &[f64]) -> Vec<f32> {
        let n = (SAMPLE_RATE * seconds) as usize;
        let mut samples = vec![0.0f32; n];
        for &t in onsets {
            let pos = (t * SAMPLE_RATE) as usize;
            for i in 0..50.min(n.saturating_sub(pos)) {
                samples[pos + i] += (-0.1 * i as f32).exp() * 0.8;
            }
        }
        samples
    }

    /// The property the explicit carry buys: a meter fed chunks of any size
    /// and the one-shot agree bit for bit, because they run the same step.
    #[test]
    fn a_meter_fed_in_chunks_equals_the_one_shot() {
        let cfg = flux().threshold(amplitude(0.2)).sensitivity(gain(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let mut fft = FftScratch::new();

        let batch = detect_at(&cfg, &samples, &mut fft);
        assert_eq!(batch.len(), 4);

        for chunk in [1usize, 7, 511, 2048, 3001, samples.len()] {
            let mut meter = cfg.measure(rate()).unwrap();
            for part in samples.chunks(chunk) {
                meter.push(Mono::new(part), &mut Vec::new()).unwrap();
            }
            let curve = meter.novelty().to_vec();
            assert_eq!(meter.finish(), batch, "chunk {chunk}");

            let reference = cfg.measure(rate()).unwrap();
            let mut whole = reference;
            whole.push(Mono::new(&samples), &mut Vec::new()).unwrap();
            assert_eq!(curve, whole.novelty(), "chunk {chunk}");
        }
    }

    /// Every sample format reads at its nominal level: a 16-bit copy of the
    /// signal gives the novelty of the `f32` signal to within quantisation.
    #[test]
    fn an_integer_buffer_is_read_at_its_nominal_level() {
        let cfg = flux();
        let samples = signal(0.5, &[0.1, 0.3]);
        let ints: Vec<i16> = samples
            .iter()
            .map(|&s| (s * f32::from(i16::MAX)) as i16)
            .collect();
        let mut float = cfg.measure(rate()).unwrap();
        float.push(Mono::new(&samples), &mut Vec::new()).unwrap();
        let mut int = cfg.measure(rate()).unwrap();
        int.push(Mono::new(&ints), &mut Vec::new()).unwrap();
        assert_eq!(float.novelty().len(), int.novelty().len());
        for (a, b) in float.novelty().iter().zip(int.novelty()) {
            assert!(
                (a.value - b.value).abs() <= 1e-2 * a.value.max(1.0),
                "{a:?} {b:?}"
            );
        }
    }

    /// A reused meter must be reset; a fresh one needs nothing.
    #[test]
    fn a_reset_meter_matches_a_fresh_one() {
        let a = signal(0.5, &[0.1, 0.3]);
        let b = signal(0.5, &[0.2]);

        let mut reused = flux().measure(rate()).unwrap();
        reused.push(Mono::new(&a), &mut Vec::new()).unwrap();
        reused.reset();
        reused.push(Mono::new(&b), &mut Vec::new()).unwrap();

        let mut fresh = flux().measure(rate()).unwrap();
        fresh.push(Mono::new(&b), &mut Vec::new()).unwrap();

        assert_eq!(reused.finish(), fresh.finish());
    }

    /// The per-frame spectrum buffer is allocated once, not per frame.
    ///
    /// A fresh `vec![Complex32; bins]` per frame cost one allocation per
    /// frame, tens of thousands for a few minutes of audio.
    ///
    /// Asserted by **reserving past the buffer and seeing the reserve
    /// survive**: `capacity()` is identical for a refilled buffer and a fresh
    /// `vec![_; bins]`, but a buffer that is *replaced* loses the spare
    /// capacity and one refilled in place keeps it.
    #[test]
    fn the_spectrum_buffer_is_reused_across_frames() {
        let mut meter = flux().measure(rate()).unwrap();
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let bins = meter.spectrum.len();
        assert!(bins > 0);

        meter.spectrum.reserve_exact(bins * 4);
        let reserved = meter.spectrum.capacity();
        assert!(reserved > bins, "the reserve must actually over-allocate");

        meter.push(Mono::new(&samples), &mut Vec::new()).unwrap();

        assert!(meter.novelty().len() > 8, "the run must cover many frames");
        assert_eq!(meter.spectrum.len(), bins, "one frame of bins");
        assert_eq!(
            meter.spectrum.capacity(),
            reserved,
            "every frame must refill the same allocation"
        );
    }

    #[test]
    fn onsets_land_near_the_known_spikes() {
        let truth = [0.1f64, 0.3, 0.5, 0.7];
        let samples = signal(1.0, &truth);
        let mut fft = FftScratch::new();

        for function in [
            DetectionFunction::SpectralFlux,
            DetectionFunction::HighFrequencyContent,
            DetectionFunction::ComplexDomain,
        ] {
            let cfg = OnsetConfig::new(function)
                .threshold(amplitude(0.2))
                .sensitivity(gain(2.0));
            let onsets = detect_at(&cfg, &samples, &mut fft);

            assert_eq!(onsets.len(), truth.len(), "{function:?}: wrong count");

            // Reported early by up to one window — a frame is attributed to
            // its start offset, so the error grows with how deep into the
            // frame the spike falls.
            let tolerance = plan(cfg).geometry().window().get() as f64 / SAMPLE_RATE;
            for (found, &expected) in onsets.iter().zip(&truth) {
                let t = found.time(SampleRate::of(SAMPLE_RATE)).get();
                assert!(
                    (t - expected).abs() <= tolerance,
                    "{function:?}: onset at {t} is not within one window of {expected}"
                );
            }
        }
    }

    /// `Energy` misses these. Pinned so the
    /// weakness stays a known property rather than becoming a silent change.
    #[test]
    fn energy_misses_short_decaying_spikes() {
        let cfg = OnsetConfig::new(DetectionFunction::Energy)
            .threshold(amplitude(0.2))
            .sensitivity(gain(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let mut fft = FftScratch::new();

        assert!(detect_at(&cfg, &samples, &mut fft).is_empty());
    }

    /// The defaults a config documents: a 0.3 threshold multiplier, unity
    /// sensitivity, a 50 ms gap, and frames of 2048 samples every 512.
    #[test]
    fn the_defaults_are_the_documented_ones() {
        let p = plan(flux());
        assert_eq!(p.threshold, Amplitude(0.3));
        assert_eq!(p.sensitivity, Amplitude::UNITY);
        assert_eq!(p.min_gap, Samples(2205));
        assert_eq!(p.geometry().window(), Samples(2048));
        assert_eq!(p.geometry().hop(), Samples(512));
    }

    #[test]
    fn the_gap_is_expressed_in_seconds() {
        let cfg = flux().min_gap(gap(0.05));
        assert_eq!(plan(cfg).min_gap, Samples(2205));
        assert_eq!(plan(cfg.min_gap(gap(0.5))).min_gap, Samples(22050));
        // The same seconds are twice the samples at twice the rate.
        assert_eq!(
            cfg.plan(SampleRate::of(88200.0)).unwrap().min_gap,
            Samples(4410)
        );
    }

    #[test]
    fn a_wide_gap_suppresses_close_onsets() {
        let cfg = flux().threshold(amplitude(0.2)).sensitivity(gain(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let mut fft = FftScratch::new();

        let baseline = detect_at(&cfg, &samples, &mut fft).len();
        let widened = detect_at(&cfg.min_gap(gap(0.5)), &samples, &mut fft);

        assert!(widened.len() < baseline);
    }

    #[test]
    fn suppression_keeps_the_stronger_of_a_close_pair() {
        let onset = |pos: usize, strength: f32| Onset {
            position: Samples(pos),
            strength: Amplitude(strength),
        };

        let mut onsets = vec![onset(100, 0.4), onset(150, 0.9), onset(9000, 0.5)];
        suppress_close_onsets(&mut onsets, Samples(1000));
        assert_eq!(onsets.len(), 2);
        assert_eq!(onsets[0].strength, Amplitude(0.9));

        let mut onsets = vec![onset(100, 0.9), onset(150, 0.4), onset(9000, 0.5)];
        suppress_close_onsets(&mut onsets, Samples(1000));
        assert_eq!(onsets[0].strength, Amplitude(0.9));

        // Degenerate inputs are no-ops.
        let mut single = vec![onset(100, 0.5)];
        suppress_close_onsets(&mut single, Samples(1000));
        assert_eq!(single.len(), 1);
    }

    /// Each onset is compared with the last one *kept*, not with its
    /// neighbour in the input: a falling run 600 apart under a 1000 gap keeps
    /// 0 (600 is too close to it), then 1200 (1200 from 0), then drops 1800.
    #[test]
    fn suppression_measures_from_the_last_kept_onset() {
        let at = |p: usize, s: f32| Onset {
            position: Samples(p),
            strength: Amplitude(s),
        };
        let mut onsets = vec![at(0, 1.0), at(600, 0.9), at(1200, 0.8), at(1800, 0.7)];
        suppress_close_onsets(&mut onsets, Samples(1000));
        let kept: Vec<usize> = onsets.iter().map(|o| o.position.get()).collect();
        assert_eq!(kept, [0, 1200]);
    }

    /// Unsorted input must not underflow.
    ///
    /// `pick_peaks` emits ascending positions, but the function is public and
    /// takes a plain `Vec`. Subtracting `usize` positions would panic in debug
    /// and wrap to a huge value in release, where the pair would silently never
    /// be suppressed.
    #[test]
    fn suppression_handles_unsorted_input() {
        let onset = |pos: usize, strength: f32| Onset {
            position: Samples(pos),
            strength: Amplitude(strength),
        };

        let mut descending = vec![onset(9000, 0.4), onset(100, 0.9)];
        suppress_close_onsets(&mut descending, Samples(1000));
        assert_eq!(descending.len(), 2, "far apart in either direction");

        let mut close = vec![onset(150, 0.4), onset(100, 0.9)];
        suppress_close_onsets(&mut close, Samples(1000));
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].strength, Amplitude(0.9));
    }

    #[test]
    fn detection_functions_respond_to_what_they_are_for() {
        let loud = vec![Complex32::new(1.0, 0.0); 16];
        let previous = vec![0.1f32; 16];

        // Flux counts growth only.
        assert!(spectral_flux(&previous, &loud) > 0.0);
        let falling = vec![Complex32::new(0.01, 0.0); 16];
        assert_eq!(spectral_flux(&previous, &falling), 0.0);

        // Complex-domain counts change in either direction.
        assert!(complex_domain_deviation(&previous, &falling) > 0.0);

        // HFC weights high bins more than low ones.
        let mut low = vec![Complex32::default(); 16];
        low[1] = Complex32::new(1.0, 0.0);
        let mut high = vec![Complex32::default(); 16];
        high[15] = Complex32::new(1.0, 0.0);
        assert!(high_frequency_content(&high) > high_frequency_content(&low));

        assert!(
            spectral_energy(Mono::new(&[1.0f32; 4])) > spectral_energy(Mono::new(&[0.1f32; 4]))
        );
    }

    #[test]
    fn input_shorter_than_a_window_yields_nothing() {
        let mut fft = FftScratch::new();
        assert!(detect_at(&flux(), &[0.0; 100], &mut fft).is_empty());
        let mut meter = flux().measure(rate()).unwrap();
        meter
            .push(Mono::new(&[0.0f32; 100]), &mut Vec::new())
            .unwrap();
        assert!(meter.novelty().is_empty());
        assert!(meter.finish().is_empty());
    }

    /// A zero window or hop is refused by the call that takes the rate.
    #[test]
    fn a_zero_framing_is_refused_where_the_rate_binds() {
        for framing in [
            Framing::new(Samples(0), Samples(512)),
            Framing::new(Samples(2048), Samples(0)),
        ] {
            let cfg = flux().framing(framing);
            assert!(matches!(
                Measure::<f32>::measure(&cfg, rate()),
                Err(crate::Error::ZeroWindow | crate::Error::ZeroHop)
            ));
            assert!(matches!(
                detect_onsets(
                    &cfg,
                    Mono::new(&[0.0f32; 4096]),
                    rate(),
                    &mut FftScratch::new()
                ),
                Err(crate::Error::ZeroWindow | crate::Error::ZeroHop)
            ));
        }
    }

    /// The trait fold over uneven chunks (1, 7, 64 samples and the rest)
    /// yields the novelty curve as its items and `detect_onsets` as its output.
    #[test]
    fn the_trait_fold_over_uneven_chunks_equals_detect_onsets_with_the_curve_as_items() {
        let cfg = flux().threshold(amplitude(0.2)).sensitivity(gain(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let batch = detect_at(&cfg, &samples, &mut FftScratch::new());
        assert_eq!(batch.len(), 4);
        let (items, onsets) = fold_uneven(&cfg, rate(), samples.len(), |meter, at, out| {
            meter.push(Mono::new(&samples[at]), out)
        })
        .unwrap();
        assert_eq!(onsets, batch);
        let mut whole = cfg.measure(rate()).unwrap();
        whole.push(Mono::new(&samples), &mut Vec::new()).unwrap();
        assert_eq!(items, whole.novelty());
    }
}
