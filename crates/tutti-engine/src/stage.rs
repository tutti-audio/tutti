//! The gain stage after the fold: each device output ramps to the gain the
//! control side published, over the declick length, never by a swap.

use crate::routing::StageTargets;
use tutti_buffers::Float;
use tutti_types::MAX_CHANNELS;

/// One output's ramp: from the gain it left off at to the target, `done` of
/// `frames` frames along.
#[derive(Clone, Copy)]
struct Ramp {
    from: f32,
    to: f32,
    now: f32,
    done: usize,
    /// The length this ramp was started with: a ramp keeps its own, so a
    /// declick length that changes (a re-prepare at another rate) neither
    /// restarts a finished ramp nor stretches one in flight.
    frames: usize,
}

impl Ramp {
    const STEADY: Self = Self {
        from: 1.0,
        to: 1.0,
        now: 1.0,
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
            let to = targets.gain(c).get();
            if to.to_bits() != ramp.to.to_bits() {
                *ramp = Ramp {
                    from: ramp.now,
                    to,
                    now: ramp.now,
                    done: 0,
                    frames: frames.max(1),
                };
            }
            if ramp.done >= ramp.frames {
                if ramp.now != 1.0 {
                    // The ramp's gain is an `f32` level (an `Amplitude`).
                    block
                        .iter_mut()
                        .skip(c)
                        .step_by(out_ch)
                        .for_each(|s| *s *= F::from_f32(ramp.now));
                }
                continue;
            }
            for s in block.iter_mut().skip(c).step_by(out_ch) {
                ramp.now = if ramp.done + 1 >= ramp.frames {
                    ramp.to
                } else {
                    ramp.from
                        + (ramp.to - ramp.from) * ((ramp.done + 1) as f32 / ramp.frames as f32)
                };
                if ramp.done < ramp.frames {
                    ramp.done += 1;
                }
                *s *= F::from_f32(ramp.now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_units::Amplitude;

    fn targets(first: f32) -> StageTargets {
        let mut t = StageTargets::UNITY;
        t.0[0] = Amplitude(first);
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
}
