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
//! [`PeakConfig`] carries a [`ChannelLayout`], [`PeakMeter::push`] refuses a
//! mis-strided chunk, and the meter carries a ragged *block* tail so a caller
//! feeding chunks of any length loses nothing.
//!
//! A meter or a loudness reading genuinely wants one number per block. Those
//! call [`PeakBlocks::to_mono`], where the reduction is visible at the call
//! site — and where its semantics can be stated, because they are *not* the
//! semantics of folding first.

use tutti_buffers::{Interleaved, Mono, PlanesMut, Sample, deinterleave};
use tutti_types::{Amplitude, ChannelLayout, Samples};

use crate::convert::for_blocks;
use crate::{Error, Result};

/// One block of a waveform summary, for one channel.
///
/// Array-of-structs on purpose: every consumer reads all three fields
/// together, both to draw and to serialize.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PeakBlock {
    /// Most negative sample in the block. A raw signal excursion, so it is
    /// signed and not an `Amplitude`.
    pub min: f32,
    /// Most positive sample in the block. Signed, for the same reason as
    /// [`min`](Self::min).
    pub max: f32,
    /// Root-mean-square level over the block, as an [`Amplitude`].
    pub rms: Amplitude,
}

impl PeakBlock {
    /// The larger of the two excursions — what a peak meter shows.
    #[inline]
    pub fn peak(&self) -> Amplitude {
        Amplitude(self.min.abs().max(self.max.abs()))
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
            rms: Amplitude(((self.rms.0 * self.rms.0 + other.rms.0 * other.rms.0) * 0.5).sqrt()),
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
    /// [`measure`](Self::measure) and [`summarize`] ([`Error::ZeroBlock`]).
    #[must_use]
    pub fn new(samples_per_block: impl Into<Samples>) -> Self {
        Self {
            samples_per_block: samples_per_block.into(),
        }
    }

    /// A meter that blocks chunks `layout` wide.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroBlock`] for a block of zero samples.
    pub fn measure(&self, layout: ChannelLayout) -> Result<PeakMeter> {
        if self.samples_per_block.is_zero() {
            return Err(Error::ZeroBlock);
        }
        let channels = layout.count() as usize;
        Ok(PeakMeter {
            block: self.samples_per_block.get(),
            layout,
            pending: vec![Vec::new(); channels],
            planes: vec![Vec::new(); channels],
            runs: vec![Vec::new(); channels],
        })
    }
}

/// Summarizes one block. Stateless.
pub fn summarize_block<S: Sample>(samples: Mono<'_, S>) -> PeakBlock {
    block_of(&crate::convert::to_f32(samples.samples()))
}

/// [`summarize_block`] over `f32` samples.
fn block_of(samples: &[f32]) -> PeakBlock {
    if samples.is_empty() {
        return PeakBlock::default();
    }

    let (min, max, sum_sq) = samples.iter().fold(
        (f32::INFINITY, f32::NEG_INFINITY, 0.0f32),
        |(min, max, sum), &s| (min.min(s), max.max(s), sum + s * s),
    );

    PeakBlock {
        min,
        max,
        rms: Amplitude((sum_sq / samples.len() as f32).sqrt()),
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
pub struct PeakMeter {
    block: usize,
    layout: ChannelLayout,
    /// Per channel: samples of an incomplete block, all the same length.
    pending: Vec<Vec<f32>>,
    /// Per channel: scratch for one chunk's deinterleave, reused across pushes.
    planes: Vec<Vec<f32>>,
    /// Per channel: the blocks completed so far.
    runs: Vec<Vec<PeakBlock>>,
}

impl PeakMeter {
    /// Blocks a chunk, appending whatever whole blocks it completes.
    ///
    /// Chunks need not align to block boundaries; the remainder is carried.
    ///
    /// # Errors
    ///
    /// [`Error::ChunkLayoutMismatch`] when `chunk` is not the meter's width:
    /// the carried tail is `layout`-wide, so a chunk of another width spliced
    /// onto it would realign at the wrong stride.
    pub fn push<S: Sample>(&mut self, chunk: Interleaved<'_, S>) -> Result<()> {
        if chunk.layout() != self.layout {
            return Err(Error::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        for_blocks(chunk.samples(), chunk.channels(), |block| {
            // `for_blocks` cuts whole frames, so the block is always one.
            if let Ok(whole) = Interleaved::new(block, self.layout) {
                self.step(whole);
            }
        });
        Ok(())
    }

    /// Splits one chunk of whole frames into planes and cuts each into blocks.
    fn step(&mut self, chunk: Interleaved<'_>) {
        let frames = chunk.frames().get();
        for plane in &mut self.planes {
            plane.clear();
            plane.resize(frames, 0.0);
        }
        let split = PlanesMut::from_vecs(&mut self.planes).and_then(|out| deinterleave(chunk, out));
        debug_assert!(split.is_ok(), "the chunk is the meter's width: {split:?}");
        for ((pending, plane), run) in self
            .pending
            .iter_mut()
            .zip(&self.planes)
            .zip(&mut self.runs)
        {
            cut_blocks(pending, self.block, plane, run);
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

    /// Emits the trailing partial block of every channel, if any, and hands
    /// back the summary, channel-major.
    ///
    /// [`summarize`] keeps a short final block, so the streaming path does too:
    /// the two agree on any input that is not a whole number of blocks.
    #[must_use]
    pub fn finish(mut self) -> PeakBlocks {
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
fn cut_blocks(pending: &mut Vec<f32>, block: usize, samples: &[f32], out: &mut Vec<PeakBlock>) {
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
/// [`Error::ZeroBlock`] for a block of zero samples.
pub fn summarize<S: Sample>(cfg: &PeakConfig, buffer: Interleaved<'_, S>) -> Result<PeakBlocks> {
    let mut meter = cfg.measure(buffer.layout())?;
    meter.push(buffer)?;
    Ok(meter.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `summarize` of a buffer that is valid for its config.
    fn summarize(cfg: &PeakConfig, buffer: Interleaved<'_>) -> PeakBlocks {
        super::summarize(cfg, buffer).unwrap()
    }

    fn mono(block: usize) -> PeakConfig {
        PeakConfig::new(Samples(block))
    }

    /// Channel `ch`'s blocks, for tests that know the channel exists.
    fn ch(blocks: &PeakBlocks, ch: usize) -> &[PeakBlock] {
        blocks.channel(ch).expect("channel is within the layout")
    }

    /// Stream `samples` in `chunk`-frame pieces at `layout`. The streaming
    /// path, wrapped so a test states only what it varies.
    fn stream(
        cfg: &PeakConfig,
        layout: ChannelLayout,
        samples: &[f32],
        chunk: usize,
    ) -> PeakBlocks {
        let mut meter = cfg.measure(layout).unwrap();
        for part in samples.chunks(chunk * usize::from(layout.count())) {
            meter.push(Interleaved::new(part, layout).unwrap()).unwrap();
        }
        meter.finish()
    }

    #[test]
    fn block_values_are_exact_for_a_ramp() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let blocks = summarize(
            &mono(100),
            Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
        );

        assert_eq!(blocks.blocks_per_channel(), 5);
        for (i, block) in ch(&blocks, 0).iter().enumerate() {
            let lo = (i * 100) as f32;
            assert_eq!(block.min, lo);
            assert_eq!(block.max, lo + 99.0);
            assert_eq!(block.peak(), Amplitude(lo + 99.0));
        }
    }

    /// The law: streaming in arbitrary chunks equals one batch call.
    ///
    /// Swept across **every layout**, not just mono: at `ChannelLayout::MONO`
    /// a frame is one sample, so a bug that discards audio on non-frame-aligned
    /// multichannel chunks would go unseen.
    ///
    /// Chunk sizes are mostly coprime with both the block size and the channel
    /// counts, so nearly every chunk ends mid-frame *and* mid-block.
    #[test]
    fn streaming_matches_batch_at_every_chunk_size_and_layout() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::QUAD,
            ChannelLayout::from(6u16),
        ] {
            let channels = layout.count() as usize;
            // 500 whole frames, so batch and streaming see the same input.
            let samples: Vec<f32> = (0..500 * channels).map(|i| i as f32).collect();

            for block in [1usize, 3, 100] {
                let cfg = PeakConfig::new(Samples(block));
                let batch = summarize(&cfg, Interleaved::new(&samples, layout).unwrap());

                for chunk in [1usize, 3, 7, 33, 100, 250, 333, 501, 1024] {
                    let streamed = stream(&cfg, layout, &samples, chunk);

                    assert_eq!(
                        streamed, batch,
                        "{layout:?}, block {block}, chunk {chunk} disagrees with batch"
                    );
                }
            }
        }
    }

    /// A chunk whose own width disagrees with the config's is refused: the
    /// pending blocks are `cfg.layout` wide.
    #[test]
    fn a_chunk_of_the_wrong_width_is_refused() {
        let cfg = PeakConfig::new(Samples(1));
        let mut meter = cfg.measure(ChannelLayout::STEREO).unwrap();

        assert_eq!(
            meter.push(Interleaved::new(&[1.0f32, 2.0, 3.0, 4.0], ChannelLayout::QUAD).unwrap()),
            Err(Error::ChunkLayoutMismatch {
                chunk: ChannelLayout::QUAD,
                config: ChannelLayout::STEREO,
            })
        );
        assert!(
            meter.finish().is_empty(),
            "a quad chunk must not be blocked as stereo"
        );
    }

    #[test]
    fn a_trailing_partial_block_is_kept() {
        let samples: Vec<f32> = (0..250).map(|i| i as f32).collect();
        let blocks = summarize(
            &mono(100),
            Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
        );

        assert_eq!(blocks.blocks_per_channel(), 3);
        assert_eq!(ch(&blocks, 0)[2].min, 200.0);
        assert_eq!(ch(&blocks, 0)[2].max, 249.0, "only the 50 samples present");
    }

    /// A ragged final block must be kept for **every** channel, not just the
    /// first — the per-channel form of the trailing-block rule.
    #[test]
    fn a_trailing_partial_block_is_kept_on_every_channel() {
        // 250 stereo frames at block 100: two whole blocks and a 50-frame tail.
        let samples: Vec<f32> = (0..250).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100)),
            Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        );

        assert_eq!(blocks.blocks_per_channel(), 3);
        assert_eq!(ch(&blocks, 0)[2].max, 249.0);
        assert_eq!(ch(&blocks, 1)[2].min, -249.0);
    }

    /// **Why blocking is per channel.**
    ///
    /// A phase-inverted pair is the worst case: folding the samples before
    /// blocking cancels it to a flat line, so the clip renders as silence and
    /// is not. Per channel, both channels are a full-scale ramp.
    #[test]
    fn a_phase_inverted_pair_does_not_cancel() {
        // L ramps up, R is its exact negation.
        let samples: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100)),
            Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        );

        assert_eq!(blocks.blocks_per_channel(), 2, "frames, not samples");
        assert_eq!(blocks.layout(), ChannelLayout::STEREO);

        // Neither channel is silent, and they are opposite.
        assert_eq!(ch(&blocks, 0)[0].min, 0.0);
        assert_eq!(ch(&blocks, 0)[0].max, 99.0);
        assert_eq!(ch(&blocks, 1)[0].min, -99.0);
        assert_eq!(ch(&blocks, 1)[0].max, 0.0);

        // And the reduction keeps the envelope rather than cancelling it —
        // this is what distinguishes `to_mono` from folding first.
        let merged = blocks.to_mono();
        assert_eq!(merged[0].min, -99.0);
        assert_eq!(merged[0].max, 99.0);
    }

    /// A hard-panned pair: one channel silent, the other full-scale.
    ///
    /// Folding halved the amplitude, so the clip drew at half height. Per
    /// channel the loud side is untouched and the silent side is visibly
    /// silent — which is the information a panned double-track needs to show.
    #[test]
    fn a_hard_panned_pair_keeps_the_loud_channel_at_full_scale() {
        // L silent, R full-scale square.
        let samples: Vec<f32> = (0..200)
            .flat_map(|i| [0.0, if i % 2 == 0 { 1.0 } else { -1.0 }])
            .collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100)),
            Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        );

        assert_eq!(ch(&blocks, 0)[0].min, 0.0, "silent channel is silent");
        assert_eq!(ch(&blocks, 0)[0].max, 0.0);
        assert_eq!(ch(&blocks, 1)[0].peak(), Amplitude(1.0), "not halved");
    }

    /// Every channel is blocked, including ones the downmix matrices weight to
    /// zero — a surround waveform shows what is in each channel, not what a
    /// stereo listener would hear.
    #[test]
    fn surround_keeps_every_channel_separately() {
        // 5.1 with only the centre non-zero.
        let layout = ChannelLayout::from(6u16);
        let samples: Vec<f32> = (0..100)
            .flat_map(|_| [0.0, 0.0, 1.0, 0.0, 0.0, 0.0])
            .collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(50)),
            Interleaved::new(&samples, layout).unwrap(),
        );

        assert_eq!(blocks.blocks_per_channel(), 2);
        assert_eq!(blocks.layout(), layout);
        assert_eq!(ch(&blocks, 2)[0].max, 1.0, "centre, at full scale");
        for silent in [0, 1, 3, 4, 5] {
            assert_eq!(
                ch(&blocks, silent)[0].max,
                0.0,
                "channel {silent} must stay silent, not borrow the centre"
            );
        }
        assert!(blocks.channel(6).is_none(), "no seventh channel");
    }

    /// On mono input the two semantics must coincide — the one case where
    /// `to_mono` reproduces the per-channel output exactly.
    #[test]
    fn to_mono_on_mono_input_is_the_channel_itself() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let blocks = summarize(
            &mono(100),
            Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
        );

        assert_eq!(blocks.to_mono(), ch(&blocks, 0));
    }

    /// `as_flat` is channel-major and contiguous — the property the GPU upload
    /// path depends on, so it is asserted rather than assumed.
    #[test]
    fn as_flat_is_channel_major() {
        let samples: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100)),
            Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        );

        let flat = blocks.as_flat();
        let per = blocks.blocks_per_channel();
        assert_eq!(flat.len(), per * 2);
        assert_eq!(&flat[..per], ch(&blocks, 0));
        assert_eq!(&flat[per..], ch(&blocks, 1));
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(
            summarize(
                &mono(100),
                Interleaved::new(&[], ChannelLayout::MONO).unwrap()
            )
            .is_empty()
        );
        // A block of zero samples is named, not silently empty.
        assert_eq!(
            super::summarize(
                &mono(0),
                Interleaved::new(&[1.0f32, 2.0], ChannelLayout::MONO).unwrap()
            ),
            Err(Error::ZeroBlock)
        );
        assert_eq!(
            summarize_block(Mono::new(&[] as &[f32])),
            PeakBlock::default()
        );
        let one = summarize_block(Mono::new(&[-0.5f32, 0.25]));
        assert_eq!((one.min, one.max), (-0.5, 0.25));

        // `to_mono` and `channel` on an empty summary answer rather than panic.
        let empty = summarize(
            &mono(100),
            Interleaved::new(&[], ChannelLayout::MONO).unwrap(),
        );
        assert!(empty.to_mono().is_empty());
        assert!(empty.as_flat().is_empty());

        let blocks = mono(100).measure(ChannelLayout::MONO).unwrap().finish();
        assert!(blocks.is_empty(), "finish on an empty meter emits nothing");
    }

    /// An empty summary reports **no** channels, not a guessed stereo pair;
    /// one a meter finishes with nothing fed keeps the width it measured.
    #[test]
    fn an_empty_summary_has_no_channels_until_a_width_is_measured() {
        assert_eq!(PeakBlocks::default().layout(), ChannelLayout::EMPTY);
        assert_eq!(PeakBlocks::default().channel(0), None);
        let blocks = mono(64).measure(ChannelLayout::STEREO).unwrap().finish();
        assert_eq!(blocks.layout(), ChannelLayout::STEREO);
        assert!(blocks.is_empty());
    }

    /// Every block is exact, including blocks past the first chunk a meter
    /// cuts and the short final one.
    ///
    /// A stereo ramp (left `i`, right `-i` at frame `i`) over 20 000 frames
    /// with 3 000-frame blocks: 7 blocks, the last a partial one. Block `k` of
    /// the left channel spans exactly `[3000k, min(3000k + 2999, 19999)]`, the
    /// right its negation.
    #[test]
    fn summarize_is_exact_across_its_chunks() {
        let frames = 20_000;
        let samples: Vec<f32> = (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let cfg = PeakConfig::new(Samples(3_000));
        let blocks = summarize(
            &cfg,
            Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
        );
        assert_eq!(blocks.blocks_per_channel(), 7);
        for (k, (l, r)) in ch(&blocks, 0).iter().zip(ch(&blocks, 1)).enumerate() {
            let lo = (k * 3_000) as f32;
            let hi = ((k * 3_000 + 2_999).min(frames - 1)) as f32;
            assert_eq!((l.min, l.max), (lo, hi), "left block {k}");
            assert_eq!((r.min, r.max), (-hi, -lo), "right block {k}");
        }
    }

    /// An empty buffer still has the view's width.
    #[test]
    fn summarizing_nothing_keeps_the_layout() {
        let cfg = PeakConfig::new(Samples(64));
        let blocks = summarize(&cfg, Interleaved::new(&[], ChannelLayout::STEREO).unwrap());
        assert_eq!(blocks.layout(), ChannelLayout::STEREO);
        assert!(blocks.is_empty());
    }

    #[test]
    fn reset_returns_to_a_fresh_meter() {
        let mut meter = mono(100).measure(ChannelLayout::MONO).unwrap();
        meter
            .push(Interleaved::new(&[1.0f32; 150], ChannelLayout::MONO).unwrap())
            .unwrap();
        meter.reset();
        meter
            .push(Interleaved::new(&[2.0f32; 100], ChannelLayout::MONO).unwrap())
            .unwrap();
        let blocks = meter.finish();

        assert_eq!(
            blocks.blocks_per_channel(),
            1,
            "the carried 50 samples were discarded"
        );
        assert_eq!(ch(&blocks, 0)[0].min, 2.0);
    }

    /// `reset` must clear every channel's carry, not just the first: a stereo
    /// stream reset mid-block would otherwise splice the stale right channel
    /// onto the new one.
    #[test]
    fn reset_clears_every_channel_carry() {
        let mut meter = PeakConfig::new(Samples(100))
            .measure(ChannelLayout::STEREO)
            .unwrap();

        // 50 frames: half a block on both channels, nothing emitted.
        let partial: Vec<f32> = (0..50).flat_map(|_| [1.0f32, -1.0]).collect();
        meter
            .push(Interleaved::new(&partial, ChannelLayout::STEREO).unwrap())
            .unwrap();
        meter.reset();

        // A full block of a different value. If either carry survived, its
        // channel would report the earlier value's excursion too.
        let full: Vec<f32> = (0..100).flat_map(|_| [2.0f32, -2.0]).collect();
        meter
            .push(Interleaved::new(&full, ChannelLayout::STEREO).unwrap())
            .unwrap();
        let blocks = meter.finish();

        assert_eq!(blocks.blocks_per_channel(), 1);
        assert_eq!(ch(&blocks, 0)[0].min, 2.0, "left carry survived reset");
        assert_eq!(ch(&blocks, 1)[0].max, -2.0, "right carry survived reset");
    }

    /// `merge` is the tier-building and channel-reducing primitive, so its
    /// range union is exact and its `rms` is the quadratic mean.
    #[test]
    fn merge_unions_the_range_and_takes_the_quadratic_mean() {
        let a = PeakBlock {
            min: -1.0,
            max: 0.5,
            rms: Amplitude(0.0),
        };
        let b = PeakBlock {
            min: -0.25,
            max: 2.0,
            rms: Amplitude(1.0),
        };
        let m = a.merge(b);

        assert_eq!(m.min, -1.0, "the lower of the two mins");
        assert_eq!(m.max, 2.0, "the upper of the two maxes");
        // sqrt((0² + 1²) / 2), not (0 + 1) / 2.
        assert!((m.rms.0 - 0.5f32.sqrt()).abs() < 1e-6, "got {}", m.rms.0);
    }
}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use tutti_types::{Amplitude, ChannelLayout};

    use super::{PeakBlock, PeakBlocks};

    fn block(v: f32) -> PeakBlock {
        PeakBlock {
            min: -v,
            max: v,
            rms: Amplitude(v / 2.0),
        }
    }

    /// A stereo summary of three blocks per channel reads back equal, each channel in its
    /// place.
    #[test]
    fn peak_blocks_round_trip_through_serde() {
        let blocks = PeakBlocks {
            blocks: (1..=6).map(|i| block(i as f32 / 8.0)).collect(),
            layout: ChannelLayout::STEREO,
            per_channel: 3,
        };
        let text = serde_json::to_string(&blocks).expect("serializes");
        let back: PeakBlocks = serde_json::from_str(&text).expect("reads back");
        assert_eq!(back, blocks);
        assert_eq!(back.channel(1).map(|c| c[0]), Some(block(0.5)));
    }

    /// Stored blocks that are not channels times blocks per channel long are refused: five
    /// blocks cannot be two channels of three.
    #[test]
    fn stored_blocks_of_the_wrong_length_are_refused() {
        let blocks = PeakBlocks {
            blocks: (1..=6).map(|i| block(i as f32 / 8.0)).collect(),
            layout: ChannelLayout::STEREO,
            per_channel: 3,
        };
        let mut value = serde_json::to_value(&blocks).expect("serializes");
        value["blocks"]
            .as_array_mut()
            .expect("blocks are a list")
            .pop();
        assert!(serde_json::from_value::<PeakBlocks>(value).is_err());
    }

    /// No channels with blocks per channel is refused (the empty summary has none of
    /// either), and too many blocks for the layout is refused as too few is.
    #[test]
    fn stored_blocks_over_no_channels_or_too_many_are_refused() {
        let over_none = r#"{"blocks":[],"layout":0,"per_channel":5}"#;
        assert!(serde_json::from_str::<PeakBlocks>(over_none).is_err());
        let empty = r#"{"blocks":[],"layout":0,"per_channel":0}"#;
        assert_eq!(
            serde_json::from_str::<PeakBlocks>(empty).expect("the empty summary"),
            PeakBlocks::default()
        );
        let blocks = PeakBlocks {
            blocks: vec![block(0.5); 2],
            layout: ChannelLayout::MONO,
            per_channel: 2,
        };
        let mut value = serde_json::to_value(&blocks).expect("serializes");
        value["blocks"]
            .as_array_mut()
            .expect("blocks are a list")
            .push(serde_json::to_value(block(0.25)).expect("a block"));
        assert!(serde_json::from_value::<PeakBlocks>(value).is_err());
    }
}
