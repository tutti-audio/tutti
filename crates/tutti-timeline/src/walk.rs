//! The tempo walk: a [`FrameClock`] that follows a
//! [`TempoMap`] that changes tempo, as one pure function of the map, the
//! grid, the rate and the frame.
//!
//! The live [`TransportClock`](super::clock::TransportClock) and the
//! [`OfflineTimeline`](super::OfflineTimeline) both call it, so a live render
//! and an offline one of the same map cannot disagree. A constant map never
//! reaches it: the clocks run their own closed form, untouched, and a
//! constant-tempo render is what it always was, bit for bit.
//!
//! The walk keeps one number, `frame`: the map frame the playhead is on. It is
//! set at a seek (`frame_at_or_after` of the exact target beat) and moves with
//! the clock, and it never feeds the beat; the beat is the clock's, derived in
//! closed form from the segment the walk starts. At each cell boundary
//! ([`CELL_GRID`] frames, and where a point of the map starts) it asks
//! `TempoView::chord_cell` for the cell's tempo and, when that differs from the clock's,
//! starts a segment on the map's beat. A step cell carries its segment's tempo
//! to the bit, so a step interior never restarts; there is no hysteresis here
//! (a map is an immutable value, and a tempo that moved by an ulp moved on
//! purpose).

use tutti_graph::{Offset, TransportChanges, TransportPosition};
use tutti_music::{FrameClock, LoopRange};
use tutti_tempo::{CellGrid, TempoMap, TempoView};
use tutti_units::{Beat, Frame, Samples};

/// The one cell grid: a cell is at most this many frames, whatever the block
/// length (one engine constant, never per block).
pub(crate) const CELL_GRID: CellGrid = CellGrid::of(128);

/// The tempo changes inside a block, found ahead of the render
/// ([`TransportAudio::tempo_ahead`](super::TransportAudio::tempo_ahead)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UpcomingTempo {
    /// The changes that fit, in time order (none at offset 0).
    pub changes: TransportChanges,
    /// Where the block must end because the next change does not fit: the
    /// frame it would start on. The change is the next block's first.
    pub limit: Option<usize>,
}

impl UpcomingTempo {
    /// No change, and the block runs whole.
    pub const NONE: Self = Self {
        changes: TransportChanges::NONE,
        limit: None,
    };
}

/// Where a clock is on a map's timeline. `Copy`, so a look ahead walks a copy.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TempoWalk {
    /// The map frame the playhead is on. Negative before beat 0 (a count-in),
    /// where the first point's tempo holds.
    frame: i64,
}

impl TempoWalk {
    /// Seek `clock` to the exact `beat` and take the map's tempo there.
    pub(crate) fn seek(&mut self, map: &TempoMap, clock: &mut FrameClock, beat: Beat) {
        clock.seat(beat);
        self.sync_tempo(map, clock);
    }

    /// Find `clock`'s current beat on `map`'s timeline and take the tempo in
    /// force there, leaving the clock's beat as it is: after a seek, a loop
    /// wrap, a rate change or a map swap. `true` when the tempo moved.
    pub(crate) fn sync_tempo(&mut self, map: &TempoMap, clock: &mut FrameClock) -> bool {
        let rate = clock.sample_rate();
        let beat = clock.beat();
        let mut view = map.view(rate);
        self.frame = if beat.get() < 0.0 {
            view.frames_between(Beat::new(0.0), beat)
        } else {
            view.frame_at_or_after(beat).get() as i64
        };
        self.sync(&mut view, clock, true)
    }

    /// Take the tempo of the cell the playhead is on. `exact`: the clock's beat
    /// was just placed and stays; otherwise the playhead is on a cell's first
    /// frame and the segment starts on the map's beat there.
    fn sync(&mut self, view: &mut TempoView, clock: &mut FrameClock, exact: bool) -> bool {
        let (tempo, beat) = match u64::try_from(self.frame) {
            Ok(f) => {
                let cell = view.chord_cell(Frame::new(f), CELL_GRID);
                (cell.tempo, Some(cell.beat))
            }
            Err(_) => (view.map().points()[0].tempo, None),
        };
        if tempo == clock.tempo() {
            return false;
        }
        if let (false, Some(beat)) = (exact, beat) {
            clock.seat(beat);
        }
        clock.set_tempo(tempo);
        true
    }

    /// Frames from the playhead to the end of its cell (at least 1).
    fn frames_to_edge(&mut self, view: &mut TempoView) -> usize {
        match u64::try_from(self.frame) {
            Ok(f) => {
                let cell = view.chord_cell(Frame::new(f), CELL_GRID);
                (cell.end.get() - f).max(1) as usize
            }
            Err(_) => self.frame.unsigned_abs() as usize,
        }
    }

    /// Advance `clock` `frames` forward, wrapping at `looping`, and following the
    /// map's tempo cell by cell. With `stop`, return right after the first
    /// frame the tempo changed on (the clock standing on it); otherwise advance
    /// all `frames`. Returns the frames advanced and whether the tempo changed.
    pub(crate) fn advance(
        &mut self,
        map: &TempoMap,
        clock: &mut FrameClock,
        frames: usize,
        looping: Option<LoopRange>,
        stop: bool,
    ) -> (usize, bool) {
        let mut view = map.view(clock.sample_rate());
        let (mut advanced, mut changed) = (0, false);
        while advanced < frames {
            let edge = self.frames_to_edge(&mut view);
            let chunk = (frames - advanced).min(edge);
            match clock.advance_to_wrap(Samples::new(chunk), looping) {
                Some(wrapped) => {
                    let w = wrapped.get();
                    advanced += w;
                    self.frame += w as i64;
                    // A wrap is a seek to the loop start: sync the tempo exactly.
                    changed |= self.sync_tempo(map, clock);
                }
                None => {
                    advanced += chunk;
                    self.frame += chunk as i64;
                    if chunk == edge {
                        changed |= self.sync(&mut view, clock, false);
                    }
                }
            }
            if changed && stop {
                break;
            }
        }
        (advanced, changed)
    }

    /// The tempo changes inside the next `frames` of a rolling `clock`, at most
    /// `cap` of them. `transport` builds the transport from the clock after a
    /// change. Walks a copy, with the code [`advance`](Self::advance) runs for real, so
    /// a render that rolls to a change's offset finds the same change there.
    pub(crate) fn ahead(
        &self,
        map: &TempoMap,
        clock: &FrameClock,
        frames: usize,
        looping: Option<LoopRange>,
        cap: usize,
        transport: impl Fn(&FrameClock) -> TransportPosition,
    ) -> UpcomingTempo {
        let (mut walk, mut clock) = (*self, *clock);
        let mut out = UpcomingTempo::NONE;
        let mut at = 0;
        while at < frames {
            let (advanced, changed) = walk.advance(map, &mut clock, frames - at, looping, true);
            at += advanced;
            if !changed || at >= frames {
                break;
            }
            if out.changes.len() >= cap {
                out.limit = Some(at);
                break;
            }
            let offset =
                Offset::new(Samples::new(at), Samples::new(frames)).expect("inside the block");
            let _ = out.changes.push(offset, transport(&clock));
        }
        out
    }
}
