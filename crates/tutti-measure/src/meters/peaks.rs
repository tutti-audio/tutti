//! Waveform peak summaries — the min/max/RMS blocks a timeline draws.
//!
//! The carry is the partial block, held explicitly, so a block spanning a
//! chunk boundary is built from both chunks rather than from a remnant.
//!
//! # Per channel, not folded
//!
//! Blocking is **per channel**: a stereo input yields two series, and
//! [`PeakBlocks::channel`] hands back one of them. Folding is a caller's
//! choice ([`PeakBlocks::to_mono`]), not this module's default.
//!
//! Folding to mono before blocking would destroy the per-channel excursions
//! at the earliest possible moment, and a waveform drawn from the result
//! would misreport the audio:
//!
//! - a hard-panned double-track folds to something visibly narrower than either
//!   channel;
//! - a **phase-inverted** pair folds to a flat line — the clip looks like
//!   silence and is not.
//!
//! [`PeakConfig`] carries a [`ChannelLayout`], [`StreamMeter::push`] refuses a
//! mis-strided chunk, and the meter carries a ragged *block* tail so a caller
//! feeding chunks of any length loses nothing.
//!
//! A meter or a loudness reading genuinely wants one number per block. Those
//! call [`PeakBlocks::to_mono`], where the reduction is visible at the call
//! site — and where its semantics can be stated, because they are *not* the
//! semantics of folding first.

use tutti_buffers::{ChannelIndex, ChannelLayout};
use tutti_buffers::{Interleaved, Mono, PlanesMut, Sample, deinterleave};
use tutti_units::{Amplitude, Samples};

use core::marker::PhantomData;

use crate::convert::for_blocks;
use crate::measure::measure_buffer;
use crate::{Measure, MeterError, StreamMeter};
use tutti_dsp::rms;

/// One block of a waveform summary, for one channel.
///
/// Array-of-structs on purpose: every consumer reads all three fields
/// together, both to draw and to serialize.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PeakBlock {
    /// Most negative sample in the block. A raw signal excursion, so it is
    /// signed and not an `Amplitude`.
    pub min: f64,
    /// Most positive sample in the block. Signed, for the same reason as
    /// [`min`](Self::min).
    pub max: f64,
    /// Root-mean-square level over the block, as an [`Amplitude`].
    pub rms: Amplitude,
}

impl PeakBlock {
    /// The larger of the two excursions — what a peak meter shows.
    #[inline]
    pub fn peak(&self) -> Amplitude {
        Amplitude::new(self.min.abs().max(self.max.abs()))
    }

    /// The block covering both inputs' samples.
    ///
    /// Used two ways, and they are the same operation: merging adjacent blocks
    /// of one channel (how a coarser mipmap tier is built) and merging the same
    /// block across channels (how [`PeakBlocks::to_mono`] reduces).
    ///
    /// `min`/`max` merge exactly — the union of two ranges is the range of the
    /// union. **`rms` does not.** A root-mean-square is not linear in its
    /// inputs, so this returns the quadratic mean, `sqrt((a² + b²) / 2)`, which
    /// is exact only when the two blocks hold the same number of samples. Every
    /// caller in this crate merges equal-length blocks — pairwise tiers and
    /// per-channel reduction both do — except a ragged final block, where the
    /// `rms` of the last merged block is approximate and the `min`/`max` are
    /// not. Nothing draws `rms`; it is there for meters, which read whole
    /// blocks.
    #[inline]
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
            rms: Amplitude::new(
                ((self.rms.get() * self.rms.get() + other.rms.get() * other.rms.get()) * 0.5)
                    .sqrt(),
            ),
        }
    }
}

/// A whole summary: `layout.count()` channels, each of the same length.
///
/// **Channel-major and one flat allocation**, not a `Vec<Vec>`. Two reasons,
/// and the second is the one that matters downstream: `channel(c)` is a
/// contiguous slice, which is exactly the layout a GPU texture upload wants —
/// one row per channel — so a renderer hands the whole buffer over without
/// re-packing it.
///
/// With the `serde` feature it is stored as its layout, its blocks per channel
/// and the flat blocks; reading one back refuses blocks that are not
/// `layout.count() * blocks_per_channel` long, and blocks per channel over no
/// channels.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "StoredPeakBlocks")
)]
pub struct PeakBlocks {
    /// Channel-major: channel `c` occupies `[c * per_channel .. (c+1) * per_channel]`.
    blocks: Vec<PeakBlock>,
    layout: ChannelLayout,
    per_channel: usize,
}

/// What a stored [`PeakBlocks`] reads back as, before its length is checked.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct StoredPeakBlocks {
    pub blocks: Vec<PeakBlock>,
    pub layout: ChannelLayout,
    pub per_channel: usize,
}

#[cfg(feature = "serde")]
impl TryFrom<StoredPeakBlocks> for PeakBlocks {
    type Error = String;

    fn try_from(s: StoredPeakBlocks) -> core::result::Result<Self, String> {
        let want = usize::from(s.layout.count()).checked_mul(s.per_channel);
        // No channels means no blocks per channel, as the empty summary has.
        let empty_with_blocks = s.layout.count() == 0 && s.per_channel != 0;
        if want != Some(s.blocks.len()) || empty_with_blocks {
            return Err(format!(
                "{} blocks stored for {} channels of {} blocks each",
                s.blocks.len(),
                s.layout.count(),
                s.per_channel
            ));
        }
        Ok(Self {
            blocks: s.blocks,
            layout: s.layout,
            per_channel: s.per_channel,
        })
    }
}

/// The empty summary: no blocks, and **no channels**.
///
/// `EMPTY` is the width that agrees with an empty `blocks`.
impl Default for PeakBlocks {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            layout: ChannelLayout::EMPTY,
            per_channel: 0,
        }
    }
}

impl PeakBlocks {
    /// The width these blocks describe.
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Blocks in **one** channel — the length of every [`channel`](Self::channel)
    /// slice, not the length of the backing buffer.
    #[inline]
    pub fn blocks_per_channel(&self) -> usize {
        self.per_channel
    }

    /// Whether there is not a single block.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.per_channel == 0
    }

    /// One channel's blocks, or `None` past the width.
    ///
    /// Returns `Option` rather than panicking because the channel index is
    /// routinely a loop bound from a *different* layout — a renderer drawing
    /// two rows over a mono summary, say — and "there is no such channel" is an
    /// ordinary answer there, not a bug.
    #[inline]
    pub fn channel(&self, ch: usize) -> Option<&[PeakBlock]> {
        if ch >= self.layout.count() as usize {
            return None;
        }
        Some(&self.blocks[ch * self.per_channel..(ch + 1) * self.per_channel])
    }

    /// Every channel's blocks, channel-major and contiguous.
    ///
    /// The upload path: one slice, no re-packing.
    #[inline]
    #[must_use]
    pub fn as_flat(&self) -> &[PeakBlock] {
        &self.blocks
    }

    /// Reduces to one series, merging the channels block by block.
    ///
    /// **This is not the same as folding the samples first, and the difference
    /// is why this module never folds.** Folding samples then blocking gives
    /// the min/max of the *sum*, so a phase-inverted pair cancels to a flat
    /// line. Blocking then merging gives min-of-mins and max-of-maxes — the
    /// envelope of what is actually present, which cannot cancel.
    ///
    /// So this is the right reduction for a *waveform* that needs one row. It
    /// is **not** a downmix: a caller that wants the mono signal's own peaks —
    /// what the listener hears through a mono fold — folds the samples with
    /// [`Fold::fold`](tutti_buffers::Fold::fold) (a fold to one channel) and blocks that
    /// instead.
    ///
    /// `rms` merges quadratically; see [`PeakBlock::merge`].
    #[must_use]
    pub fn to_mono(&self) -> Vec<PeakBlock> {
        let channels = self.layout.count() as usize;
        if channels == 0 || self.per_channel == 0 {
            return Vec::new();
        }
        let mut out = self.blocks[..self.per_channel].to_vec();
        for ch in 1..channels {
            let plane = &self.blocks[ch * self.per_channel..(ch + 1) * self.per_channel];
            for (acc, block) in out.iter_mut().zip(plane) {
                *acc = acc.merge(*block);
            }
        }
        out
    }
}

/// One whole block of a [`PeakMeter`] stream, with where it stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelBlock {
    /// The channel the block summarizes, from 0.
    pub channel: ChannelIndex,
    /// The frame the block starts at, in [`Samples`] of the stream.
    pub frame: Samples,
    /// The block's summary.
    pub block: PeakBlock,
}

/// How input is blocked: how many samples of one channel each block
/// summarizes. The channel layout is the audio's, bound where it is measured
/// ([`summarize`] takes it from its view, [`PeakConfig::measure`] from its
/// argument), and so is no rate: blocking is in samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakConfig {
    samples_per_block: Samples,
}

impl PeakConfig {
    /// A config blocking `samples_per_block` of each channel: the horizontal
    /// resolution of a waveform display. A block of zero is refused by
    /// [`measure`](Self::measure) and [`summarize`] ([`MeterError::ZeroBlock`]).
    #[must_use]
    pub fn new(samples_per_block: impl Into<Samples>) -> Self {
        Self {
            samples_per_block: samples_per_block.into(),
        }
    }
}

impl<S: Sample> Measure<S> for PeakConfig {
    type Context = ChannelLayout;
    type Meter = PeakMeter<S>;

    /// A meter that blocks chunks `layout` wide.
    ///
    /// # Errors
    ///
    /// [`MeterError::ZeroBlock`] for a block of zero samples.
    fn measure(&self, layout: ChannelLayout) -> Result<PeakMeter<S>, MeterError> {
        if self.samples_per_block.is_zero() {
            return Err(MeterError::ZeroBlock);
        }
        let channels = layout.count() as usize;
        Ok(PeakMeter {
            block: self.samples_per_block.get(),
            layout,
            pending: vec![Vec::new(); channels],
            planes: vec![Vec::new(); channels],
            runs: vec![Vec::new(); channels],
            sample: PhantomData,
        })
    }
}

/// Summarizes one block. Stateless.
pub fn summarize_block<S: Sample>(samples: Mono<'_, S>) -> PeakBlock {
    block_of(&crate::convert::widen(samples.samples()))
}

/// [`summarize_block`] over `f64` samples.
fn block_of(samples: &[f64]) -> PeakBlock {
    if samples.is_empty() {
        return PeakBlock::default();
    }

    let (min, max) = samples
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), &s| {
            (min.min(s), max.max(s))
        });

    PeakBlock {
        min,
        max,
        rms: rms(Mono::new(samples)),
    }
}

/// A running waveform blocking: fed chunks of any length, it appends the whole
/// blocks each completes, per channel, and carries the ragged tail.
///
/// The tail is carried **per channel**, the same invariant N times. Channels
/// advance in lockstep (every whole frame contributes one sample to each), so
/// all `pending` planes are always the same length. Without the carry,
/// non-aligned chunks would drop a block at every boundary.
///
/// Not for the audio thread: the summary grows with the audio, so a push
/// allocates.
#[derive(Debug, Clone)]
pub struct PeakMeter<S: Sample> {
    block: usize,
    layout: ChannelLayout,
    /// Per channel: samples of an incomplete block, all the same length.
    pending: Vec<Vec<f64>>,
    /// Per channel: scratch for one chunk's deinterleave, reused across pushes.
    planes: Vec<Vec<f64>>,
    /// Per channel: the blocks completed so far.
    runs: Vec<Vec<PeakBlock>>,
    sample: PhantomData<fn(S)>,
}

impl<S: Sample> PeakMeter<S> {
    /// Splits one chunk of whole frames into planes and cuts each into blocks,
    /// calling `emit` with each whole block, channel by channel.
    fn step(&mut self, chunk: Interleaved<'_, f64>, emit: &mut impl FnMut(ChannelBlock)) {
        let frames = chunk.frames().get();
        for plane in &mut self.planes {
            plane.clear();
            plane.resize(frames, 0.0);
        }
        let split = PlanesMut::from_vecs(&mut self.planes).and_then(|out| deinterleave(chunk, out));
        debug_assert!(split.is_ok(), "the chunk is the meter's width: {split:?}");
        let layout = self.layout;
        for (channel, ((pending, plane), run)) in self
            .pending
            .iter_mut()
            .zip(&self.planes)
            .zip(&mut self.runs)
            .enumerate()
        {
            let done = run.len();
            cut_blocks(pending, self.block, plane, run);
            for (i, &block) in run[done..].iter().enumerate() {
                emit(ChannelBlock {
                    channel: layout
                        .index(channel)
                        .expect("a meter has a plane per channel of its layout"),
                    frame: Samples::new((done + i) * self.block),
                    block,
                });
            }
        }
    }

    /// Drops every carry and everything blocked, so the next chunk starts a
    /// fresh stream. Scratch capacity is kept.
    pub fn reset(&mut self) {
        for plane in self.pending.iter_mut().chain(&mut self.planes) {
            plane.clear();
        }
        for run in &mut self.runs {
            run.clear();
        }
    }
}

impl<S: Sample> StreamMeter for PeakMeter<S> {
    type Input<'a> = Interleaved<'a, S>;
    type Item = ChannelBlock;
    type Output = PeakBlocks;

    /// Blocks a chunk, extending `out` with each whole block it completes as a
    /// [`ChannelBlock`], channel by channel.
    ///
    /// Chunks need not align to block boundaries; the remainder is carried.
    ///
    /// # Errors
    ///
    /// [`MeterError::ChunkLayoutMismatch`] when `chunk` is not the meter's width:
    /// the carried tail is `layout`-wide, so a chunk of another width spliced
    /// onto it would realign at the wrong stride.
    fn push(
        &mut self,
        chunk: Interleaved<'_, S>,
        out: &mut impl Extend<ChannelBlock>,
    ) -> Result<(), MeterError> {
        if chunk.layout() != self.layout {
            return Err(MeterError::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        for_blocks(chunk.samples(), chunk.channels(), |block| {
            // `for_blocks` cuts whole frames, so the block is always one.
            if let Ok(whole) = Interleaved::new(block, self.layout) {
                self.step(whole, &mut |item| out.extend([item]));
            }
        });
        Ok(())
    }

    /// Emits the trailing partial block of every channel, if any, and hands
    /// back the summary, channel-major.
    ///
    /// [`summarize`] keeps a short final block, so the streaming path does too:
    /// the two agree on any input that is not a whole number of blocks.
    fn finish(mut self) -> PeakBlocks {
        for (pending, run) in self.pending.iter().zip(&mut self.runs) {
            if !pending.is_empty() {
                run.push(block_of(pending));
            }
        }
        // Channels are equal-length by construction, so the shortest is the true
        // count; `min` only keeps a hypothetical mismatch from producing a
        // jagged buffer.
        let per_channel = self.runs.iter().map(Vec::len).min().unwrap_or(0);
        let mut blocks = Vec::with_capacity(per_channel * self.runs.len());
        for run in &self.runs {
            blocks.extend_from_slice(&run[..per_channel]);
        }
        PeakBlocks {
            blocks,
            layout: self.layout,
            per_channel,
        }
    }
}

/// Cuts one channel's samples into whole blocks, completing the pending one
/// first and carrying the remainder.
fn cut_blocks(pending: &mut Vec<f64>, block: usize, samples: &[f64], out: &mut Vec<PeakBlock>) {
    let mut rest = samples;

    // Finish the block the previous call left half-built before taking whole
    // blocks out of this chunk.
    if !pending.is_empty() {
        let needed = block - pending.len();
        let take = needed.min(rest.len());
        pending.extend_from_slice(&rest[..take]);
        rest = &rest[take..];

        if pending.len() < block {
            return;
        }
        out.push(block_of(pending));
        pending.clear();
    }

    let mut chunks = rest.chunks_exact(block);
    for whole in chunks.by_ref() {
        out.push(block_of(whole));
    }
    pending.extend_from_slice(chunks.remainder());
}

/// Summarizes a whole buffer, per channel, at the buffer's own width.
///
/// Feeds one [`PeakMeter`] the buffer, so every block is summarized from the
/// same samples in the same order as when streaming.
///
/// # Errors
///
/// [`MeterError::ZeroBlock`] for a block of zero samples.
pub fn summarize<S: Sample>(
    cfg: &PeakConfig,
    buffer: Interleaved<'_, S>,
) -> Result<PeakBlocks, MeterError> {
    measure_buffer::<S, _>(cfg, buffer.layout(), buffer)
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "serde"))]
mod serde_tests;
