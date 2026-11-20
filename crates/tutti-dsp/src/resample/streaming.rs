//! Sample-rate conversion as a streaming stage.
//!
//! [`StreamingResampler`] wraps the block converter, [`FftResampler`]
//! (an FFT overlap-add one on [`RealFft`](crate::RealFft), so its bits do not
//! depend on the CPU), in the bookkeeping a stream needs, so it never needs the
//! whole signal.
//!
//! # The delay
//!
//! An FFT resampler has latency: `output_delay()` FRAMES of its output are the
//! filter warming up, not signal. Ignoring that figure shifts every resampled
//! stream late by exactly it (320 frames at 44.1->48 k, ~6.7 ms of leading
//! silence) and loses the same amount off the tail, because a blind `truncate()`
//! to the expected length cuts where the delayed content would have been.
//! [`StreamingResampler`] drops those frames at the head and flushes with
//! silence at the end to push the real tail out.

use super::{FftResampler, ResampleError};
use crate::FftFloat;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{SampleRate, Samples};

/// Input frames per FFT chunk, and how finely each chunk is subdivided.
///
/// Named for the quantity, not for a ranking. A `Fast | Medium | High | Best`
/// enum names which end of a scale a caller is on and nothing about what changes
/// between two of them, so the trade cannot be reasoned about — longer chunks
/// are a steeper anti-alias filter and more latency; more sub-chunks is finer
/// time resolution and more work — and a point the enum does not list cannot be
/// expressed at all.
///
/// Constants named for their numbers, `MIN`/`MAX` bounds, and a fallible
/// constructor for anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSize {
    chunk: Samples,
    sub_chunks: usize,
}

impl ChunkSize {
    /// 512 frames, undivided.
    pub const N512: Self = Self {
        chunk: Samples::new(512),
        sub_chunks: 1,
    };
    /// 1024 frames in 2 — the default.
    pub const N1024: Self = Self {
        chunk: Samples::new(1024),
        sub_chunks: 2,
    };
    /// 2048 frames in 4.
    pub const N2048: Self = Self {
        chunk: Samples::new(2048),
        sub_chunks: 4,
    };
    /// 4096 frames in 8.
    pub const N4096: Self = Self {
        chunk: Samples::new(4096),
        sub_chunks: 8,
    };

    /// Shortest chunk that still admits a subdivision.
    pub const MIN: Self = Self {
        chunk: Samples::new(64),
        sub_chunks: 1,
    };
    /// Longest chunk before the filter's latency dominates a short render.
    pub const MAX: Self = Self {
        chunk: Samples::new(32768),
        sub_chunks: 32,
    };

    /// Every preset, for tests and for enumerating a UI.
    pub const PRESETS: [Self; 4] = [Self::N512, Self::N1024, Self::N2048, Self::N4096];

    /// A chunk length and subdivision, or `None` unless `chunk` is a power of
    /// two in [`MIN`](Self::MIN)..=[`MAX`](Self::MAX) and `sub_chunks` divides
    /// it, within `1..=MAX.sub_chunks()`.
    ///
    /// Fallible rather than clamping: rounding a chunk length hands back a
    /// resampler whose latency is not the one that was asked for. The chunk
    /// sets the input frames per call and, with the sub-chunk count, how many
    /// of the rate pair's repeating units one FFT block holds; the block
    /// sizes are those units, not powers of two.
    #[must_use]
    pub const fn new(chunk: Samples, sub_chunks: usize) -> Option<Self> {
        if !chunk.get().is_power_of_two()
            || chunk.get() < Self::MIN.chunk.get()
            || chunk.get() > Self::MAX.chunk.get()
            || sub_chunks == 0
            || sub_chunks > Self::MAX.sub_chunks
            || !chunk.get().is_multiple_of(sub_chunks)
        {
            return None;
        }
        Some(Self { chunk, sub_chunks })
    }

    /// Input frames per FFT chunk.
    #[must_use]
    pub const fn chunk(self) -> Samples {
        self.chunk
    }

    /// Sub-chunks each chunk is split into.
    #[must_use]
    pub const fn sub_chunks(self) -> usize {
        self.sub_chunks
    }
}

impl Default for ChunkSize {
    fn default() -> Self {
        Self::N1024
    }
}

/// A rate as the converter's integer Hz, or an error if it is under 0.5 Hz
/// (which rounds to 0) or at 2^32 Hz and over. A bare `as usize` would
/// saturate a huge rate and turn a tiny one into 0.
fn whole_hz(hz: f64) -> Result<usize, ResampleError> {
    if (0.5..4_294_967_296.0).contains(&hz) {
        Ok(hz.round() as usize)
    } else {
        Err(ResampleError::InvalidRate { hz })
    }
}

/// The frames `input` becomes at `ratio`, the one rounding the budget and
/// [`resampled_frames`] share.
fn scaled(input: usize, ratio: f64) -> usize {
    (input as f64 * ratio).round() as usize
}

/// Whether a stream from `source_rate` to `target_rate` needs a converter
/// at all: `false` when both are the same whole rate in Hz, which is the
/// rate the converter and a codec's header speak, so two rates that write
/// the same header are one rate. A float comparison would turn on an ULP.
/// A rate [`StreamingResampler::try_from`] refuses is not an identity either.
#[must_use]
pub fn needs_resampling(source_rate: SampleRate, target_rate: SampleRate) -> bool {
    match (whole_hz(source_rate.get()), whole_hz(target_rate.get())) {
        (Ok(source), Ok(target)) => source != target,
        _ => true,
    }
}

/// The frames a stream of `input` frames comes out as, flush included:
/// `input` itself when [`needs_resampling`] is `false`, else
/// `round(input * target / source)`, which is what a stream fed `input`
/// frames emits in all however it is blocked.
#[must_use]
pub fn resampled_frames(
    source_rate: SampleRate,
    target_rate: SampleRate,
    input: Samples,
) -> Samples {
    if !needs_resampling(source_rate, target_rate) {
        return input;
    }
    Samples::new(scaled(input.get(), target_rate.get() / source_rate.get()))
}

/// The most input frames one block of the converter can span, which bounds how
/// much silence a flush may need past the filter's delay (`tutti_dsp`'s
/// `MAX_BLOCK`, 2^20, is the largest block either side of an FFT may have).
const MAX_FLUSH_BLOCK: usize = 1 << 20;

/// A rate converter fed one block at a time.
///
/// Owns its own input carry, so callers hand it whatever block size they
/// have and it emits output whenever the converter has a chunk ready.
#[derive(Debug)]
pub struct StreamingResampler<F: FftFloat = f32> {
    inner: FftResampler<F>,
    ratio: f64,
    /// Input frames per channel, consumed up to `head`.
    carry: Vec<Vec<F>>,
    /// Frames at the front of every `carry` plane already handed to the
    /// converter. A chunk advances it instead of shifting the plane down,
    /// so a push of the whole signal (the two-pass path) costs O(N) rather
    /// than one memmove of the rest per chunk; `drain` drops the consumed
    /// prefix once, on its way out.
    head: usize,
    /// Output frames still to be discarded as filter warm-up.
    skip: usize,
    /// Real input frames fed so far (excludes the flush padding), which is
    /// what sets how much output is signal rather than filter tail.
    fed: usize,
    /// Output frames emitted so far, so the budget is a running total.
    emitted: usize,
    /// Reusable scratch so the per-block path does not allocate.
    in_scratch: Vec<Vec<F>>,
    /// The converter's output, `output_frames_max()` per channel, written
    /// by `process`.
    out_scratch: Vec<Vec<F>>,
}

/// What a [`StreamingResampler`] is made from: the width of the signal, the rate it comes at
/// and the rate it goes to, and the chunking of its FFT blocks.
///
/// `StreamingResampler::try_from(config)` depends on the two rates together: each is narrowed
/// once to the converter's whole Hz (at least 0.5 Hz and under 2^32 Hz), and the pair's
/// repeating unit must fit an FFT block of the supported size.
///
/// # Errors
///
/// [`ResampleError`] for a rate that is not finite, at least 0.5 Hz and under 2^32 Hz, or for a
/// rate pair whose block exceeds the supported size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamingResamplerConfig {
    width: ChannelLayout,
    source_rate: SampleRate,
    target_rate: SampleRate,
    chunk: ChunkSize,
}

impl StreamingResamplerConfig {
    /// A converter of a `width`-wide signal from `source_rate` to `target_rate`, in chunks of
    /// [`ChunkSize::N1024`].
    ///
    /// The two arguments are the same type, so swapping them still builds: what is gained is one
    /// derivation of the ratio, from values that never lost precision.
    #[must_use]
    pub const fn new(
        width: ChannelLayout,
        source_rate: SampleRate,
        target_rate: SampleRate,
    ) -> Self {
        Self {
            width,
            source_rate,
            target_rate,
            chunk: ChunkSize::N1024,
        }
    }

    /// The input frames per FFT chunk and how finely each is subdivided.
    #[must_use]
    pub const fn chunk(mut self, chunk: ChunkSize) -> Self {
        self.chunk = chunk;
        self
    }
}

impl<F: FftFloat> TryFrom<StreamingResamplerConfig> for StreamingResampler<F> {
    type Error = ResampleError;

    fn try_from(config: StreamingResamplerConfig) -> Result<Self, ResampleError> {
        let channels = usize::from(config.width.count());
        let (source_hz, target_hz) = (config.source_rate.get(), config.target_rate.get());
        let (source_int, target_int) = (whole_hz(source_hz)?, whole_hz(target_hz)?);
        let inner = FftResampler::new(
            source_int,
            target_int,
            config.chunk.chunk().get(),
            config.chunk.sub_chunks(),
            channels,
        )?;
        let skip = inner.output_delay();
        let out_scratch = vec![vec![F::ZERO; inner.output_frames_max()]; channels];
        Ok(Self {
            inner,
            ratio: target_hz / source_hz,
            carry: vec![Vec::new(); channels],
            head: 0,
            skip,
            fed: 0,
            emitted: 0,
            in_scratch: vec![Vec::new(); channels],
            out_scratch,
        })
    }
}

impl<F: FftFloat> StreamingResampler<F> {
    /// Feed one block of planar input; append whatever output it completes.
    ///
    /// # Errors
    /// [`ResampleError::WidthMismatch`] if the view is not as wide as the resampler (nothing is
    /// consumed then), else [`ResampleError`] if the converter refuses a chunk or the carry
    /// stops shrinking.
    pub fn push(&mut self, planes: Planes<'_, F>, out: &mut [Vec<F>]) -> Result<(), ResampleError> {
        let n = planes.frames().get();
        if planes.channels() != self.carry.len() {
            return Err(ResampleError::WidthMismatch {
                expected: self.carry.len(),
                got: planes.channels(),
            });
        }
        for (carry, p) in self.carry.iter_mut().zip(planes) {
            carry.extend_from_slice(p.samples());
        }
        self.fed += n;
        self.drain(out, false)
    }

    /// Flush the filter's delayed tail out.
    ///
    /// Feeding silence pushes the last `output_delay()` frames of real
    /// signal — still inside the converter — into `out`. The budget in
    /// `drain` then stops the padding itself from being
    /// emitted, so the total lands at `input * ratio`.
    ///
    /// Both halves are needed. Capping without flushing is what the previous
    /// version did, and it cut exactly where the real tail would have been.
    ///
    /// How much silence that takes is not one figure to compute. The delay
    /// is in OUTPUT frames and the silence is INPUT frames (they differ by
    /// the ratio, which is why a fixed pad of `output_delay()` frames fell
    /// short downsampling), and the converter only emits whole blocks of
    /// its own, which can be longer than a chunk. So the silence goes in a
    /// chunk at a time until the budget is met, bounded by the most a
    /// converter can owe: the delay in input frames plus one block
    /// (`MAX_FLUSH_BLOCK`-sized at the very most).
    ///
    /// # Errors
    /// [`ResampleError::Stalled`] if the silence cannot push the tail out, or
    /// the converter's own error.
    pub fn finish(&mut self, out: &mut [Vec<F>]) -> Result<(), ResampleError> {
        // The real input's last partial chunk, padded by `drain`.
        self.drain(out, true)?;
        let need = self.inner.input_frames_next();
        let delay_in = (self.inner.output_delay() as f64 / self.ratio).ceil() as usize;
        let bound = (delay_in + MAX_FLUSH_BLOCK).div_ceil(need.max(1)) + 2;
        let mut chunks = 0usize;
        while self.budget() > 0 {
            chunks += 1;
            if chunks > bound {
                return Err(ResampleError::Stalled { chunks: bound });
            }
            for c in self.carry.iter_mut() {
                c.resize(c.len() + need, F::ZERO);
            }
            self.drain(out, true)?;
        }
        Ok(())
    }

    /// Frames of real output still owed, given everything fed so far.
    ///
    /// The budget is a running total rather than a truncate at the end,
    /// because output is emitted block by block: a caller has already
    /// written earlier frames by the time `finish` runs, so there is nothing
    /// left to trim.
    fn budget(&self) -> usize {
        scaled(self.fed, self.ratio).saturating_sub(self.emitted)
    }

    /// `(unconsumed carry frames, head, input frames the next chunk
    /// needs)`, for the test that pins the carry's bound.
    #[cfg(test)]
    pub fn backlog(&self) -> (usize, usize, usize) {
        let len = self.carry.first().map_or(0, |c| c.len());
        (len - self.head, self.head, self.inner.input_frames_next())
    }

    fn drain(&mut self, out: &mut [Vec<F>], flushing: bool) -> Result<(), ResampleError> {
        let result = self.drain_chunks(out, flushing);
        // Drop the consumed prefix once per call, not once per chunk.
        for c in self.carry.iter_mut() {
            c.drain(..self.head);
        }
        self.head = 0;
        result
    }

    fn drain_chunks(&mut self, out: &mut [Vec<F>], flushing: bool) -> Result<(), ResampleError> {
        // Every pass consumes one chunk, so the carry this call starts
        // with runs out within `ceil(have / need)` passes (the last one
        // padded when flushing). The `+ 1` is slack; past it the carry is
        // not shrinking, and a bug that stops it shrinking fails here
        // instead of spinning forever.
        let max_passes = {
            let have = self.carry.first().map_or(0, |c| c.len()) - self.head;
            have.div_ceil(self.inner.input_frames_next().max(1)) + 1
        };
        let mut passes = 0usize;
        loop {
            let need = self.inner.input_frames_next();
            let have = self.carry.first().map_or(0, |c| c.len()) - self.head;
            if have < need {
                if !flushing {
                    return Ok(());
                }
                // Pad the last partial chunk so the converter can emit it.
                if have == 0 {
                    return Ok(());
                }
                for c in self.carry.iter_mut() {
                    c.resize(self.head + need, F::ZERO);
                }
            }

            passes += 1;
            if passes > max_passes {
                debug_assert!(false, "the resample carry stopped shrinking");
                return Err(ResampleError::Stalled { chunks: max_passes });
            }

            let chunk = self.head..self.head + need;
            for (c, scratch) in self.in_scratch.iter_mut().enumerate() {
                scratch.clear();
                scratch.extend_from_slice(&self.carry[c][chunk.clone()]);
            }
            // Each scratch plane is `need` frames in and `output_frames_max()` out, and `new`
            // checked the width against `ChannelLayout`, so both views build.
            let produced_len = self.inner.process(
                Planes::from_vecs(&self.in_scratch).expect("BUG: equal planes of a checked width"),
                PlanesMut::from_vecs(&mut self.out_scratch)
                    .expect("BUG: equal planes of a checked width"),
            )?;
            let produced = &self.out_scratch;
            self.head += need;

            // Drop warm-up frames at the head before anything is kept.
            let drop = self.skip.min(produced_len);
            self.skip -= drop;
            // Emit at most what is still owed, so the flush padding that
            // pushed the tail out does not itself become output.
            let take = (produced_len - drop).min(self.budget());
            for (c, p) in produced.iter().enumerate().take(out.len()) {
                out[c].extend_from_slice(&p[drop..drop + take]);
            }
            self.emitted += take;

            if flushing && self.carry.first().is_none_or(|c| c.len() == self.head) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
