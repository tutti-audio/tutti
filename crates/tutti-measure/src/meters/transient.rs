//! Transient detection to the sample.
//!
//! The onset meter ([`OnsetMeter`](crate::OnsetMeter)) places an onset to its frame hop. A
//! [`TransientConfig`] places an attack to the sample, for slicing or as the key of a transient
//! shaper: it runs [`tutti_dsp::TransientDetector`], the fast-minus-slow envelope difference of
//! the transient shaper's gain, with a threshold and a minimum gap, and reads each attack's
//! position as the sample of the fast envelope's steepest rise (see the detector's docs).
//!
//! The rate binds where the audio does: [`detect_transients`] takes a buffer and its rate, and
//! [`TransientConfig::measure`] makes a [`TransientMeter`] that takes the buffer in chunks of
//! any length. Not for the audio thread: the meter keeps every position, so it grows with the
//! attacks found.

use core::marker::PhantomData;

use tutti_buffers::{Mono, Sample};
use tutti_dsp::TransientDetector;
use tutti_units::{Amplitude, SampleRate, Samples, Seconds, ranged};

use crate::convert::for_blocks;
use crate::meters::onset::OnsetGap;
use crate::{Error, Measure, Result, StreamMeter};

ranged! {
    /// How far the fast envelope must rise above the slow one for an attack: finite and above
    /// zero, in the signal's own scale (a full-scale sample is `1.0`).
    TransientThreshold(Amplitude), TransientThresholdError, "a transient threshold above zero",
    |raw| raw.get() > 0.0 && raw.get().is_finite()
}

ranged! {
    /// The time constant of one of the two envelopes: within `0.1 ms..=1 s`.
    TransientTime(Seconds), TransientTimeError, "an envelope time within 0.1 ms..=1 s",
    |raw| raw.get() >= 1e-4 && raw.get() <= 1.0
}

/// Transient detection parameters. Immutable once built, and free of a sample rate:
/// [`detect_transients`] and [`TransientConfig::measure`] take it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransientConfig {
    threshold: TransientThreshold,
    fast: TransientTime,
    slow: TransientTime,
    min_gap: OnsetGap,
}

impl TransientConfig {
    /// Detects with a 0.05 threshold, a 1 ms fast and a 30 ms slow envelope and a 50 ms
    /// minimum gap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            threshold: TransientThreshold::of(Amplitude::new(0.05)),
            fast: TransientTime::of(Seconds::new(0.001)),
            slow: TransientTime::of(Seconds::new(0.03)),
            min_gap: OnsetGap::of(Seconds::new(0.05)),
        }
    }

    /// How far the fast envelope must stand above the slow one. An attack of level `L` that
    /// reaches the threshold needs `L` times the fast envelope's per-sample step above it;
    /// quieter material needs a lower threshold. A periodic signal's own ripple in the fast
    /// envelope must stay under it.
    #[must_use]
    pub fn threshold(mut self, threshold: TransientThreshold) -> Self {
        self.threshold = threshold;
        self
    }

    /// The fast envelope's time constant: the shorter it is, the shorter an attack it sees and
    /// the more of a low tone's ripple it follows.
    #[must_use]
    pub fn fast(mut self, time: TransientTime) -> Self {
        self.fast = time;
        self
    }

    /// The slow envelope's time constant: the level an attack is measured against, and the
    /// slope of a fade it tolerates.
    #[must_use]
    pub fn slow(mut self, time: TransientTime) -> Self {
        self.slow = time;
        self
    }

    /// The least spacing between reported positions.
    #[must_use]
    pub fn min_gap(mut self, gap: OnsetGap) -> Self {
        self.min_gap = gap;
        self
    }
}

impl Default for TransientConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Measure<S> for TransientConfig {
    type Context = SampleRate;
    type Meter = TransientMeter<S>;

    /// A meter that detects transients in audio at `rate`, chunk by chunk.
    ///
    /// # Errors
    ///
    /// [`Error::TransientTimes`] when the fast time is not below the slow one.
    fn measure(&self, rate: SampleRate) -> Result<TransientMeter<S>> {
        let (fast, slow) = (self.fast.get(), self.slow.get());
        if fast >= slow {
            return Err(Error::TransientTimes { fast, slow });
        }
        Ok(TransientMeter {
            detector: TransientDetector::new(
                rate,
                fast,
                slow,
                self.threshold.get(),
                self.min_gap.get().to_samples(rate),
            ),
            found: Vec::new(),
            sample: PhantomData,
        })
    }
}

/// A running transient detection at one rate: fed chunks of the buffer in order, of any
/// length, it yields each attack's position, in [`Samples`] of the stream, as the chunk that
/// confirms it is pushed.
///
/// A NaN sample reads as quiet and an infinite one as the largest finite level.
pub struct TransientMeter<S: Sample> {
    detector: TransientDetector<f64>,
    found: Vec<Samples>,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> core::fmt::Debug for TransientMeter<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransientMeter")
            .field("found", &self.found.len())
            .finish_non_exhaustive()
    }
}

impl<S: Sample> TransientMeter<S> {
    /// Forgets everything fed. Equivalent to a fresh meter at the same rate.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.found.clear();
    }
}

impl<S: Sample> StreamMeter for TransientMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = Samples;
    type Output = Vec<Samples>;

    /// Feeds the next chunk, in order, and extends `out` with the position of each attack the
    /// chunk confirms. A position can lie in an earlier chunk than the one that confirms it.
    fn push(&mut self, chunk: Mono<'_, S>, out: &mut impl Extend<Samples>) -> Result<()> {
        for_blocks(chunk.samples(), 1, |block| {
            for &x in block {
                if let Some(position) = self.detector.push(x) {
                    self.found.push(position);
                    out.extend([position]);
                }
            }
        });
        Ok(())
    }

    /// The positions of every attack fed, in order.
    fn finish(self) -> Vec<Samples> {
        self.found
    }
}

/// Detects transients across a whole buffer at `rate`.
///
/// Feeds one [`TransientMeter`] the buffer, so the two cannot drift.
///
/// # Errors
///
/// [`Error::TransientTimes`] when the config's fast time is not below its slow one.
pub fn detect_transients<S: Sample>(
    cfg: &TransientConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Vec<Samples>> {
    let mut meter = Measure::<S>::measure(cfg, rate.into())?;
    meter.push(samples, &mut crate::convert::Discard)?;
    Ok(meter.finish())
}

#[cfg(test)]
mod tests;
