//! The walk a beat-scheduled event source makes through each block's
//! transport: which of its sorted events playback reaches, on which frame,
//! and where playback jumps. Shared by [`MidiClipNode`](super::MidiClipNode)
//! and [`HarmonyNode`](super::HarmonyNode).
//!
//! It keeps **no play cursor**. Which events fall in a segment is a binary
//! search over the sorted events and the engine's one beat→frame rule
//! ([`Env::due`]), so a seek, a loop wrap or a tempo change inside a block
//! needs no bookkeeping, and an offline fork walks its render's `Env`. What it
//! keeps is where the next segment starts if playback is continuous, which is
//! how it tells a jump (a start, a seek, a loop wrap, a restart) from the
//! next block of the same run, and whether the run was stopped, which is how
//! it tells a start from a seek or a loop wrap.

use tutti_graph::{Due, Env, Offset};
use tutti_music::{FRAME_TOLERANCE, first_frame_at_or_after};
use tutti_units::{At, Beat, FramesPerBeat, Samples};

/// An event scheduled at a beat.
pub(crate) trait Beated {
    /// The beat it is scheduled at.
    fn beat(&self) -> Beat;
}

/// What a source does as the walk goes: [`Walk::segment`] calls these in
/// frame order.
pub(crate) trait Visit<T> {
    /// Playback is stopped from `at` (or has no usable tempo).
    fn stop(&mut self, at: Option<Offset>);
    /// Playback jumped to `beat` at `at`: a seek, a loop wrap, or (unless
    /// [`start`](Self::start) is overridden) a start. `events` are the
    /// source's, and `frames_per_beat` the rate the walk places them at
    /// ([`lands_on_first_frame`]).
    fn jump(
        &mut self,
        at: Option<Offset>,
        beat: Beat,
        frames_per_beat: FramesPerBeat,
        events: &[T],
    );
    /// Playback starts at `beat` at `at`: it was stopped (or the walk was
    /// told to [`forget`](Walk::forget) where it was), or its receiver needs
    /// its setup again ([`Walk::resend`]) at a jump. A jump the source may
    /// treat as any other
    /// ([`jump`](Self::jump), the default).
    fn start(
        &mut self,
        at: Option<Offset>,
        beat: Beat,
        frames_per_beat: FramesPerBeat,
        events: &[T],
    ) {
        self.jump(at, beat, frames_per_beat, events);
    }
    /// Playback goes on from `beat` at `at`, and its receiver needs its setup
    /// again ([`Walk::resend`]). Nothing by default.
    fn resend(
        &mut self,
        _at: Option<Offset>,
        _beat: Beat,
        _frames_per_beat: FramesPerBeat,
        _events: &[T],
    ) {
    }
    /// Playback wrapped its loop inside a segment: the pass starts at `at`,
    /// on `beat` (the loop's start, or a hair past it). A jump the source may
    /// treat as one ([`jump`](Self::jump), the default) or as the next pass
    /// of the same run.
    fn wrap(
        &mut self,
        at: Option<Offset>,
        beat: Beat,
        frames_per_beat: FramesPerBeat,
        events: &[T],
    ) {
        self.jump(at, beat, frames_per_beat, events);
    }
    /// `event` is due at `at`.
    fn event(&mut self, at: Offset, event: &T);
}

/// Where playback was.
#[derive(Clone, Copy, Debug, Default)]
enum Run {
    /// Stopped, before the first block, or after [`Walk::forget`]: the next
    /// playing segment is a start.
    #[default]
    Stopped,
    /// A loop wrapped on the last segment's edge: the next segment is the
    /// wrap's jump.
    Wrapped,
    /// Playing: the beat the next segment starts on if it is continuous.
    At(Beat),
}

/// Where playback was, and whether its receiver is owed its setup again. See
/// the module docs.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    run: Run,
    /// [`resend`](Self::resend) was called and no segment has played since.
    resend: bool,
}

impl Walk {
    /// Make the next segment read as a start (its source's events changed).
    pub(crate) fn forget(&mut self) {
        self.run = Run::Stopped;
        self.resend = false;
    }

    /// The receiver changed: the next playing segment gives it its setup
    /// again ([`Visit::resend`], or [`Visit::start`] if that segment jumps).
    pub(crate) fn resend(&mut self) {
        self.resend = true;
    }

    /// Walk `seg`, the block's piece from `start`: every event of `events`
    /// (sorted by beat) that playback reaches in it, and where it stops or
    /// jumps.
    pub(crate) fn segment<T: Beated>(
        &mut self,
        events: &[T],
        block: &Env,
        start: Offset,
        seg: &Env,
        v: &mut impl Visit<T>,
    ) {
        let t = seg.transport;
        let len = seg.block_len.get();
        let at = |k: usize| Offset::new(Samples::new(start.index() + k), block.block_len);
        let Some(fpb) = t
            .tempo
            .frames_per_beat(seg.sample_rate)
            .filter(|_| t.playing)
        else {
            v.stop(at(0));
            self.run = Run::Stopped;
            return;
        };
        let now = t.beat();
        let frame = fpb.beats_per_frame();
        // A jump is a start more than half a frame away from where the last
        // segment left off (a seek, a wrap at a block edge, a restart).
        let continuous = matches!(self.run, Run::At(e) if fpb.frames_in(now - e).abs() < 0.5);
        let resend = std::mem::take(&mut self.resend);
        if continuous {
            if resend {
                v.resend(at(0), now, fpb, events);
            }
        } else if resend || matches!(self.run, Run::Stopped) {
            v.start(at(0), now, fpb, events);
        } else {
            v.jump(at(0), now, fpb, events);
        }

        // The host's own clock walks the segment (`Transport::clock`, the
        // `FrameClock` its transport is counted on): each loop wrap is on
        // the frame the clock wraps on, at the beat it wraps to, and where
        // the segment ends is where the clock does. A wrap is at least a
        // frame after the last, so the walk is at most one pass a frame
        // however short the loop.
        let looping = t.looping.filter(|l| now < l.end());
        let mut clock = t.clock(seg.sample_rate);
        let first = clock.advance_to_wrap(Samples::new(len), looping);
        let wrap = first.map_or(len, Samples::get);

        // Up to the wrap: a beat less than a frame behind the playhead falls
        // on the first frame (`Env::due`), so the range starts a frame back.
        let hi = now + frame * (wrap as f64 + 1.0);
        emit(events, now - frame, hi, seg, v, &at, |k| k < wrap);

        // Whether the loop wraps exactly at this segment's end: the next
        // segment then starts on the loop's start, which reads as
        // continuous, so the jump a wrap is is made there (`Run::Wrapped`
        // below).
        let mut wraps_at_end = wrap == len && first.is_some();
        if let (Some(l), true) = (looping, wrap < len) {
            let in_loop = {
                let lo = events.partition_point(|e| e.beat() < l.start());
                let hi = events.partition_point(|e| e.beat() < l.end());
                &events[lo..hi.max(lo)]
            };
            let mut k = wrap;
            while k < len {
                // The clock is on pass `k`'s first frame, at the beat it
                // wrapped to; the pass runs to the next wrap.
                let from = clock.beat();
                let wrapped = clock.advance_to_wrap(Samples::new(len - k), Some(l));
                let next = wrapped.map_or(len, |r| k + r.get());
                v.wrap(at(k), from, fpb, events);
                pass(in_loop, from, k, next, fpb, v, &at);
                wraps_at_end = wrapped.is_some() && next == len;
                k = next;
            }
        }

        // Where the next segment starts if nothing jumps.
        self.run = if wraps_at_end {
            Run::Wrapped
        } else {
            Run::At(clock.beat())
        };
    }
}

/// Whether an event at `beat` that playback reaches lands on the frame it
/// is at `from` (or past), where a jump or a pass starts: any beat before
/// `from` is behind it, and one ahead lands there within
/// [`FRAME_TOLERANCE`] (`first_frame_at_or_after`). Of the events behind,
/// the walk visits on that frame only those less than a frame behind; the
/// rest were before playback got there.
pub(crate) fn lands_on_first_frame(beat: Beat, from: Beat, frames_per_beat: FramesPerBeat) -> bool {
    frames_per_beat.frames_in(beat - from) <= FRAME_TOLERANCE
}

/// Visit the events of one loop pass that playback reaches on its frames
/// `[k, next)`: the pass starts on frame `k` at beat `from`. A beat less than
/// a frame behind `from` (crossed as the clock wrapped) falls on frame `k`,
/// as `Env::due` places one behind a block's first frame; the rest on the
/// first frame at or after them (`first_frame_at_or_after`).
fn pass<T: Beated>(
    in_loop: &[T],
    from: Beat,
    k: usize,
    next: usize,
    fpb: FramesPerBeat,
    v: &mut impl Visit<T>,
    at: &impl Fn(usize) -> Option<Offset>,
) {
    let frame = fpb.beats_per_frame();
    let lo = from - frame;
    let hi = from + frame * ((next - k) as f64 + 1.0);
    let first = in_loop.partition_point(|e| e.beat() < lo);
    for e in &in_loop[first..] {
        if e.beat() >= hi {
            break;
        }
        let ahead = fpb.frames_in(e.beat() - from);
        let d = if ahead < 0.0 {
            if -ahead < 1.0 - FRAME_TOLERANCE {
                0
            } else {
                continue;
            }
        } else {
            first_frame_at_or_after(ahead).max(0) as usize
        };
        if k + d < next
            && let Some(o) = at(k + d)
        {
            v.event(o, e);
        }
    }
}

/// Visit every event with a beat in `[lo, hi)` that playback reaches in `seg`
/// at an offset `keep` accepts.
fn emit<T: Beated>(
    events: &[T],
    lo: Beat,
    hi: Beat,
    seg: &Env,
    v: &mut impl Visit<T>,
    at: &impl Fn(usize) -> Option<Offset>,
    keep: impl Fn(usize) -> bool,
) {
    let first = events.partition_point(|e| e.beat() < lo);
    for e in &events[first..] {
        if e.beat() >= hi || e.beat().get().is_nan() {
            break;
        }
        let Due::In(k) = seg.due(At::Beat(e.beat())) else {
            continue;
        };
        if !keep(k.index()) {
            continue;
        }
        if let Some(offset) = at(k.index()) {
            v.event(offset, e);
        }
    }
}
