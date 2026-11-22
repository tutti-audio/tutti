//! Strip silence: the regions of a buffer that hold signal.
//!
//! A sample is *loud* if its magnitude reaches the threshold. A run of
//! non-loud samples at least `min_silence` long cuts the audio in two; shorter
//! runs (a sine's zero crossings, a drum's decay dipping under the line) are
//! kept inside the region. Each region is then widened by the padding and
//! regions that touch after widening are joined.
//!
//! A [`SilenceConfig`] describes the scan in decibels and seconds. The rate
//! binds where the audio does: [`strip_silence`] takes a buffer and its rate,
//! and [`Measure::measure`] makes a [`SilenceMeter`] that takes the
//! buffer in chunks. `strip_silence` feeds one meter the whole buffer, so batch
//! and streaming cannot drift.

use core::ops::Range;

use tutti_buffers::{Mono, Sample};
use tutti_units::{Amplitude, Db, SampleRate, Samples, Seconds};

use core::marker::PhantomData;

use crate::convert::for_blocks;
use crate::{Measure, MeterError, StreamMeter};
use tutti_units::ranged;

ranged! {
    /// A level in dBFS that is a finite number of decibels: a silence gate's
    /// threshold, a note floor, or an end of a velocity range. A NaN would make a
    /// config unequal to itself, and an infinity is no level.
    FiniteLevel(Db), FiniteLevelError, "a finite level",
    |raw| raw.get().is_finite()
}

/// A half-open span of samples, `start..end`, with `start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Region {
    start: Samples,
    end: Samples,
}

impl Region {
    /// The span `start..end`, or `None` when `end` is before `start`.
    #[must_use]
    pub fn new(start: Samples, end: Samples) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }

    /// The first sample in the region, in [`Samples`].
    #[inline]
    pub fn start(&self) -> Samples {
        self.start
    }

    /// One past the last sample in the region, in [`Samples`].
    #[inline]
    pub fn end(&self) -> Samples {
        self.end
    }

    /// How many samples the region spans.
    #[inline]
    pub fn len(&self) -> Samples {
        Samples::new(self.end.get() - self.start.get())
    }

    /// Whether the region spans nothing.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The region as a slice index.
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.start.get()..self.end.get()
    }
}

/// What a strip-silence scan reads: a threshold, the shortest silence that
/// cuts and the padding kept around a region. Immutable once built, and in
/// seconds, so it holds no sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceConfig {
    threshold: Db,
    min_silence: Seconds,
    pre_pad: Seconds,
    post_pad: Seconds,
}

impl SilenceConfig {
    /// A threshold and the shortest run below it that cuts the audio, with no
    /// padding.
    ///
    /// A `min_silence` under one sample cuts at every sample below the
    /// threshold.
    #[must_use]
    pub fn new(threshold: FiniteLevel, min_silence: Seconds) -> Self {
        Self {
            threshold: threshold.get(),
            min_silence,
            pre_pad: Seconds::new(0.0),
            post_pad: Seconds::new(0.0),
        }
    }

    /// Audio kept before the first loud sample of a region, so an attack is not
    /// clipped.
    #[must_use]
    pub fn pre_padding(mut self, pre: Seconds) -> Self {
        self.pre_pad = pre;
        self
    }

    /// Audio kept after the last loud sample of a region, so a tail is not
    /// clipped.
    #[must_use]
    pub fn post_padding(mut self, post: Seconds) -> Self {
        self.post_pad = post;
        self
    }

    /// The meter at `rate`: the infallible core of [`Measure::measure`], which
    /// `strip_silence` calls so its own result needs no `Result`.
    fn bind<S: Sample>(&self, rate: SampleRate) -> SilenceMeter<S> {
        SilenceMeter {
            floor: self.threshold.to_amplitude(),
            min_silence: self.min_silence.to_samples(rate).max(Samples::new(1)),
            pre_pad: self.pre_pad.to_samples(rate),
            post_pad: self.post_pad.to_samples(rate),
            seen: Samples::ZERO,
            open: None,
            closed: Vec::new(),
            sample: PhantomData,
        }
    }
}

impl<S: Sample> Measure<S> for SilenceConfig {
    type Context = SampleRate;
    type Meter = SilenceMeter<S>;

    /// A meter that scans audio at `rate`, chunk by chunk. Never fails.
    fn measure(&self, rate: SampleRate) -> Result<SilenceMeter<S>, MeterError> {
        Ok(self.bind(rate))
    }
}

/// A running strip-silence scan at one rate: fed chunks of the buffer in
/// order, it reads the regions at [`finish`](StreamMeter::finish). A push yields no
/// item: the regions are the output.
///
/// Not for the audio thread: it keeps one entry per region found, so it grows
/// with the audio and allocates. Feed it from the offline or UI side.
#[derive(Debug, Clone)]
pub struct SilenceMeter<S: Sample> {
    /// The threshold as a linear magnitude.
    floor: Amplitude,
    min_silence: Samples,
    pre_pad: Samples,
    post_pad: Samples,
    /// Frames consumed so far.
    seen: Samples,
    /// The region being grown: first and last loud sample.
    open: Option<(usize, usize)>,
    /// Regions already cut off, as `(first loud, one past last loud)`.
    closed: Vec<(usize, usize)>,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> SilenceMeter<S> {
    fn scan(&mut self, samples: &[f64]) {
        for (offset, &sample) in samples.iter().enumerate() {
            // Below the floor, or NaN, is silence.
            if sample.abs() < self.floor.get() || sample.is_nan() {
                continue;
            }
            let at = self.seen.get() + offset;
            self.open = match self.open {
                None => Some((at, at)),
                // The gap is `at - last - 1` samples: split once it reaches the minimum.
                Some((first, last)) if at - last > self.min_silence.get() => {
                    self.closed.push((first, last + 1));
                    Some((at, at))
                }
                Some((first, _)) => Some((first, at)),
            };
        }
        self.seen += Samples::new(samples.len());
    }

    /// Forgets everything fed. Equivalent to a fresh meter at the same rate.
    pub fn reset(&mut self) {
        self.seen = Samples::ZERO;
        self.open = None;
        self.closed.clear();
    }
}

impl<S: Sample> StreamMeter for SilenceMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = Vec<Region>;

    fn push(&mut self, chunk: Mono<'_, S>, _: &mut impl Extend<()>) -> Result<(), MeterError> {
        for_blocks(chunk.samples(), 1, |block| self.scan(block));
        Ok(())
    }

    /// The regions of the audio fed, padded, clamped to the buffer and joined
    /// where they touch. Ascending and disjoint.
    fn finish(self) -> Vec<Region> {
        let open = self.open.map(|(first, last)| (first, last + 1));
        let mut regions: Vec<Region> = Vec::new();
        for &(first, end) in self.closed.iter().chain(open.iter()) {
            let start = Samples::new(first).remaining_after(self.pre_pad);
            let end = (Samples::new(end) + self.post_pad).min(self.seen);
            match regions.last_mut() {
                Some(prev) if start <= prev.end => prev.end = prev.end.max(end),
                _ => regions.push(Region { start, end }),
            }
        }
        regions
    }
}

/// The signal regions of a whole buffer at `rate`: a [`SilenceMeter`] fed the
/// buffer, then finished.
///
/// Mono. Fold a multichannel buffer first (`Fold::fold`), or take the
/// union of per-channel results.
pub fn strip_silence<S: Sample>(
    cfg: &SilenceConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Vec<Region> {
    let mut meter = cfg.bind::<S>(rate.into());
    for_blocks(samples.samples(), 1, |block| meter.scan(block));
    meter.finish()
}

#[cfg(test)]
mod tests;
