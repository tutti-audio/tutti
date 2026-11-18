//! [`BrickwallBank`]: the hard clipper over buffers views.
//!
//! A brickwall clamps every sample to `+-c` and has nothing else: no state, no
//! envelope, no lookahead and so no latency and no tail. Its only control is the ceiling,
//! which is read per frame when the graph drives it ([`BrickwallLanes`]); the aimed ceiling
//! is the one a frame nobody drives is clamped at. The clamp bounds the **sample** peak of
//! the output for any finite or infinite input; a NaN input passes as NaN. It is not a
//! true-peak limit.
//!
//! The ceiling is in dB and the clamp bound is its amplitude. `+inf` dB passes
//! everything and `-inf` dB mutes (both are ceilings); only a NaN has no level, and it
//! is ignored wherever it comes from (the node keeps the last ceiling for a NaN
//! cell, and a NaN frame on the lane clamps at the aimed ceiling).

use tutti_buffers::{Float, Mono, Planes, PlanesMut, Sample};
use tutti_units::{Amplitude, Db};

/// The controls of a [`BrickwallBank`]: the ceiling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallControls {
    /// The level samples are clamped to, in dB (`+-inf` are ceilings; a NaN has no
    /// level and falls back to the default, 0 dB).
    pub ceiling: Db,
}

impl BrickwallControls {
    /// 0 dB: the full scale.
    pub const DEFAULT: Self = Self { ceiling: Db(0.0) };
}

impl Default for BrickwallControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The amplitude of `ceiling`, or `None` for a NaN, which has none.
#[inline]
fn amplitude(ceiling: Db) -> Option<f32> {
    let linear = ceiling.to_amplitude().get();
    (!linear.is_nan()).then_some(linear)
}

/// `x` clamped to `+-ceiling`. `ceiling` is never NaN (`clamp` panics on one) and
/// never negative.
#[inline]
fn clip(x: f32, ceiling: f32) -> f32 {
    x.clamp(-ceiling, ceiling)
}

/// The per-frame lane a brickwall reads beside its audio, in the audio's sample format,
/// `None` where nothing drives it. A lane is at least as long as the frames a call renders;
/// a frame past its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrickwallLanes<'a, S: Float = f32> {
    /// The ceiling in dB. A frame that is NaN clamps at the aimed ceiling.
    pub ceiling: Option<Mono<'a, S>>,
}

/// A brickwall at one ceiling, every channel clamped to it: `y = clamp(x, -c, c)`,
/// memoryless, so no latency and no tail. The ceiling lands whole (a glided bound would let
/// a sample past the new one), and the clip is memoryless, so the render does not depend on
/// how the input is cut into blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallBank {
    ceiling: Amplitude,
}

impl BrickwallBank {
    /// A clipper at the full scale, 0 dB.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ceiling: Amplitude::UNITY,
        }
    }

    /// Aims at `controls`: a NaN ceiling is the default's, 0 dB.
    pub fn aim(&mut self, controls: &BrickwallControls) {
        self.ceiling = Amplitude(amplitude(controls.ceiling).unwrap_or(1.0));
    }

    /// Runs `input` into `output`, every frame clamped at the ceiling: its own on a driven
    /// frame, the aimed one on any other (a NaN is not a level).
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the narrower of
    /// the two views is not touched. No allocation.
    pub fn process<S: Float>(
        &self,
        lanes: BrickwallLanes<'_, S>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        let base = self.ceiling.get();
        match lanes.ceiling {
            // Each frame's ceiling is converted once, for every channel.
            Some(lane) => {
                let lane = lane.samples();
                for i in 0..frames {
                    let ceiling = lane
                        .get(i)
                        .and_then(|x| amplitude(Db(x.convert_to::<f32>())))
                        .unwrap_or(base);
                    for c in 0..width {
                        let x = input.plane(c).samples()[i].convert_to::<f32>();
                        output.plane_mut(c).samples_mut()[i] = clip(x, ceiling).convert_to::<S>();
                    }
                }
            }
            None => {
                for c in 0..width {
                    let xs = &input.plane(c).samples()[..frames];
                    let mut plane = output.plane_mut(c);
                    let ys = &mut plane.samples_mut()[..frames];
                    for (y, x) in ys.iter_mut().zip(xs) {
                        *y = clip(x.convert_to::<f32>(), base).convert_to::<S>();
                    }
                }
            }
        }
    }
}

impl Default for BrickwallBank {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
