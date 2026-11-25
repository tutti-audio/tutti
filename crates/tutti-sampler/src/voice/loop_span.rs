//! A loop, as the sequence of file frames a looped voice plays.
//!
//! Shared by every reader that indexes a file directly — `MemorySource` (its
//! free-running loop and a placed voice's) and the offline disk reader a forked
//! disk voice plays through — and by the streamer, which writes a live stream's
//! ring by it (`streamer::loops::Mapping::fill`), so the same loop sounds the
//! same on each. Pure arithmetic: no allocation, safe on the audio thread.
//!
//! # The loop is a sequence of frames, and the interpolator reads that sequence
//!
//! A looped voice plays file frames `0, 1, …, end - 1`, then `start, …, end -
//! 1` again, forever (`resume` rather than `start` when the fade has to go into
//! the loop's head, below). Two things follow:
//!
//! - The crossfade leads into `start`, it does not replay it. Frame `end -
//!   fade + k` (for `k` in `0..fade`) is blended toward frame `start - fade +
//!   k`: the material that leads into `start`. The last blended frame is almost
//!   all `start - 1`, and the next frame played is `start`, so the join is the
//!   file's own step from `start - 1` to `start`. Blending toward `start + k`
//!   and then playing `start` again would play the loop's head twice, a jump
//!   of `fade` frames at every wrap.
//! - Taps near the end read through the wrap. The cubic kernel reads two
//!   frames ahead of a position; a position within two frames of `end` reads
//!   `start`, `start + 1` there, which are the frames the loop plays next, not
//!   `end`, `end + 1` from the file past the loop. Behind a position on the
//!   loop's start, once the voice has been round, the frame before is `end - 1`,
//!   the one the loop just played. This is the sequence the streamer writes into
//!   a looped stream's ring (fade blended in, wrapping to `resume`), so a live
//!   voice's own four-tap history reads the same frames.
//!
//! # The fade gains
//!
//! Frame `k` of an `n`-frame fade reads `tail * out + lead * in`, the gains of
//! [`FadeLaw::EqualPower`] at the position `x = (k + 1) / (n + 1)`, which
//! excludes both endpoints: the frame before the fade is pure tail, the frame
//! after it is pure `start`. The tail and the lead-in are different stretches
//! of the source, so they add in power: `out² + in² = 1` at every frame, and a
//! seam between uncorrelated material holds its level.
//!
//! # When there is too little before `start`: fade into the head instead
//!
//! The lead-in is the `fade` frames before `start`, so a loop starting at frame
//! 0 (or at any frame closer to the file's start than the fade asked for) has
//! none. Then the tail fades into the loop's own head, `[start, start + fade)`,
//! and the wrap resumes at `start + fade`: frame `end - fade + k` blends toward
//! `start + k`, the last blended frame is almost all `start + fade - 1`, and the
//! loop continues at `start + fade` — still the file's own step at the join,
//! with no material before `start` needed. The loop that repeats is then
//! `[start + fade, end)`; its head is heard only on the first pass (and inside
//! every fade). The fade is at most half the loop in that mode, so the head and
//! the tail never overlap.
//!
//! Both modes are one rule with a resume frame: the tail blends toward the
//! `fade` frames that lead into `resume`, and the wrap lands on `resume`.
//! `resume` is `start` when the lead-in exists, `start + fade` when it does not.
//! A fade is never longer than the loop, and the loop's end is clamped to the
//! file where its length is known.

use std::cmp::Ordering;
use std::ops::Range;
use tutti_buffers::Float;
use tutti_graph::FadeLaw;
use tutti_units::{SamplePosition, Samples};

/// A loop over whole file frames `[start, end)`, with a crossfade into it: the
/// one value every tier takes (a memory voice, a disk voice, a stream, a pool
/// slot, a node's handle), checked where it is made.
///
/// The span is in whole frames of the file (`Samples`: a loop point between
/// frames has no meaning, so none is writable), `start < end` and the
/// crossfade no longer than the span, or no `Loop` exists to hand a voice. A
/// crossfade of zero is a hard loop. Where a voice's file is shorter than the
/// span's end, the loop plays to the file's end (the file's length is not
/// always known where the loop is made: a stream's header is read on the
/// streamer's thread); a loop that starts at or past the file's end plays
/// nothing in a loop.
///
/// The fade is equal-power ([`FadeLaw::EqualPower`]), over the last
/// `crossfade` frames before `end`, and continuous at the wrap:
///
/// - With at least `crossfade` frames before `start`, the tail blends toward
///   the frames leading into `start`, and the wrap lands on `start`.
/// - With fewer (a loop from frame 0, say), the tail blends toward the loop's
///   own head `[start, start + fade)` and the wrap lands on `start + fade`:
///   the loop that repeats is `[start + fade, end)`, and the fade is at most
///   half the loop's length.
///
/// ```
/// # use tutti_sampler::Loop;
/// # use tutti_units::Samples;
/// let looped = Loop::new(Samples::new(100)..Samples::new(400))?
///     .with_crossfade(Samples::new(32))?;
/// assert_eq!(looped.len(), Samples::new(300));
/// // An empty or reversed span, or a fade longer than the loop, is not a loop.
/// assert!(Loop::new(Samples::new(400)..Samples::new(100)).is_err());
/// assert!(looped.with_crossfade(Samples::new(301)).is_err());
/// # Ok::<(), tutti_sampler::LoopError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loop {
    start: Samples,
    end: Samples,
    crossfade: Samples,
}

/// Why a [`Loop`] was refused where it was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LoopError {
    /// The span has no frame in it: its end is not after its start.
    #[error("a loop needs its end after its start, not {start}..{end}")]
    EmptySpan {
        /// The span's start, as asked.
        start: Samples,
        /// The span's end, as asked.
        end: Samples,
    },
    /// The crossfade is longer than the loop it leads into.
    #[error("a crossfade of {crossfade} frames does not fit a loop of {len}")]
    CrossfadeTooLong {
        /// The crossfade asked for.
        crossfade: Samples,
        /// The loop's length.
        len: Samples,
    },
}

impl Loop {
    /// The hard loop over `span` (no crossfade); add one with
    /// [`with_crossfade`](Self::with_crossfade).
    ///
    /// # Errors
    ///
    /// [`LoopError::EmptySpan`] when `span.end` is not after `span.start`.
    pub fn new(span: Range<Samples>) -> Result<Self, LoopError> {
        if span.end <= span.start {
            return Err(LoopError::EmptySpan {
                start: span.start,
                end: span.end,
            });
        }
        Ok(Self {
            start: span.start,
            end: span.end,
            crossfade: Samples::ZERO,
        })
    }

    /// This loop with a `crossfade` of that many frames into it.
    ///
    /// # Errors
    ///
    /// [`LoopError::CrossfadeTooLong`] when the crossfade is longer than the
    /// loop.
    pub fn with_crossfade(self, crossfade: Samples) -> Result<Self, LoopError> {
        if crossfade > self.len() {
            return Err(LoopError::CrossfadeTooLong {
                crossfade,
                len: self.len(),
            });
        }
        Ok(Self { crossfade, ..self })
    }

    /// The first frame of the loop.
    #[must_use]
    pub fn start(self) -> Samples {
        self.start
    }

    /// One past the loop's last frame.
    #[must_use]
    pub fn end(self) -> Samples {
        self.end
    }

    /// The loop's length in frames.
    #[must_use]
    pub fn len(self) -> Samples {
        Samples::new(self.end.get() - self.start.get())
    }

    /// The crossfade into the loop, as asked; zero is a hard loop.
    #[must_use]
    pub fn crossfade(self) -> Samples {
        self.crossfade
    }

    /// The same loop with no crossfade.
    #[must_use]
    pub(crate) fn hard(self) -> Self {
        Self {
            crossfade: Samples::ZERO,
            ..self
        }
    }

    /// This loop read in a file `len` frames long: `None` when it starts at
    /// or past the file's end.
    #[must_use]
    pub(crate) fn bound(self, len: usize) -> Option<LoopSpan> {
        LoopSpan::new(self.start.get(), self.end.get(), self.crossfade.get(), len)
    }
}

/// A loop over whole file frames `[start, end)`, with a crossfade of `fade`
/// frames into it, wrapping to `resume`. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoopSpan {
    start: usize,
    end: usize,
    /// The crossfade in use: the length asked for, clamped to the loop (to
    /// half of it when fading into the head).
    fade: usize,
    /// Where the wrap lands and what the fade leads into: `start`, or `start +
    /// fade` when there is too little before `start` for a lead-in.
    resume: usize,
}

/// One of the four frames the kernel interpolates a looped position from: the
/// file frame it reads, and, inside the crossfade, the lead-in frame it blends
/// toward and the gains of the tail and the lead-in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LoopTap<F> {
    pub(crate) frame: usize,
    pub(crate) fade: Option<(usize, F, F)>,
}

impl LoopSpan {
    /// The loop `[start, end)` in whole file frames of a file `len` frames
    /// long (`end` is clamped to it), fading over `crossfade_frames` (see the
    /// module docs for the two modes). `None` for a range with nothing in it,
    /// which does not loop.
    pub(crate) fn new(
        start: usize,
        end: usize,
        crossfade_frames: usize,
        len: usize,
    ) -> Option<Self> {
        Self::unbounded(start, end.min(len), crossfade_frames)
    }

    /// [`new`](Self::new) for a file whose length is not known: `end` is
    /// taken as it is.
    pub(crate) fn unbounded(start: usize, end: usize, crossfade_frames: usize) -> Option<Self> {
        if end <= start {
            return None;
        }
        let loop_len = end - start;
        let (fade, resume) = if crossfade_frames <= start {
            // A lead-in exists: fade toward what leads into `start`.
            (crossfade_frames.min(loop_len), start)
        } else {
            // Too little before `start`: fade into the head, resume after it.
            let fade = crossfade_frames.min(loop_len / 2);
            (fade, start + fade)
        };
        Some(Self {
            start,
            end,
            fade,
            resume,
        })
    }

    /// The loop `setting` (if any) reads as in a file `len` frames long.
    pub(crate) fn from_setting(setting: Option<Loop>, len: usize) -> Option<Self> {
        setting.and_then(|l| l.bound(len))
    }

    /// One past the loop's last frame (clamped to the file).
    pub(crate) fn end(&self) -> usize {
        self.end
    }

    /// Where a wrap lands: `start`, or `start + fade` when the fade is into the
    /// loop's head.
    pub(crate) fn resume(&self) -> usize {
        self.resume
    }

    /// The crossfade length in use, after the clamp.
    pub(crate) fn fade(&self) -> usize {
        self.fade
    }

    /// Place a forward read at `pos` (file frames, counted as if the file
    /// played straight on) on the loop: the position it plays, which once it
    /// has reached `end` repeats `[resume, end)`, and whether it has been round.
    ///
    /// Modulo, so an overshoot longer than the loop still lands inside it.
    pub(crate) fn place(&self, pos: f64) -> (f64, bool) {
        let (resume, end) = (self.resume as f64, self.end as f64);
        if !self.wrapped(pos) {
            return (pos, false);
        }
        (resume + (pos - end).rem_euclid(end - resume), true)
    }

    /// Whether [`place`](Self::place) puts a read at `pos` past the loop's
    /// first pass (its second value), without placing it.
    #[inline]
    pub(crate) fn wrapped(&self, pos: f64) -> bool {
        // Not "below the end", so a NaN position counts as wrapped, as
        // `place` has always placed it.
        !matches!(pos.partial_cmp(&(self.end as f64)), Some(Ordering::Less))
    }

    /// [`place`](Self::place) for a whole frame: the file frame a forward
    /// stream at `pos` (counted as if the file played straight on) plays.
    ///
    /// The streamer's refill writes a looped stream's ring frame by frame
    /// through this, so the ring carries the sequence every other tier reads
    /// (`streamer::loops::Mapping::fill`).
    #[inline]
    pub(crate) fn place_frame(&self, pos: usize) -> usize {
        if pos < self.end {
            return pos;
        }
        self.resume + (pos - self.end) % (self.end - self.resume)
    }

    /// The crossfade at file frame `frame`: the lead-in frame it blends toward
    /// (one of the `fade` frames leading into `resume`) and the gains
    /// `(out, in)` of the tail and the lead-in, or `None` outside the fade.
    /// The gains are [`FadeLaw::EqualPower`]'s at frame `k` of the `fade`-frame
    /// fade, narrowed to `F` once.
    #[inline]
    pub(crate) fn fade_at<F: Float>(&self, frame: usize) -> Option<(usize, F, F)> {
        let fade_start = self.end - self.fade;
        if frame < fade_start || frame >= self.end {
            return None;
        }
        let k = frame - fade_start;
        let (out, lead) = FadeLaw::EqualPower.gains(Samples::new(k), Samples::new(self.fade));
        Some((
            self.resume - self.fade + k,
            F::from_unit(out),
            F::from_unit(lead),
        ))
    }

    /// The four frames of the looped sequence the kernel interpolates `pos`
    /// from, and the fraction between the second and third, in a file `len`
    /// frames long (non-zero).
    ///
    /// `pos` is placed on the loop already (below `end`; see
    /// [`place`](Self::place)), and `looped` says whether the read has been
    /// round it. Ahead, a tap at or past `end` wraps through `resume`. Behind,
    /// a tap before `resume` is the loop's last frames — but only when the
    /// position itself is on the repeating loop (at or after `resume`) and has
    /// been round it: a position before `resume` reads the file behind it,
    /// whatever `looped` says (a loop moved under a cursor that had been round
    /// the old one leaves it there). Each tap is clamped to the file, as
    /// [`tap_indices`](super::interp::tap_indices) clamps an unlooped read.
    #[inline]
    pub(crate) fn taps<F: Float>(
        &self,
        len: usize,
        pos: SamplePosition,
        looped: bool,
    ) -> ([LoopTap<F>; 4], F) {
        let (idx, frac) = super::interp::split_position(pos);
        (self.frame_taps(len, idx, looped), frac)
    }

    /// Whether [`frame_taps`](Self::frame_taps) for whole frame `idx` (of a
    /// file at least [`end`](Self::end) long) are the file's own
    /// `idx - 1 ..= idx + 2`, unclamped and outside the fade: the frame is
    /// past the file's first, its taps short of the fade (and of the wrap
    /// behind it), and not on `resume` after a wrap (where the frame behind
    /// is the loop's last). Then a looped read takes the taps an unlooped
    /// one does.
    #[inline]
    pub(crate) fn plain(&self, idx: usize, looped: bool) -> bool {
        idx >= 1 && idx + 2 < self.end - self.fade && !(looped && idx == self.resume)
    }

    /// [`taps`](Self::taps) for whole frame `idx` (the position split by
    /// `interp::split_position`): the four frames, without the fraction.
    #[inline]
    pub(crate) fn frame_taps<F: Float>(
        &self,
        len: usize,
        idx: usize,
        looped: bool,
    ) -> [LoopTap<F>; 4] {
        let cycle = self.end - self.resume;
        let last = len - 1;
        let back_wraps = looped && idx >= self.resume;
        let tap = |offset: isize| {
            let mut at = idx as isize + offset;
            if back_wraps && at < self.resume as isize {
                at += cycle as isize;
            }
            let mut at = at.max(0) as usize;
            if at >= self.end {
                at = self.resume + (at - self.end) % cycle;
            }
            let frame = at.min(last);
            LoopTap {
                frame,
                // A lead-in is before `resume`, so inside the file whenever the
                // tail is.
                fade: self.fade_at(frame),
            }
        };
        [tap(-1), tap(0), tap(1), tap(2)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `plain` is exactly the frames whose looped taps are the file's
    /// own `idx - 1 ..= idx + 2`, none in the fade: a looped block read
    /// takes those four straight from the wave, so a `plain` frame whose
    /// taps wrap or fade would read the wrong frames, and one missed is only
    /// slower. Exhaustive over small loops in both fade modes, before and
    /// after the first wrap, in a file that ends at the loop or past it,
    /// every frame of the loop from 1 (frame 0's tap behind clamps to
    /// itself).
    #[test]
    fn plain_is_exactly_the_unwrapped_unfaded_taps() {
        let mut plain = 0;
        for start in 0..6 {
            for end in start + 1..start + 12 {
                for (fade, len) in (0..6).flat_map(|f| [end, end + 1, end + 3].map(|l| (f, l))) {
                    let Some(span) = LoopSpan::new(start, end, fade, len) else {
                        continue;
                    };
                    for looped in [false, true] {
                        for idx in 1..end {
                            let own = span
                                .frame_taps::<f32>(len, idx, looped)
                                .iter()
                                .enumerate()
                                .all(|(k, tap)| tap.frame + 1 == idx + k && tap.fade.is_none());
                            assert_eq!(
                                span.plain(idx, looped),
                                own,
                                "[{start}, {end}) of {len}, fade {fade}, frame {idx}, looped {looped}"
                            );
                            plain += usize::from(own);
                        }
                    }
                }
            }
        }
        assert!(plain > 0, "some frames are plain");
    }

    /// The closed form of frame `k` of an `n`-frame fade's gains: the
    /// smootherstep `s = x³(6x² − 15x + 10)` of `x = (k + 1) / (n + 1)`, and
    /// `(cos(πs/2), sin(πs/2))`.
    fn closed_gains(k: usize, n: usize) -> (f64, f64) {
        let x = (k + 1) as f64 / (n + 1) as f64;
        let s = x.powi(3) * (6.0 * x * x - 15.0 * x + 10.0);
        let a = s * std::f64::consts::FRAC_PI_2;
        (a.cos(), a.sin())
    }

    /// The fade leads into `start`. A 4-frame fade on `[10, 20)` blends
    /// frames 16..20 toward 6..10 at the equal-power gains of positions
    /// 1/5 … 4/5, and nothing else; the wrap lands on 10.
    #[test]
    fn the_fade_blends_the_tail_toward_what_leads_into_start() {
        let span = LoopSpan::unbounded(10, 20, 4).expect("a loop");
        assert_eq!(span.fade_at::<f64>(15), None);
        for k in 0..4 {
            let (lead, out, inn) = span.fade_at::<f64>(16 + k).expect("inside the fade");
            let (want_out, want_in) = closed_gains(k, 4);
            assert_eq!(lead, 6 + k);
            assert!((out - want_out).abs() <= 1e-12, "out at {k}: {out}");
            assert!((inn - want_in).abs() <= 1e-12, "in at {k}: {inn}");
        }
        assert_eq!(span.fade_at::<f64>(20), None);
        assert_eq!(span.resume(), 10);
        assert_eq!(span.place(20.0), (10.0, true));
    }

    /// The seam's gains are equal-power: `out² + in² = 1` at every frame of a
    /// 7-frame fade, and the lead-in's gain rises strictly while the tail's
    /// falls.
    #[test]
    fn a_fades_gains_are_equal_power_and_the_lead_in_rises_strictly() {
        let span = LoopSpan::unbounded(20, 40, 7).expect("a loop");
        let mut previous = (1.0, 0.0);
        for frame in 33..40 {
            let (_, out, inn) = span.fade_at::<f64>(frame).expect("inside the fade");
            assert!(
                (out * out + inn * inn - 1.0).abs() <= 1e-12,
                "frame {frame}"
            );
            assert!(inn > previous.1 && out < previous.0, "frame {frame}");
            previous = (out, inn);
        }
    }

    /// A long fade's gains are the closed form rounded once to the width: the
    /// `f32` gains are within half a unit in the last place of it (`2^-25` for
    /// a gain in `[0.5, 1)`, `2^-26` below). The fade is 30 million frames, past
    /// `2^24`, where `f32` holds only even counts: a position formed from the
    /// two counts as `f32` would round three times.
    #[test]
    fn a_long_fades_gains_are_their_closed_form_rounded_once() {
        let fade = 30_000_000usize;
        let span = LoopSpan::unbounded(40_000_000, 80_000_000, fade).expect("a loop");
        let k = 25_000_002usize;
        let frame = 80_000_000 - fade + k;
        let (want_out, want_in) = closed_gains(k, fade);
        let (_, out, inn) = span.fade_at::<f32>(frame).expect("inside the fade");
        assert!((f64::from(out) - want_out).abs() <= 2.0_f64.powi(-25));
        assert!((f64::from(inn) - want_in).abs() <= 2.0_f64.powi(-25));
    }

    /// With too little before `start`, the fade goes into the loop's head
    /// and the wrap resumes after it. A 4-frame fade on `[2, 20)` (two
    /// frames before the start) blends frames 16..20 toward the head 2..6 and
    /// wraps to 6, so the last blended frame (almost all 5) is followed by 6.
    /// A loop from frame 0 fades too; the fade is at most half the loop there.
    #[test]
    fn with_too_little_before_start_the_fade_goes_into_the_head() {
        let span = LoopSpan::unbounded(2, 20, 4).expect("a loop");
        assert_eq!((span.fade(), span.resume()), (4, 6));
        assert_eq!(span.fade_at::<f64>(16).map(|(lead, ..)| lead), Some(2));
        assert_eq!(span.fade_at::<f64>(19).map(|(lead, ..)| lead), Some(5));
        assert_eq!(span.place(20.0), (6.0, true));
        assert_eq!(span.place(34.0), (6.0, true), "the loop repeats [6, 20)");
        // The whole-frame twin the streamer writes its ring by agrees.
        for pos in [0, 5, 19, 20, 33, 34, 47, 1_000] {
            assert_eq!(span.place_frame(pos) as f64, span.place(pos as f64).0);
        }
        let whole = LoopSpan::unbounded(0, 1_000, 256).expect("a loop");
        assert_eq!((whole.fade(), whole.resume()), (256, 256));
        let short = LoopSpan::unbounded(0, 100, 256).expect("a loop");
        assert_eq!((short.fade(), short.resume()), (50, 50));
    }

    /// A fade is clamped to the loop, and the loop to the file.
    #[test]
    fn a_fade_is_clamped_to_its_loop_and_the_loop_to_its_file() {
        assert_eq!(
            LoopSpan::unbounded(500, 520, 100).map(|s| s.fade()),
            Some(20)
        );
        assert_eq!(LoopSpan::unbounded(20, 20, 4), None);
        let clamped = LoopSpan::new(100, 5_000, 0, 1_000).expect("a loop");
        assert_eq!(clamped.end(), 1_000);
        assert_eq!(LoopSpan::new(2_000, 5_000, 0, 1_000), None);
    }

    /// A position before the loop reads the file behind it, even from a
    /// cursor that has been round a loop — the cursor a loop change leaves
    /// behind the new loop's start. Every tap of 1 500.5 is the file's own
    /// 1 499..1 502 on a loop `[2000, 4000)`, looped or not.
    #[test]
    fn a_position_before_the_loop_reads_the_file_behind_it() {
        let span = LoopSpan::unbounded(2_000, 4_000, 0).expect("a loop");
        for looped in [false, true] {
            let (taps, _) = span.taps::<f32>(10_000, SamplePosition::new(1_500.5), looped);
            assert_eq!(taps.map(|t| t.frame), [1_499, 1_500, 1_501, 1_502]);
        }
    }

    /// Taps wrap through the loop: ahead of a position near `end` they read
    /// `start`, `start + 1`; behind one on `start`, after a wrap, `end - 1`.
    #[test]
    fn taps_wrap_through_the_loop() {
        let span = LoopSpan::unbounded(10, 20, 0).expect("a loop");
        let frames = |pos, looped| {
            let (taps, _) = span.taps::<f32>(100, SamplePosition::new(pos), looped);
            taps.map(|t| t.frame)
        };
        assert_eq!(frames(18.5, false), [17, 18, 19, 10]);
        assert_eq!(frames(19.5, false), [18, 19, 10, 11]);
        assert_eq!(frames(10.5, true), [19, 10, 11, 12]);
        // The first pass reaches `start` from the file before it.
        assert_eq!(frames(10.5, false), [9, 10, 11, 12]);
        assert_eq!(span.place(25.0), (15.0, true));
        assert_eq!(span.place(19.0), (19.0, false));
    }
}
