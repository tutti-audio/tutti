//! [`SweepClock`]: the LFO and the segments of the swept effects, one for the phaser and the
//! chorus.
//!
//! **The LFO is state on the stream's clock.** Its phase is a [`RunningPhase`] walked by the
//! rate's step, counted from the last reset. The effect's per-channel value (an all-pass
//! coefficient, a delay) is solved at the **corners** of a segment, every [`COEFF_INTERVAL`]
//! frames of the stream: the segment's start and its end, and each channel's [`Glide`] runs
//! between them, so the cut of the blocks does not move the render. A channel whose offset
//! equals its neighbour's shares its glide. The only thing that differs between effects is
//! the corner: the clock hands the solver the sine of each channel's phase and whether the
//! corner is the segment's end.

use tutti_buffers::Float;
use tutti_dsp::{COEFF_INTERVAL, Glide, Lerp};
use tutti_types::MAX_CHANNELS;
use tutti_units::{Bipolar, Phase, PhaseIncrement, PhaseStep, RunningPhase};

/// The frames of a segment: a corner's glide takes this many steps.
pub(crate) const SEGMENT: u32 = 16;
const _: () = assert!(COEFF_INTERVAL == SEGMENT as usize);

/// The LFO's phase, its rate and where the stream's segments fall.
#[derive(Clone, Debug)]
pub(crate) struct SweepClock {
    /// Each channel's LFO phase offset (channel 0's is zero), wrapped.
    offsets: [PhaseStep; MAX_CHANNELS],
    inc: PhaseStep,
    /// The LFO's phase at the end of the segment in flight, where the corners last solved.
    phase: RunningPhase,
    /// Frames since the segment began, in `0..COEFF_INTERVAL`: counted from the last reset,
    /// so where the segments fall does not depend on how the stream is cut.
    pos: usize,
    /// Whether a segment has been solved since the last reset.
    primed: bool,
    /// Whether the first retarget since a prepare, or since a reset, has landed or a frame has
    /// rendered: what separates a fresh effect's first design (it lands at once) from a move.
    landed: bool,
}

impl SweepClock {
    pub(crate) fn new(offsets: [PhaseIncrement; MAX_CHANNELS]) -> Self {
        Self {
            offsets: offsets.map(PhaseStep::from),
            inc: PhaseStep::ZERO,
            phase: RunningPhase::START,
            pos: 0,
            primed: false,
            landed: false,
        }
    }

    /// The LFO back at its start, no segment solved.
    pub(crate) fn reset(&mut self) {
        self.phase = RunningPhase::START;
        self.pos = 0;
        self.primed = false;
    }

    /// The next design lands whole, as a fresh effect's first does.
    pub(crate) fn unland(&mut self) {
        self.landed = false;
    }

    /// The LFO's step per frame, for the next segment.
    pub(crate) fn set_inc(&mut self, inc: PhaseIncrement) {
        self.inc = PhaseStep::from(inc);
    }

    /// Whether this is the first design since the clock was unlanded: it lands at once.
    pub(crate) fn lands(&mut self) -> bool {
        !std::mem::replace(&mut self.landed, true)
    }

    /// Whether the next frame begins a segment.
    pub(crate) fn at_corner(&self) -> bool {
        self.pos == 0
    }

    /// `frames` rendered: the segment position moves on.
    pub(crate) fn advance(&mut self, frames: usize) {
        self.landed = true;
        self.pos = (self.pos + frames) % COEFF_INTERVAL;
    }

    /// Solves the next segment into `glides`, one per channel: its start corner is the last
    /// one's end (or the LFO's start after a reset), its end corner the sweep
    /// [`COEFF_INTERVAL`] frames on at the LFO's step. `corner(lfo, end)` is the effect's value
    /// with the LFO at `lfo` (a sine, `-1..=1`) at the segment's start (`end` false) or end.
    pub(crate) fn solve<T: Lerp>(
        &mut self,
        glides: &mut [Glide<T>],
        mut corner: impl FnMut(Bipolar, bool) -> T,
    ) {
        let start = self.phase;
        let mut end = start;
        for _ in 0..COEFF_INTERVAL {
            end = end.advance(self.inc);
        }
        // The LFO's sine is solved in `f64`; each effect crosses its corner value once.
        let sine = |offset: PhaseStep, p: RunningPhase| {
            Bipolar::clamped(Phase(f64::from_phase(p.advance(offset))).sin())
        };
        for c in 0..glides.len() {
            if c > 0 && self.offsets[c] == self.offsets[c - 1] {
                glides[c] = glides[c - 1];
                continue;
            }
            let from = if self.primed {
                glides[c].target()
            } else {
                corner(sine(self.offsets[c], start), false)
            };
            let mut glide = Glide::landed(from);
            glide.aim_in(corner(sine(self.offsets[c], end), true), SEGMENT);
            glides[c] = glide;
        }
        self.phase = end;
        self.primed = true;
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> (RunningPhase, usize, bool) {
        (self.phase, self.pos, self.primed)
    }
}
