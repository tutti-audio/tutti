//! The gain stage after the fold: each device output ramps to the gain the
//! control side published, over the declick length, never by a swap.

use crate::routing::StageTargets;
use tutti_buffers::Float;
use tutti_buffers::MAX_CHANNELS;
use tutti_graph::ramp_in;
use tutti_units::{Amplitude, Samples};

/// One output's ramp: from the gain it left off at to the target, `done` of
/// `frames` frames along.
#[derive(Clone, Copy)]
struct Ramp {
    from: Amplitude,
    to: Amplitude,
    now: Amplitude,
    done: usize,
    /// The length this ramp was started with: a ramp keeps its own, so a
    /// declick length that changes (a re-prepare at another rate) neither
    /// restarts a finished ramp nor stretches one in flight.
    frames: usize,
}

impl Ramp {
    const STEADY: Self = Self {
        from: Amplitude::UNITY,
        to: Amplitude::UNITY,
        now: Amplitude::UNITY,
        done: 0,
        frames: 0,
    };
}

/// The audio thread's side of the output gains: where each output's gain
/// stands. Fixed size, so applying it never allocates.
pub(crate) struct OutputStage {
    ramps: [Ramp; MAX_CHANNELS],
}

impl OutputStage {
    pub(crate) const fn new() -> Self {
        Self {
            ramps: [Ramp::STEADY; MAX_CHANNELS],
        }
    }

    /// Applies the gains to `block`, interleaved `out_ch` wide. An output whose
    /// target changed starts a ramp of `frames` frames (the declick length now)
    /// from where it stands, so a change in mid-ramp turns smoothly; an output
    /// at unity and steady is not touched.
    pub(crate) fn apply<F: Float>(
        &mut self,
        block: &mut [F],
        out_ch: usize,
        targets: &StageTargets,
        frames: usize,
    ) {
        for (c, ramp) in self.ramps.iter_mut().enumerate().take(out_ch) {
            let to = targets.gain(c);
            // Bit equality: a NaN target equals itself here, so it does not
            // restart the ramp every block, and `-0.0` stays apart from `0.0`.
            if to.get().to_bits() != ramp.to.get().to_bits() {
                *ramp = Ramp {
                    from: ramp.now,
                    to,
                    now: ramp.now,
                    done: 0,
                    frames: frames.max(1),
                };
            }
            if ramp.done >= ramp.frames {
                if ramp.now != Amplitude::UNITY {
                    let gain = F::from_unit(ramp.now);
                    block
                        .iter_mut()
                        .skip(c)
                        .step_by(out_ch)
                        .for_each(|s| *s *= gain);
                }
                continue;
            }
            // The ramp renders in `F`, as an edge weight's does; the gain it
            // ends the block on is where the next target starts from.
            let (from, to) = (F::from_unit(ramp.from), F::from_unit(ramp.to));
            let mut last = None;
            for s in block.iter_mut().skip(c).step_by(out_ch) {
                let gain = ramp_in(
                    None,
                    from,
                    to,
                    Samples::new(ramp.done),
                    Samples::new(ramp.frames),
                );
                if ramp.done < ramp.frames {
                    ramp.done += 1;
                }
                *s *= gain;
                last = Some(gain);
            }
            if let Some(gain) = last {
                ramp.now = Amplitude::new(gain.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_units::Amplitude;

    fn targets(first: f64) -> StageTargets {
        let mut t = StageTargets::UNITY;
        t.0[0] = Amplitude::new(first);
        t
    }

    /// A finished ramp is steady at its target whatever length the next call
    /// carries: a declick length that grew (a re-prepare at a higher rate)
    /// does not restart it, and one in flight keeps the length it began with.
    #[test]
    fn a_ramp_keeps_its_own_length_across_a_rate_change() {
        let mut stage = OutputStage::new();
        let to_half = targets(0.5);
        // A 100-frame ramp, run to its end across two calls.
        let mut a = [1.0f32; 60];
        stage.apply(&mut a, 1, &to_half, 100);
        let mut b = [1.0f32; 60];
        stage.apply(&mut b, 1, &to_half, 100);
        assert_eq!(b[39], 0.5, "the ramp ended at frame 100");
        // The next block comes at a rate whose declick is ten times longer: the
        // finished ramp stays at its target, exactly.
        let mut after = [1.0f32; 32];
        stage.apply(&mut after, 1, &to_half, 1_000);
        assert!(after.iter().all(|&s| s == 0.5), "{after:?}");

        // A ramp in flight keeps the length it began with.
        let mut stage = OutputStage::new();
        let mut first = [1.0f32; 10];
        stage.apply(&mut first, 1, &to_half, 20);
        let mut rest = [1.0f32; 10];
        stage.apply(&mut rest, 1, &to_half, 2_000);
        assert_eq!(
            rest[9], 0.5,
            "ended at its own 20th frame, not the new 2000"
        );
        let line = |k: usize| 1.0 + (0.5 - 1.0) * ((k + 1) as f32 / 20.0);
        assert_eq!(first[0], line(0));
        assert_eq!(rest[0], line(10));
    }

    /// A NaN target is the same target on the next block: the ramp to it runs
    /// its length once and is not restarted every block.
    #[test]
    fn a_nan_target_does_not_restart_its_ramp_every_block() {
        let mut stage = OutputStage::new();
        let to_nan = targets(f64::NAN);
        let mut a = [1.0f32; 4];
        stage.apply(&mut a, 1, &to_nan, 8);
        assert_eq!(stage.ramps[0].done, 4);
        let mut b = [1.0f32; 4];
        stage.apply(&mut b, 1, &to_nan, 8);
        assert_eq!(
            stage.ramps[0].done, 8,
            "the second block continued the ramp"
        );
    }
}
