//! The gain stage after the fold: each device output ramps to the gain the
//! control side published, over the declick length, never by a swap.

use crate::routing::StageTargets;
use tutti_types::MAX_CHANNELS;

/// One output's ramp: from the gain it left off at to the target, `done` of
/// `frames` frames along.
#[derive(Clone, Copy)]
struct Ramp {
    from: f32,
    to: f32,
    now: f32,
    done: usize,
}

impl Ramp {
    const STEADY: Self = Self {
        from: 1.0,
        to: 1.0,
        now: 1.0,
        done: usize::MAX,
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
    /// target changed starts a ramp of `frames` frames from where it stands, so
    /// a change in mid-ramp turns smoothly; an output at unity and steady is
    /// not touched.
    pub(crate) fn apply(
        &mut self,
        block: &mut [f32],
        out_ch: usize,
        targets: &StageTargets,
        frames: usize,
    ) {
        let frames = frames.max(1);
        for (c, ramp) in self.ramps.iter_mut().enumerate().take(out_ch) {
            let to = targets.gain(c).get();
            if to.to_bits() != ramp.to.to_bits() {
                *ramp = Ramp {
                    from: ramp.now,
                    to,
                    now: ramp.now,
                    done: 0,
                };
            }
            if ramp.done >= frames {
                if ramp.now != 1.0 {
                    block
                        .iter_mut()
                        .skip(c)
                        .step_by(out_ch)
                        .for_each(|s| *s *= ramp.now);
                }
                continue;
            }
            for s in block.iter_mut().skip(c).step_by(out_ch) {
                ramp.now = if ramp.done + 1 >= frames {
                    ramp.to
                } else {
                    ramp.from + (ramp.to - ramp.from) * ((ramp.done + 1) as f32 / frames as f32)
                };
                if ramp.done < frames {
                    ramp.done += 1;
                }
                *s *= ramp.now;
            }
        }
    }
}
