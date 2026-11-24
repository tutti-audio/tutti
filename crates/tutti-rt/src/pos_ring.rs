//! [`PosRing`]: frames a writer thread places ahead of a reader, indexed by
//! position rather than consumed in order.
//!
//! **The invariant.** A slot the reader's block in flight may read is never
//! written until that block is over: a position the block may read — inside
//! one of the ranges it claimed, and held by the window it took — keeps the
//! frame it had when the block took the window, for as long as the block
//! runs. Every write goes to the window's end; a position leaves the window
//! only by a shrink the writer counts as possibly still held (a *stale*
//! position) until the reader says it has moved on, or by being overwritten
//! round the ring, which the writer checks against the block's ranges first.
//!
//! One writer (a streaming thread, [`PosWriter`]) and one reader (the audio
//! thread, [`PosReader`]) share `N` frame slots. Slot `s mod N` holds position
//! `s` — a position on whatever line the two agree on, such as a file
//! counted straight on. The writer publishes the window
//! `[from, to)` of positions the slots hold as one packed word, appends at
//! `to`, and may shrink the window (a retraction, a raise, a reset); the
//! reader claims the positions its block may read, takes the window, and reads
//! any position the window holds inside its claim, in any order, as often as
//! it likes. Nothing is consumed and nothing on the reading side locks,
//! allocates or waits.
//!
//! **One of each, by type.** [`PosRing::new`] returns the one writer and the
//! one reader. Neither is `Clone`; the writer's methods take `&mut self`; a
//! read goes through a [`PosClaim`], which borrows the reader, so a claim
//! cannot outlive the next one.
//!
//! # Why a write never tears a read
//!
//! Every sample is an atomic word of its format's width ([`RingSample`]), so
//! a slip in the protocol below is a wrong sample, never undefined behaviour;
//! the protocol is what makes it never a wrong sample. The reader, per block ([`PosReader::claim`]):
//!
//! 1. stores where it plays, and the ranges `[a, e)` its block may read
//!    (`Release`);
//! 2. a `SeqCst` fence (**F1**);
//! 3. loads the shrink generation `g` (`Acquire`);
//! 4. loads the window (`Acquire`);
//! 5. echoes `g` as the generation it holds (`Release`).
//!
//! The writer never writes a slot the block in flight may read:
//!
//! - **Going round the ring.** Writing position `s` reuses the slot of every
//!   `s ± kN`. Before a write the writer raises the window's start past
//!   `s - N` (`Release`), then a `SeqCst` fence (**F2**), then it loads the
//!   reader's ranges (`Acquire`). By the fence rule (C++20 [atomics.fences]),
//!   F1 and F2 order the reader's range stores against the writer's raise:
//!   either the reader's window load sees the raised start, or the writer's
//!   range load sees the reader's ranges and skips any `s` whose slot another
//!   position of a range holds ([`first_alias`]). A write that is cut short
//!   leaves the start where the write it did make needs it, no higher: every
//!   position it takes out of the window is one it overwrote, and one it
//!   raised past and then did not overwrite is put back.
//! - **Shrinking.** A retraction, a raise or a reset stores the smaller window
//!   (`Release`), *then* bumps the generation (`Release`). The positions it
//!   removed are stale: a block that took the window before it may still read
//!   them. The writer does not write a stale position inside the reader's
//!   ranges until the reader echoes the new generation. That echo is a
//!   `Release` the writer loads with `Acquire`, and it comes after the reader
//!   loaded the new generation (`Acquire`, pairing with the bump) and then a
//!   window no older than the shrink; and after every read of the block
//!   before, in program order. So once the writer sees it, no block that could
//!   hold a stale position is still reading.
//!
//! Which orderings carry that: F1/F2 (going round); the window's
//! `Release`/`Acquire` (a claimed window's frames were stored before it was
//! published); the generation's `Release`/`Acquire` (the echo implies the
//! shrunk window); the echo's `Release`/`Acquire` (the block before it is
//! over). The ranges' `Release`/`Acquire` are redundant beside F1/F2, and
//! close load buffering formally. A position *in* a range but never in any
//! window the reader could hold is free to write: the reader reads it only
//! once a later window holds it.
//!
//! A reader that stops claiming ([`PosReader::idle`]) clears its ranges and
//! echoes the generation, so a paused reader holds nothing back.
//!
//! `tests/pos_ring_loom.rs` checks this code under loom (its atomics become
//! loom's under `--cfg loom`), with models that fail without each part.

use std::fmt::Debug;

use crate::sync::{Arc, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};
use tutti_buffers::ChannelLayout;
use tutti_buffers::{BufferError, Float, I24, Interleaved, Sample};
use tutti_units::Samples;

/// The atomics: `std`'s in a real build, `loom`'s under `--cfg loom`, so the
/// model in `tests/pos_ring_loom.rs` checks this code rather than a replica.
mod sealed {
    /// Keeps [`RingSample`](super::RingSample) to the formats this crate maps
    /// to an atomic word.
    pub trait Sealed {}
}

/// A [`Sample`] format a [`PosRing`] can hold: one with an atomic word of its
/// width, so a slot is a run of atomics and a slip in the protocol is a wrong
/// sample, never undefined behaviour.
///
/// Implemented for every [`Sample`] format (`i8`, `i16`, [`I24`], `i32`,
/// `f32`, `f64`) and sealed.
pub trait RingSample: Sample + sealed::Sealed {
    /// The atomic word one sample occupies.
    type Atom: Debug + Send + Sync;

    /// A word holding `v`.
    fn atom(v: Self) -> Self::Atom;

    /// The sample the word holds, a `Relaxed` load.
    fn load(word: &Self::Atom) -> Self;

    /// Stores `v` in the word, a `Relaxed` store.
    fn store(word: &Self::Atom, v: Self);
}

/// A [`Float`] format a [`PosRing`] holds: `f32` or `f64`.
pub trait RingFloat: Float + RingSample {}

impl<F: Float + RingSample> RingFloat for F {}

macro_rules! ring_sample {
    ($t:ty, $atom:ident, $to:expr, $from:expr) => {
        impl sealed::Sealed for $t {}

        impl RingSample for $t {
            type Atom = $atom;
            #[inline]
            fn atom(v: Self) -> $atom {
                $atom::new($to(v))
            }
            #[inline]
            fn load(word: &$atom) -> Self {
                $from(word.load(Ordering::Relaxed))
            }
            #[inline]
            fn store(word: &$atom, v: Self) {
                word.store($to(v), Ordering::Relaxed);
            }
        }
    };
}

ring_sample!(i8, AtomicU8, |v: i8| v as u8, |r: u8| r as i8);
ring_sample!(i16, AtomicU16, |v: i16| v as u16, |r: u16| r as i16);
ring_sample!(i32, AtomicU32, |v: i32| v as u32, |r: u32| r as i32);
// A word only ever holds a value stored from a valid `I24` (or the initial
// silence), so the range check cannot fail on a load.
ring_sample!(I24, AtomicU32, |v: I24| v.get() as u32, |r: u32| {
    I24::new(r as i32).expect("a slot holds only values stored from an I24")
});
ring_sample!(f32, AtomicU32, f32::to_bits, f32::from_bits);
ring_sample!(f64, AtomicU64, f64::to_bits, f64::from_bits);

/// Bits of the packed window word that hold its length; the rest its end.
const LEN_BITS: u32 = 24;

/// The most frames a [`PosRing`] holds.
pub const MAX_POS_RING_FRAMES: usize = (1 << LEN_BITS) - 1;

/// The largest window end a [`PosRing`] holds.
///
/// The window's length is packed in 24 bits and its end takes the other 40,
/// so a position must stay below `2^40` (about 290 days of frames at
/// 44.1 kHz). The writer enforces it in every build: [`PosWriter::push`]
/// stops short of it and [`PosWriter::reset`] saturates at it, so a window
/// never passes it and no caller can make the writer panic with a position.
/// The reader packs nothing, so the check costs the audio thread nothing.
pub const MAX_POS_RING_POSITION: u64 = (1 << (64 - LEN_BITS)) - 1;

/// A window of ring positions `[from, to)`: the positions a [`PosRing`]'s
/// slots hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingWindow {
    /// The first position held.
    pub from: u64,
    /// One past the last position held.
    pub to: u64,
}

impl RingWindow {
    fn pack(self) -> u64 {
        assert!(
            self.to <= MAX_POS_RING_POSITION,
            "a window end past 2^40 - 1"
        );
        debug_assert!(self.to - self.from <= MAX_POS_RING_FRAMES as u64);
        (self.to << LEN_BITS) | (self.to - self.from)
    }

    fn unpack(word: u64) -> Self {
        let to = word >> LEN_BITS;
        Self {
            from: to - (word & ((1 << LEN_BITS) - 1)),
            to,
        }
    }

    /// Returns whether position `s` is in the window.
    #[inline]
    pub fn holds(&self, s: u64) -> bool {
        s >= self.from && s < self.to
    }
}

/// A ring of frames a writer thread places ahead of the audio thread, indexed
/// by position rather than consumed in order.
///
/// One writer (a streaming thread, [`PosWriter`]) and one reader (the audio
/// thread, [`PosReader`]) share `frames` slots of `stride` samples each. Slot
/// `s mod frames` holds position `s`, a position on whatever line the two
/// agree on (a disk stream uses the file's frame index). The writer publishes
/// the [`RingWindow`] of positions the slots hold, appends at its end with
/// [`PosWriter::push`], and may shrink it ([`retract_to`](PosWriter::retract_to),
/// [`raise_from`](PosWriter::raise_from), [`reset`](PosWriter::reset)). Once
/// per block the reader [`claim`](PosReader::claim)s the ranges it may read,
/// takes the window, and reads any position inside both, in any order, as
/// often as it likes. Nothing is consumed, and nothing on the reading side
/// locks, allocates or waits.
///
/// **The guarantee.** A position the reader's block may read keeps the frame
/// it had when the block took its window, for as long as the block runs: the
/// writer never overwrites a slot the block in flight may read, even after a
/// shrink, until the reader's next claim (or [`idle`](PosReader::idle)) shows
/// it has moved on. A write that would break this is cut short instead, and
/// `push` reports how many frames landed.
///
/// **One of each, by type.** [`PosRing::new`] returns the one writer and the
/// one reader. Neither is `Clone`; the writer's methods take `&mut self`; a
/// read goes through a [`PosClaim`], which borrows the reader, so a claim
/// cannot outlive the next one. The ring itself is only reachable read-only,
/// through [`PosWriter::watch`], for status displays and tests.
///
/// # Examples
///
/// ```
/// use tutti_buffers::Interleaved;
/// use tutti_rt::PosRing;
/// use tutti_units::Samples;
/// use tutti_buffers::ChannelLayout;
///
/// // 8 stereo frames, nothing kept behind the play position.
/// let (mut writer, mut reader) = PosRing::<f32>::new(8, ChannelLayout::STEREO, 0)?;
///
/// // Streaming thread: frames for positions 0..3.
/// let data = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
/// let frames = Interleaved::new(&data, ChannelLayout::STEREO)?;
/// assert_eq!(writer.push(frames)?, Samples::new(3));
///
/// // Audio thread, once per block: play from 0, may read [0, 3).
/// let claim = reader.claim(0, [(0, 3), (0, 0)]);
/// assert!(claim.holds(2));
/// assert_eq!(claim.sample(1, 1), 0.4);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
pub struct PosRing<S: RingSample = f32> {
    layout: ChannelLayout,
    frames: usize,
    /// `ceil(2^64 / frames)` (wrapping to 0 for one frame): [`slot`](Self::slot)'s
    /// reciprocal.
    recip: u64,
    /// Positions the writer keeps behind where the reader plays.
    keep_behind: u64,
    /// `frames` slots of a frame's samples, each an atomic word of its format's width.
    slots: Box<[S::Atom]>,
    /// The packed [`RingWindow`].
    window: AtomicU64,
    /// Where the reader plays (the reader's claims, or the writer before one).
    play: AtomicU64,
    /// Reader → writer: the ranges its block may read, `[a0, e0)`, `[a1, e1)`.
    reads: [AtomicU64; 4],
    /// Writer → reader: bumped after every shrink.
    generation: AtomicU64,
    /// Reader → writer: the generation its latest claim loaded.
    claimed: AtomicU64,
}

impl<S: RingSample> std::fmt::Debug for PosRing<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosRing")
            .field("frames", &self.frames)
            .field("layout", &self.layout)
            .field("window", &self.window())
            .finish_non_exhaustive()
    }
}

impl<S: RingSample> PosRing<S> {
    /// Creates an empty ring and returns its one writer and its one reader.
    ///
    /// `frames` is the slot count, within `1..=`[`MAX_POS_RING_FRAMES`];
    /// `layout` the width of a frame. The writer never reuses the slot of a
    /// position less than `keep_behind` behind where the reader plays (for a
    /// reader that taps positions behind the one it plays). Allocates the
    /// slots, so call it off the audio thread.
    ///
    /// # Errors
    ///
    /// [`BufferError::ZeroWidth`] for the empty layout;
    /// [`BufferError::CapacityOutOfRange`] for a `frames` of zero or past
    /// [`MAX_POS_RING_FRAMES`].
    #[allow(clippy::new_ret_no_self)] // the ring is its two ends
    #[must_use = "the ring's two ends are the result"]
    pub fn new(
        frames: usize,
        layout: ChannelLayout,
        keep_behind: u64,
    ) -> Result<(PosWriter<S>, PosReader<S>), BufferError> {
        if layout.count() == 0 {
            return Err(BufferError::ZeroWidth);
        }
        if !(1..=MAX_POS_RING_FRAMES).contains(&frames) {
            return Err(BufferError::CapacityOutOfRange {
                frames,
                max: MAX_POS_RING_FRAMES,
            });
        }
        let ring = Arc::new(Self {
            layout,
            frames,
            recip: recip(frames as u64),
            keep_behind,
            slots: (0..layout.interleaved_len(Samples::new(frames)))
                .map(|_| S::atom(S::SILENCE))
                .collect(),
            window: AtomicU64::new(0),
            play: AtomicU64::new(0),
            reads: std::array::from_fn(|_| AtomicU64::new(0)),
            generation: AtomicU64::new(0),
            claimed: AtomicU64::new(0),
        });
        let writer = PosWriter {
            ring: Arc::clone(&ring),
            stale: (u64::MAX, 0),
        };
        Ok((writer, PosReader { ring }))
    }

    /// Returns the slot count, in frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Returns the slot of position `s`, `s mod frames`, without a divide.
    ///
    /// Lemire, Kaser and Kurz's direct remainder ("Faster Remainder by Direct
    /// Computation", 2019, Theorem 1): with `c = ceil(2^64 / d)`, the low 64
    /// bits of `c * s`, times `d`, shifted down 64, are `s mod d` for every
    /// `s < 2^N` once `c * d - 2^64 <= 2^(64 - N)`. Here `c * d - 2^64 < d <
    /// 2^24` and `N = 40`, the bounds the window word puts on a ring
    /// ([`MAX_POS_RING_FRAMES`]) and on a position, so it is exact for every
    /// position a window can hold. (For one frame `c` wraps to 0, and so does
    /// the slot.) Whatever `s`, the result is below `frames`: a position past
    /// `2^40` maps to a wrong slot, never out of bounds.
    ///
    /// The reader's taps call this per frame (four per output frame in the
    /// sampler's cubic), where a `u64` divide was most of a tap's cost.
    #[inline]
    fn slot(&self, s: u64) -> usize {
        rem(s, self.recip, self.frames as u64) as usize
    }

    /// Returns the window now: a glance for status, not a claim (nothing may
    /// be read by it).
    #[inline]
    pub fn window(&self) -> RingWindow {
        RingWindow::unpack(self.window.load(Ordering::Acquire))
    }

    /// Returns where the reader plays (or where the writer last said it
    /// would): the position the writer fills ahead of.
    pub fn play(&self) -> u64 {
        self.play.load(Ordering::Relaxed)
    }

    /// Returns how many times the window has shrunk (the generation).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The reader's ranges, `Acquire` (the writer has fenced, or is about to
    /// only cap with them).
    fn in_flight(&self) -> [(u64, u64); 2] {
        let r = |i: usize| self.reads[i].load(Ordering::Acquire);
        [(r(0), r(1)), (r(2), r(3))]
    }
}

/// The one writer of a [`PosRing`], owned by a streaming thread.
///
/// Not `Clone`. None of its methods is meant for the audio thread: `push`
/// runs a `SeqCst` fence and a loop over the frames it writes.
pub struct PosWriter<S: RingSample = f32> {
    ring: Arc<PosRing<S>>,
    /// The span of positions shrinks have removed since the reader last
    /// echoed the generation, `[lo, hi)`: what a block that took an older
    /// window may still hold. Writer-local.
    stale: (u64, u64),
}

impl<S: RingSample> std::fmt::Debug for PosWriter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosWriter")
            .field("ring", &self.ring)
            .field("stale", &self.stale)
            .finish()
    }
}

impl<S: RingSample> PosWriter<S> {
    /// Returns a read-only handle on the shared state, for glances (the window,
    /// the play position, the generation). It reads no sample.
    pub fn watch(&self) -> Arc<PosRing<S>> {
        Arc::clone(&self.ring)
    }

    /// Returns the slot count, in frames.
    pub fn frames(&self) -> usize {
        self.ring.frames
    }

    /// Returns the width of a frame.
    pub fn layout(&self) -> ChannelLayout {
        self.ring.layout
    }

    /// Returns the window now.
    #[inline]
    pub fn window(&self) -> RingWindow {
        self.ring.window()
    }

    /// Returns where the reader plays (or where this writer last said it
    /// would).
    pub fn play(&self) -> u64 {
        self.ring.play()
    }

    /// Returns one past the last position the reader's block in flight may
    /// read, as of a `SeqCst` fence now (0 for an idle reader).
    pub fn in_flight_end(&self) -> u64 {
        fence(Ordering::SeqCst);
        let [(_, e0), (_, e1)] = self.ring.in_flight();
        e0.max(e1)
    }

    /// Sets where the reader will play before its first claim, so the writer
    /// fills ahead of the right position.
    pub fn set_play(&mut self, play: u64) {
        self.ring.play.store(play, Ordering::Relaxed);
    }

    /// Whether the reader's latest claim has seen every shrink so far.
    fn seen(&self) -> bool {
        self.ring.claimed.load(Ordering::Acquire) == self.ring.generation.load(Ordering::Relaxed)
    }

    /// Shrinks to `window`, removing `[removed.0, removed.1)`, then bumps the
    /// generation (the order is the module docs' argument).
    fn shrink(&mut self, window: RingWindow, removed: (u64, u64)) {
        // What earlier shrinks removed stays stale only until the reader has
        // seen them: an echo of the current generation says it has.
        if self.seen() {
            self.stale = (u64::MAX, 0);
        }
        self.ring.window.store(window.pack(), Ordering::Release);
        if removed.0 < removed.1 {
            self.stale = (self.stale.0.min(removed.0), self.stale.1.max(removed.1));
        }
        self.ring.generation.fetch_add(1, Ordering::Release);
    }

    /// Drops every position at and past `x`, for a rewrite from `x`.
    pub fn retract_to(&mut self, x: u64) {
        let w = self.window();
        if x < w.to {
            self.shrink(
                RingWindow {
                    from: w.from.min(x),
                    to: x,
                },
                (x.max(w.from), w.to),
            );
        }
    }

    /// Drops every position below `y`.
    pub fn raise_from(&mut self, y: u64) {
        let w = self.window();
        if y > w.from {
            self.shrink(
                RingWindow {
                    from: y.min(w.to),
                    to: w.to,
                },
                (w.from, y.min(w.to)),
            );
        }
    }

    /// Empties the window and restarts it at `start`, for a reader that moved
    /// outside the current one.
    ///
    /// A `start` past [`MAX_POS_RING_POSITION`] saturates there: the window
    /// is empty at the limit and [`push`](Self::push) lands nothing.
    pub fn reset(&mut self, start: u64) {
        let start = start.min(MAX_POS_RING_POSITION);
        let w = self.window();
        self.shrink(
            RingWindow {
                from: start,
                to: start,
            },
            (w.from, w.to),
        );
    }

    /// Appends interleaved frames at the window's end and returns how many
    /// frames landed.
    ///
    /// The count is short where the next write would reuse a slot the reader's
    /// block in flight may read, or where the ring is full ahead of where the
    /// reader plays; push the rest again later.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] if the view's width is not the ring's;
    /// nothing is written.
    #[must_use = "the count is how many frames landed; push the rest again"]
    pub fn push(&mut self, view: Interleaved<'_, S>) -> Result<Samples, BufferError> {
        let ring = &*self.ring;
        if view.layout() != ring.layout {
            return Err(BufferError::WidthMismatch {
                src: view.layout(),
                dst: ring.layout,
            });
        }
        let n = view.frames().get() as u64;
        let frames = ring.frames as u64;
        let RingWindow { from, to } = self.window();
        // Never reuse the slot of a position the reader still stands on.
        let room = ring
            .play()
            .saturating_sub(ring.keep_behind)
            .saturating_add(frames)
            .saturating_sub(to);
        let mut end = (to + n.min(room)).min(MAX_POS_RING_POSITION);
        // A first cap from the ranges as they stand, so the raise below is
        // not needlessly high. Not the check: that is after the fence.
        for (a, e) in ring.in_flight() {
            end = end.min(first_alias(to, a, e, frames));
        }
        let raised = from.max(end.saturating_sub(frames));
        if raised != from {
            ring.window
                .store(RingWindow { from: raised, to }.pack(), Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let held = !self.seen();
        let stale = self.stale;
        for (a, e) in ring.in_flight() {
            end = end.min(first_alias(to, a, e, frames));
            // A block that took a window from before a shrink may hold the
            // positions it removed.
            let (lo, hi) = (a.max(to).max(stale.0), e.min(stale.1));
            if held && lo < hi {
                end = end.min(lo);
            }
        }
        if !held {
            self.stale = (u64::MAX, 0);
        }
        // The start the write needs: only what it overwrites leaves the
        // window. What the raise took out and the write does not reach is put
        // back (its slots were not touched).
        let needed = from.max(end.saturating_sub(frames));
        if end <= to {
            if raised != from {
                ring.window
                    .store(RingWindow { from, to }.pack(), Ordering::Release);
            }
            return Ok(Samples::new(0));
        }
        let landing = Samples::new((end - to) as usize);
        for (i, frame) in view.slice(Samples::new(0)..landing).iter().enumerate() {
            let at = ring
                .layout
                .interleaved_len(Samples::new(ring.slot(to + i as u64)));
            for (word, &s) in ring.slots[at..at + frame.len()].iter().zip(frame) {
                S::store(word, s);
            }
        }
        ring.window.store(
            RingWindow {
                from: needed,
                to: end,
            }
            .pack(),
            Ordering::Release,
        );
        Ok(landing)
    }
}

/// The one reader of a [`PosRing`], owned by the audio thread.
///
/// Not `Clone`. Its methods are wait-free and allocation-free.
pub struct PosReader<S: RingSample = f32> {
    ring: Arc<PosRing<S>>,
}

impl<S: RingSample> std::fmt::Debug for PosReader<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosReader")
            .field("ring", &self.ring)
            .finish()
    }
}

impl<S: RingSample> PosReader<S> {
    /// Claims this block's reads; call it once per block, before any read.
    ///
    /// `play` is where the reader plays; `reads` the (up to two) ranges
    /// `[a, e)` the block may read (pass `(0, 0)` for an unused one). The
    /// returned [`PosClaim`] holds the window taken now. It borrows the
    /// reader, so it ends before the next claim starts.
    #[inline]
    #[must_use = "the claim is the only way to read the ring"]
    pub fn claim(&mut self, play: u64, reads: [(u64, u64); 2]) -> PosClaim<'_, S> {
        let ring = &*self.ring;
        ring.play.store(play, Ordering::Relaxed);
        for (i, (a, e)) in reads.into_iter().enumerate() {
            ring.reads[2 * i].store(a, Ordering::Release);
            ring.reads[2 * i + 1].store(e, Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let generation = ring.generation.load(Ordering::Acquire);
        let window = RingWindow::unpack(ring.window.load(Ordering::Acquire));
        ring.claimed.store(generation, Ordering::Release);
        PosClaim {
            ring,
            window,
            reads,
        }
    }

    /// Marks a block that reads nothing.
    ///
    /// Clears the reader's ranges and acknowledges every shrink so far, so a
    /// paused reader holds no write back. Without it the ranges of its last
    /// claim would keep blocking the writer.
    #[inline]
    pub fn idle(&mut self) {
        let ring = &*self.ring;
        for r in &ring.reads {
            r.store(0, Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let generation = ring.generation.load(Ordering::Acquire);
        ring.claimed.store(generation, Ordering::Release);
    }
}

/// One block's claim on a [`PosRing`]: the window it took and the ranges it
/// may read.
///
/// Returned by [`PosReader::claim`].
#[derive(Debug)]
pub struct PosClaim<'a, S: RingSample = f32> {
    ring: &'a PosRing<S>,
    window: RingWindow,
    reads: [(u64, u64); 2],
}

impl<'a, S: RingSample> PosClaim<'a, S> {
    /// Returns the window this block may read.
    #[inline]
    pub fn window(&self) -> RingWindow {
        self.window
    }

    /// Returns whether position `s` may be read this block: the window holds
    /// it. The caller keeps to the ranges it claimed.
    #[inline]
    pub fn holds(&self, s: u64) -> bool {
        self.window.holds(s)
    }

    /// Returns the frame at position `s`, found once for all its channels.
    ///
    /// # Panics
    ///
    /// In debug builds, if `s` is outside the window or the claimed ranges.
    /// A release build reads whatever the slot holds.
    #[inline]
    pub fn frame(&self, s: u64) -> PosFrame<'a, S> {
        debug_assert!(self.window.holds(s), "position {s} outside the window");
        debug_assert!(
            self.reads.iter().any(|&(a, e)| (a..e).contains(&s)),
            "position {s} outside the claimed ranges"
        );
        let slot = self.ring.slot(s);
        let layout = self.ring.layout;
        let (from, to) = (
            layout.interleaved_len(Samples::new(slot)),
            layout.interleaved_len(Samples::new(slot + 1)),
        );
        PosFrame(&self.ring.slots[from..to])
    }

    /// Returns channel `c` of position `s`.
    ///
    /// # Panics
    ///
    /// In debug builds, as [`frame`](Self::frame) does, and if `c` is not
    /// below the stride.
    #[inline]
    pub fn sample(&self, s: u64, c: usize) -> S {
        self.frame(s).get(c)
    }

    /// Copies channel `c` of positions `from..from + out.len()` into `out`,
    /// in order: what [`sample`](Self::sample) reads at each, with the slot
    /// found once and the run split at most once where it wraps the ring.
    ///
    /// # Panics
    ///
    /// In debug builds, if the run leaves the window or the claimed ranges,
    /// if it is longer than the ring, or if `c` is not below the stride. A
    /// release build reads whatever the slots hold, and silence for a `c`
    /// past the stride.
    #[inline]
    pub fn copy_channel(&self, from: u64, c: usize, out: &mut [S]) {
        let Some(last) = (out.len() as u64).checked_sub(1).map(|n| from + n) else {
            return;
        };
        debug_assert!(
            self.window.holds(from) && self.window.holds(last),
            "positions {from}..={last} outside the window"
        );
        debug_assert!(
            self.reads.iter().any(|&(a, e)| a <= from && last < e),
            "positions {from}..={last} outside the claimed ranges"
        );
        debug_assert!(out.len() <= self.ring.frames, "a run longer than the ring");
        let stride = usize::from(self.ring.layout.count());
        debug_assert!(c < stride, "channel {c} past the stride");
        if c >= stride {
            out.fill(S::SILENCE);
            return;
        }
        let layout = self.ring.layout;
        let start = self.ring.slot(from);
        let (head, tail) = out.split_at_mut(out.len().min(self.ring.frames - start));
        let (lo, hi) = (
            layout.interleaved_len(Samples::new(start)),
            layout.interleaved_len(Samples::new(start + head.len())),
        );
        copy_strided(&self.ring.slots[lo..hi], stride, c, head);
        copy_strided(&self.ring.slots, stride, c, tail);
    }
}

/// Channel `c` of each whole `stride`-long frame of `atoms`, into `out` in
/// order, as far as the shorter of the two runs.
#[inline]
fn copy_strided<S: RingSample>(atoms: &[S::Atom], stride: usize, c: usize, out: &mut [S]) {
    for (o, frame) in out.iter_mut().zip(atoms.chunks_exact(stride)) {
        *o = S::load(&frame[c]);
    }
}

/// One claimed frame's samples, from [`PosClaim::frame`].
#[derive(Clone, Copy, Debug)]
pub struct PosFrame<'a, S: RingSample = f32>(&'a [S::Atom]);

impl<S: RingSample> PosFrame<'_, S> {
    /// Returns channel `c`.
    ///
    /// # Panics
    ///
    /// In debug builds, if `c` is not below the ring's stride. A release build
    /// returns 0 there.
    #[inline]
    pub fn get(&self, c: usize) -> S {
        debug_assert!(c < self.0.len(), "channel {c} past the stride");
        self.0.get(c).map_or(S::SILENCE, S::load)
    }
}

/// `ceil(2^64 / d)`, wrapping to 0 for `d == 1`: [`rem`]'s reciprocal.
fn recip(d: u64) -> u64 {
    (u64::MAX / d).wrapping_add(1)
}

/// `s mod d` from `c = recip(d)`, exact for `s < 2^40` and `d < 2^24` (see
/// [`PosRing::slot`]); below `d` for any `s`.
#[inline]
fn rem(s: u64, c: u64, d: u64) -> u64 {
    ((u128::from(c.wrapping_mul(s)) * u128::from(d)) >> 64) as u64
}

/// The first position at or past `to` whose slot also holds a position of the
/// range `[a, e)` other than itself — an alias `[a + kN, e + kN)`, `k != 0`,
/// of a ring `n` slots long. Writing a position *in* the range is no conflict
/// of this kind (see [`PosWriter::push`] for the other kind).
fn first_alias(to: u64, a: u64, e: u64, n: u64) -> u64 {
    if e <= a {
        return u64::MAX;
    }
    // A position `s` of the range has another of the range in its slot when
    // the range reaches `s + n`, or reaches back to `s - n`.
    let inside = |s: u64| if s + n < e { s } else { s.max(a + n) };
    if (a..e).contains(&to) {
        return inside(to);
    }
    let len = e - a;
    let d = (to as i128 - a as i128).rem_euclid(n as i128) as u64;
    if d < len {
        return to;
    }
    let next = to + (n - d);
    if next == a { inside(a) } else { next }
}

#[cfg(all(test, not(loom)))]
mod tests;
