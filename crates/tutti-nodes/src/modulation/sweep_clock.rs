//! [`SweepClock`]: the LFO and the segments of the swept effects, one for the phaser and the
//! chorus.
//!
//! **The LFO is state on the stream's clock.** Its phase is a [`RunningPhase`] walked by the
//! rate's step, counted from the last reset. The effect's per-channel value (an all-pass
//! coefficient, a delay) is solved at the **corners** of a segment, every [`COEFF_INTERVAL`]
//! frames of the stream: the segment's start and its end, and each channel's [`Ramp`] runs
//! between them, so the cut of the blocks does not move the render. A channel whose offset
//! equals its neighbour's shares its ramp. The only thing that differs between effects is
//! the corner: the clock hands the solver the sine of each channel's phase and whether the
//! corner is the segment's end.

use tutti_buffers::Float;
use tutti_buffers::MAX_CHANNELS;
use tutti_dsp::{COEFF_INTERVAL, Lerp, Ramp};
use tutti_units::{Bipolar, Phase, PhaseIncrement, PhaseStep, RunningPhase};

/// The frames of a segment: a corner's ramp takes this many steps.
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
    /// Whether the next segment awaits its solve: true from a reset until the first
    /// [`solve`](Self::solve), and again whenever the frames rendered end on a corner.
    due: bool,
}

impl SweepClock {
    pub(crate) fn new(offsets: [PhaseIncrement; MAX_CHANNELS]) -> Self {
        Self {
            offsets: offsets.map(PhaseStep::from),
            inc: PhaseStep::ZERO,
            phase: RunningPhase::START,
            pos: 0,
            primed: false,
            due: true,
        }
    }

    /// The LFO back at its start, no segment solved.
    pub(crate) fn reset(&mut self) {
        self.phase = RunningPhase::START;
        self.pos = 0;
        self.primed = false;
        self.due = true;
    }

    /// The LFO's step per frame, for the next segment.
    pub(crate) fn set_inc(&mut self, inc: PhaseIncrement) {
        self.inc = PhaseStep::from(inc);
    }

    /// Whether the next segment awaits its solve: the next frame begins a segment that
    /// [`solve`](Self::solve) has not yet been called for.
    pub(crate) fn due(&self) -> bool {
        self.due
    }

    /// `frames` rendered: the segment position moves on, and the next segment is due when
    /// the frames end on a corner.
    pub(crate) fn advance(&mut self, frames: usize) {
        self.pos = (self.pos + frames) % COEFF_INTERVAL;
        self.due |= frames > 0 && self.pos == 0;
    }

    /// Solves the next segment into `ramps`, one per channel: its start corner is the last
    /// one's end (or the LFO's start after a reset), its end corner the sweep
    /// [`COEFF_INTERVAL`] frames on at the LFO's step. `corner(lfo, end)` is the effect's value
    /// with the LFO at `lfo` (a sine, `-1..=1`) at the segment's start (`end` false) or end.
    pub(crate) fn solve<T: Lerp>(
        &mut self,
        ramps: &mut [Ramp<T>],
        mut corner: impl FnMut(Bipolar, bool) -> T,
    ) {
        let start = self.phase;
        let mut end = start;
        for _ in 0..COEFF_INTERVAL {
            end = end.advance(self.inc);
        }
        // The LFO's sine is solved in `f64`; each effect crosses its corner value once.
        let sine = |offset: PhaseStep, p: RunningPhase| {
            Bipolar::clamped(Phase::new(f64::from_phase(p.advance(offset))).sin())
        };
        for c in 0..ramps.len() {
            if c > 0 && self.offsets[c] == self.offsets[c - 1] {
                ramps[c] = ramps[c - 1];
                continue;
            }
            let from = if self.primed {
                ramps[c].target()
            } else {
                corner(sine(self.offsets[c], start), false)
            };
            let mut ramp = Ramp::new(from);
            ramp.retarget_in(corner(sine(self.offsets[c], end), true), SEGMENT);
            ramps[c] = ramp;
        }
        self.phase = end;
        self.primed = true;
        self.due = false;
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> (RunningPhase, usize, bool, bool) {
        (self.phase, self.pos, self.primed, self.due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The segment is due from a reset until it is solved, and again exactly when the frames
    /// rendered end on a corner: a chunk that ends off the grid, or begins at an off-grid corner
    /// of a driving port and ends before the next grid corner, solves nothing, and a chunk of
    /// no frames at a corner does not make the segment due again.
    #[test]
    fn a_segment_is_due_only_when_the_frames_rendered_end_on_a_corner() {
        let mut clock = SweepClock::new([PhaseIncrement::new(0.0); MAX_CHANNELS]);
        clock.set_inc(PhaseIncrement::new(0.01));
        let mut ramps = [Ramp::new(0.0f32)];
        let solve = |clock: &mut SweepClock, ramps: &mut [Ramp<f32>; 1]| {
            clock.solve(ramps, |lfo, _| lfo.get() as f32);
        };
        assert!(clock.due(), "due from the start");
        solve(&mut clock, &mut ramps);
        assert!(!clock.due());
        let solved = clock.state().0;
        clock.advance(0);
        assert!(!clock.due(), "no frames end nowhere");
        for chunk in [5, 7] {
            clock.advance(chunk);
            assert!(!clock.due(), "a chunk ending off a corner");
            assert_eq!(clock.state().0, solved, "nothing is solved");
        }
        clock.advance(SEGMENT as usize - 12);
        assert!(clock.state().1 == 0 && clock.due(), "the corner is due");
        solve(&mut clock, &mut ramps);
        assert_ne!(
            clock.state().0,
            solved,
            "the corner solved the next segment"
        );
        clock.reset();
        assert!(clock.due(), "due again after a reset");
    }
}
