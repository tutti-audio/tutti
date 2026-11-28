//! Planar scratch for the block read, and the kernels that run over it.
//!
//! A voice renders a block at a time into [`Lanes`] — one contiguous run of
//! frames per channel — and the owner then adds each lane into its output
//! with one [`accumulate`] per channel. The per-voice read is gather-bound
//! (every voice reads a different wave at a different fractional position),
//! so the vectors run *along time*, over the lanes, not across voices.
//!
//! The kernels are plain loops over equal-length slices, written so the
//! compiler vectorises them (no bounds checks inside the loop: both slices
//! are cut to one length first). Each is the per-element arithmetic of a
//! frame-at-a-time read, in the same order, so a block read is bit-identical
//! to reading its frames one by one — IEEE-754 `+` and `*` on one element
//! give one answer whether they run four at a time or one.
//!
//! Sized in `prepare`. A rendering node sizes its [`BlockScratch`] in
//! its `prepare`: as many lanes as the node has channels, each as long as
//! `Prepare::max_block`, so a block renders in one piece. In a graph,
//! `prepare` always precedes `process` and a block is never longer than
//! `max_block`.
//!
//! Driven by hand, unprepared. A node driven straight through
//! `Node::process` without a `prepare` (the crate's own
//! `testing::block`/`play`, `contract::drive_input`, which the sampler's unit
//! tests use throughout) is still served: it is built with
//! [`UNPREPARED_FRAMES`] of scratch, and a block longer than its scratch
//! renders in pieces that long, on a `#[cold]` path the prepared render
//! never enters.

use crate::voice::stretched::PRIME_SCRATCH;
use tutti_buffers::Float;
use tutti_buffers::MAX_CHANNELS;
use tutti_units::{Amplitude, SamplePosition};

/// Frames of scratch a node holds before any `prepare` (its constructor's
/// size, a few KiB), and the piece length of a source's unprepared stack
/// positions: what a node driven by hand without a `prepare` renders
/// through. Never the length a prepared node renders in.
pub(crate) const UNPREPARED_FRAMES: usize = 64;

/// One channel of a block, planar: as long as its scratch's frames.
pub(crate) type Lane<F> = Vec<F>;

/// A block of planar scratch: [`MAX_CHANNELS`] lanes, the first
/// `width` (the owner's channel count) as long as the longest block its
/// owner is prepared for, the rest empty.
///
/// Owned by the node that renders through it, built and resized on the
/// control thread, boxed so a node that moves does not move it all. Carries
/// nothing between blocks: every read writes each frame it hands on. A read
/// writes only lanes below its `n`, which every caller clamps to the
/// owner's width.
pub(crate) struct Lanes<F: Float>(Box<[Lane<F>; MAX_CHANNELS]>);

impl<F: Float> Lanes<F> {
    /// No frames in any lane. Allocates the box: control thread.
    fn empty() -> Self {
        Self(Box::new(std::array::from_fn(|_| Vec::new())))
    }

    /// Frames each sized lane holds.
    #[inline]
    pub(crate) fn frames(&self) -> usize {
        self.0[0].len()
    }

    /// The first `width` lanes exactly `frames` long, zeroed; the rest
    /// empty. Allocates: control thread.
    fn resize(&mut self, frames: usize, width: usize) {
        for (c, lane) in self.0.iter_mut().enumerate() {
            fit(lane, if c < width { frames } else { 0 }, F::ZERO);
        }
    }

    /// The lanes, one per channel.
    #[inline]
    pub(crate) fn lanes(&self) -> &[Lane<F>; MAX_CHANNELS] {
        &self.0
    }

    /// The lanes, one per channel, to write.
    #[inline]
    pub(crate) fn lanes_mut(&mut self) -> &mut [Lane<F>; MAX_CHANNELS] {
        &mut self.0
    }

    /// Frames `0..frames` of the first `width` lanes set to zero.
    #[inline]
    pub(crate) fn clear(&mut self, width: usize, frames: usize) {
        for lane in self.0.iter_mut().take(width) {
            lane[..frames].fill(F::ZERO);
        }
    }

    /// Write `frame` (one sample per channel) into frame `i` of the lanes.
    #[inline]
    pub(crate) fn put(&mut self, i: usize, frame: &[F]) {
        for (lane, &s) in self.0.iter_mut().zip(frame) {
            lane[i] = s;
        }
    }
}

impl<F: Float> std::fmt::Debug for Lanes<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lanes")
            .field("frames", &self.frames())
            .finish_non_exhaustive()
    }
}

/// What a voice's block read renders through: the lanes it hands on, the
/// lanes its stretch vocoder reads from, and the block's positions and
/// gather. One per rendering node (a pool reads every slot through its
/// one), sized on the control thread from the node's width and
/// `Prepare::max_block`, so a voice's block read touches no stack arrays of
/// its own and renders a whole block in one piece.
///
/// Its cost a frame: 57 bytes (two positions, 32; the gather's index, 4,
/// fraction, 4, flag, 1, and four tap lanes, 16) plus 8 a channel (an `out`
/// and a `raw` lane): 73 bytes a stereo frame, 73 KiB for a stereo voice
/// prepared at 1024 frames (`tests::a_resize_sizes_every_buffer_to_the_width`
/// counts it), and the priming scratch, a hop of frames a channel whatever the block.
#[derive(Debug)]
pub(crate) struct BlockScratch<F: Float> {
    /// The voice's output for the block, before the mix.
    pub(crate) out: Lanes<F>,
    /// The source's frames, when a vocoder sits between them and
    /// `out`.
    pub(crate) raw: Lanes<F>,
    /// Where a stretching voice's priming sends the output it discards: one hop
    /// of frames, whatever the block.
    pub(crate) prime: Lanes<F>,
    /// Where each frame of the block reads (`None`: silence). The disk tier
    /// rewrites them in place to its ring positions.
    pub(crate) positions: Vec<Option<SamplePosition>>,
    /// Where each source frame a stretching voice feeds its vocoder reads: the
    /// vocoder takes its input at a cursor of its own, apart from the
    /// per-output-frame `positions`.
    pub(crate) reads: Vec<Option<SamplePosition>>,
    /// The along-time reads' taps (`MemorySource::read_placed_lanes`, a
    /// steady live disk read).
    pub(crate) gather: Gather<F>,
}

impl<F: Float> BlockScratch<F> {
    /// Scratch for blocks of up to `frames` frames of `width` channels (see
    /// [`resize`](Self::resize)). Allocates: control thread.
    pub(crate) fn new(frames: usize, width: usize) -> Self {
        let mut s = Self {
            out: Lanes::empty(),
            raw: Lanes::empty(),
            prime: Lanes::empty(),
            positions: Vec::new(),
            reads: Vec::new(),
            gather: Gather::empty(),
        };
        s.resize(frames, width);
        s
    }

    /// Scratch sized as `other` is, for a copy of its owner. Allocates:
    /// control thread.
    pub(crate) fn sized_like(other: &Self) -> Self {
        Self::new(other.frames(), other.width())
    }

    /// The longest block a read through this scratch may be: the
    /// `max_block` it was last prepared for (0 before any `prepare`).
    #[inline]
    pub(crate) fn frames(&self) -> usize {
        self.positions.len()
    }

    /// The channels its lanes are sized for.
    pub(crate) fn width(&self) -> usize {
        self.out.0.iter().take_while(|l| !l.is_empty()).count()
    }

    /// Sized for blocks of up to `frames` frames of `width` channels
    /// (clamped to `1..=MAX_CHANNELS`), every buffer zeroed and
    /// exactly that long: what a node's `prepare` calls with
    /// `Prepare::max_block` and its own width, so a re-prepare at a larger
    /// block grows it and one at a smaller block gives the memory back.
    /// Allocates: control thread only, never `process`.
    pub(crate) fn resize(&mut self, frames: usize, width: usize) {
        let width = width.clamp(1, MAX_CHANNELS);
        self.out.resize(frames, width);
        self.raw.resize(frames, width);
        self.prime.resize(PRIME_SCRATCH.get(), width);
        fit(&mut self.positions, frames, None);
        fit(&mut self.reads, frames, None);
        self.gather.resize(frames);
    }
}

/// `v` exactly `frames` long (its capacity too), every element `fill`.
fn fit<T: Clone>(v: &mut Vec<T>, frames: usize, fill: T) {
    v.clear();
    v.resize(frames, fill);
    v.shrink_to_fit();
}

/// Each frame's whole index and fraction, and the taps gathered into four
/// lanes, for the cubic along time. Each as long as its scratch's frames,
/// and written in full for the frames a block reads before any is used.
/// The four taps of frame `i` are derived from `index[i]` per channel
/// (`interp::tap`), rather than kept: 4 bytes a frame, not 32.
#[derive(Debug)]
pub(crate) struct Gather<F: Float> {
    pub(crate) index: Vec<u32>,
    pub(crate) frac: Vec<F>,
    /// Whether frame `i` has a sample (a position inside the file).
    pub(crate) live: Vec<bool>,
    pub(crate) y: [Lane<F>; 4],
    /// One channel of a stretch of ring positions, which a block's taps
    /// are read from: [`GATHER_TAPS`] past `frames()`, so a block at a rate
    /// of at most one frame per frame is one stretch.
    pub(crate) run: Lane<F>,
}

/// The taps of the cubic along time: the frames a gathered frame reads.
pub(crate) const GATHER_TAPS: usize = 4;

impl<F: Float> Gather<F> {
    /// No frames. Allocation-free.
    pub(crate) fn empty() -> Self {
        Self {
            index: Vec::new(),
            frac: Vec::new(),
            live: Vec::new(),
            y: std::array::from_fn(|_| Vec::new()),
            run: Vec::new(),
        }
    }

    /// The frames a read through it may be (0 before it is sized).
    #[inline]
    pub(crate) fn frames(&self) -> usize {
        self.index.len()
    }

    /// Every buffer exactly `frames` long, zeroed. Allocates: control
    /// thread.
    pub(crate) fn resize(&mut self, frames: usize) {
        fit(&mut self.index, frames, 0);
        fit(&mut self.frac, frames, F::ZERO);
        fit(&mut self.live, frames, false);
        for lane in &mut self.y {
            fit(lane, frames, F::ZERO);
        }
        fit(&mut self.run, frames + GATHER_TAPS, F::ZERO);
    }
}

/// `dst[i] += src[i]` over the shorter of the two: the one mix a block read
/// ends in.
#[inline]
pub(crate) fn accumulate<F: Float>(dst: &mut [F], src: &[F]) {
    let n = dst.len().min(src.len());
    let (dst, src) = (&mut dst[..n], &src[..n]);
    for (d, &s) in dst.iter_mut().zip(src) {
        *d += s;
    }
}

/// A voice's gain cell as the sample-domain scalar its block multiplies by:
/// crossed once to `F`, and silence when it is NaN or infinite (checked in the
/// cell's own width, before the crossing saturates it). Every place a voice's
/// gain scales a sample reads it through this.
#[inline]
pub(crate) fn audible_gain<F: Float>(gain: Amplitude) -> F {
    if gain.get().is_finite() {
        F::from_unit(gain)
    } else {
        F::ZERO
    }
}

/// `lane[i] *= gain`: one voice gain over a lane.
#[inline]
pub(crate) fn scale<F: Float>(lane: &mut [F], gain: F) {
    for s in lane.iter_mut() {
        *s *= gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VoiceWindow;
    use crate::testing::placed_source;
    use tutti_buffers::Interleaved;
    use tutti_buffers::MaxBlock;

    /// [`BlockScratch`]'s bytes a frame that do not depend on the width: two
    /// positions, the gather's index, fraction, flag and four tap lanes.
    const BYTES_PER_FRAME_FIXED: usize =
        2 * size_of::<Option<SamplePosition>>() + 4 + 4 + 1 + 4 * 4;

    /// [`BlockScratch`]'s bytes a frame per channel: an `out` and a `raw`
    /// lane.
    const BYTES_PER_FRAME_CHANNEL: usize = 2 * 4;

    /// The kernels are the per-element arithmetic, exactly. Against a
    /// scalar loop over values chosen so rounding shows (thirds, added to
    /// values of like size): equal bit for bit, so a vectorised kernel can
    /// never be the reason a block read differs from the per-sample one.
    #[test]
    fn the_kernels_are_the_per_element_arithmetic() {
        // One 64-frame lane's worth (the length the lanes had when this was
        // written; any length shows the same arithmetic).
        const LANE_FRAMES: usize = 64;
        let src: Vec<f32> = (0..LANE_FRAMES).map(|i| (i as f32 + 1.0) / 3.0).collect();
        let base: Vec<f32> = (0..LANE_FRAMES).map(|i| 1.0 + i as f32 * 0.1).collect();
        let mut dst = base.clone();
        accumulate(&mut dst, &src);
        for i in 0..LANE_FRAMES {
            assert_eq!(dst[i].to_bits(), (base[i] + src[i]).to_bits(), "frame {i}");
        }
        let mut lane = src.clone();
        scale(&mut lane, 0.3);
        for i in 0..LANE_FRAMES {
            assert_eq!(lane[i].to_bits(), (src[i] * 0.3).to_bits(), "frame {i}");
        }
    }

    /// A re-prepare resizes, to the owner's width. Scratch resized to
    /// 512 frames of 2 channels holds 512 frames in every buffer a block
    /// read writes (the first two lanes of `out` and `raw`, the positions,
    /// the gather) and nothing in the lanes past its width; resized to 64
    /// frames of 6 it holds 64 in six lanes; and its bytes are the ones
    /// [`BYTES_PER_FRAME_FIXED`] and [`BYTES_PER_FRAME_CHANNEL`] state (73 a
    /// stereo frame).
    #[test]
    fn a_resize_sizes_every_buffer_to_the_width() {
        let mut s = BlockScratch::<f32>::new(UNPREPARED_FRAMES, 1);
        assert_eq!((s.frames(), s.width()), (UNPREPARED_FRAMES, 1));
        for (frames, width) in [(512usize, 2usize), (64, 6), (1024, 2), (1, 1)] {
            s.resize(frames, width);
            assert_eq!(s.frames(), frames);
            assert_eq!(s.width(), width);
            for lanes in [&s.out, &s.raw] {
                for (c, l) in lanes.lanes().iter().enumerate() {
                    let want = if c < width { frames } else { 0 };
                    assert_eq!(l.len(), want, "{frames}x{width}: lane {c}");
                    assert_eq!(l.capacity(), want, "{frames}x{width}: lane {c}");
                }
            }
            let g = &s.gather;
            assert_eq!(
                (g.index.len(), g.frac.len(), g.live.len()),
                (frames, frames, frames)
            );
            assert!(g.y.iter().all(|l| l.len() == frames));
            let prime_bytes = s
                .prime
                .lanes()
                .iter()
                .map(|l| l.capacity() * 4)
                .sum::<usize>();
            assert_eq!(prime_bytes, PRIME_SCRATCH.get() * width * 4);
            let bytes = (s.positions.capacity() + s.reads.capacity())
                * size_of::<Option<SamplePosition>>()
                + g.index.capacity() * 4
                + g.frac.capacity() * 4
                + g.live.capacity()
                + g.y.iter().map(|l| l.capacity() * 4).sum::<usize>()
                + [&s.out, &s.raw]
                    .iter()
                    .flat_map(|l| l.lanes().iter())
                    .map(|l| l.capacity() * 4)
                    .sum::<usize>();
            assert_eq!(
                bytes,
                frames * (BYTES_PER_FRAME_FIXED + width * BYTES_PER_FRAME_CHANNEL)
            );
        }
        assert_eq!(BYTES_PER_FRAME_FIXED + 2 * BYTES_PER_FRAME_CHANNEL, 73);
    }

    /// Every voice node sizes its scratch from `Prepare::max_block` and
    /// its own width, and a re-prepare resizes it: a stereo pool, a stereo
    /// `VoiceNode` and a `MemorySource` prepared at 512 hold 512 frames,
    /// re-prepared at 1024 hold 1024, and at 64, 64; the pool's and the
    /// node's lanes are two wide, not `MAX_CHANNELS`. Unprepared, the pool
    /// and the node hold `UNPREPARED_FRAMES` and the source none. (The disk voice's own check is
    /// `disk_voice::tests::a_disk_voice_sizes_its_positions_from_max_block`.)
    #[test]
    fn every_voice_node_sizes_its_scratch_from_max_block() {
        use crate::voice::{VoiceNode, VoicePool};
        use tutti_graph::{Node, Prepare};
        use tutti_units::{Beat, SampleRate, Samples};

        let mut wave = tutti_buffers::Wave::new(2, SampleRate::SR_48K);
        wave.append(Interleaved::new(&[0.0, 0.0], wave.layout()).unwrap())
            .unwrap();
        let wave = std::sync::Arc::new(wave);
        let source = placed_source(
            std::sync::Arc::clone(&wave),
            VoiceWindow::from(Beat::new(0.0)),
        );
        let mut pool = VoicePool::<f32>::new();
        let mut node = VoiceNode::new(source.clone(), Default::default())
            .expect("a finite gain and a nonempty width");
        let mut memory = source;
        assert_eq!(pool.scratch_size(), (UNPREPARED_FRAMES, 2));
        assert_eq!(node.scratch_size(), (UNPREPARED_FRAMES, 2));
        assert_eq!(memory.scratch_frames(), 0);
        for max in [512usize, 1024, 64] {
            let p = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(max)));
            pool.prepare(&p);
            node.prepare(&p);
            memory.prepare(&p);
            assert_eq!(pool.scratch_size(), (max, 2), "pool at {max}");
            assert_eq!(node.scratch_size(), (max, 2), "voice node at {max}");
            assert_eq!(memory.scratch_frames(), max, "memory source at {max}");
        }
    }
}
