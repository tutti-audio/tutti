//! [`BrickwallBank`]: the hard clipper over buffers views.
//!
//! A brickwall clamps every sample to `+-c` and has nothing else: no state, no
//! envelope, no lookahead and so no latency and no tail. Its only control is the ceiling,
//! which is read per frame when the graph drives it ([`BrickwallLanes`]); the set ceiling
//! is the one a frame nobody drives is clamped at. The clamp bounds the **sample** peak of
//! the output for any finite or infinite input; a NaN input passes as NaN. It is not a
//! true-peak limit.
//!
//! The ceiling is in dB and the clamp bound is its amplitude. `+inf` dB passes
//! everything and `-inf` dB mutes (both are ceilings); only a NaN has no level, and it
//! is ignored wherever it comes from (the node keeps the last ceiling for a NaN
//! cell, and a NaN frame on the lane clamps at the set ceiling).

use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_units::Db;

use super::curves::db_gain;

/// The controls of a [`BrickwallBank`]: the ceiling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallControls {
    /// The level samples are clamped to, in dB (`+-inf` are ceilings; a NaN has no
    /// level and falls back to the default, 0 dB).
    pub ceiling: Db,
}

impl BrickwallControls {
    /// 0 dB: the full scale.
    pub const DEFAULT: Self = Self { ceiling: Db::UNITY };
}

impl Default for BrickwallControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The amplitude of a ceiling of `db` decibels, or `None` for a NaN, which has none.
#[inline]
fn amplitude<F: Float>(db: F) -> Option<F> {
    let linear = db_gain(db);
    (!linear.is_nan()).then_some(linear)
}

/// `x` clamped to `+-ceiling`. `ceiling` is never NaN (`clamp` panics on one) and
/// never negative.
#[inline]
fn clip<F: Float>(x: F, ceiling: F) -> F {
    x.clamp(-ceiling, ceiling)
}

/// The per-frame lane a brickwall reads beside its audio, in the bank's float, `None` where
/// nothing drives it. A lane is at least as long as the frames a call renders; a frame past
/// its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrickwallLanes<'a, F: Float> {
    /// The ceiling in dB. A frame that is NaN clamps at the set ceiling.
    pub ceiling: Option<Mono<'a, F>>,
}

/// A brickwall at one ceiling, every channel clamped to it: `y = clamp(x, -c, c)`,
/// memoryless, so no latency and no tail. The ceiling lands whole (a ramped bound would let
/// a sample past the new one), and the clip is memoryless, so the render does not depend on
/// how the input is cut into blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallBank<F: Float> {
    /// The set ceiling as an amplitude.
    ceiling: F,
}

impl<F: Float> BrickwallBank<F> {
    /// A clipper at the full scale, 0 dB.
    #[must_use]
    pub const fn new() -> Self {
        Self { ceiling: F::ONE }
    }

    /// Aims at `controls`: a NaN ceiling is the default's, 0 dB. A ceiling whose amplitude is
    /// past the float's largest finite value (about 770 dB at `f32`) passes everything, an
    /// infinity included, as `+inf` dB does.
    pub fn retarget(&mut self, controls: &BrickwallControls) {
        let ceiling = amplitude::<f64>(controls.ceiling.get()).unwrap_or(1.0);
        // A ceiling past the float's range passes everything, as `+inf` dB does.
        self.ceiling = if ceiling > F::MAX.into() {
            F::INFINITY
        } else {
            F::from_design(ceiling)
        };
    }

    /// Runs `input` into `output`, every frame clamped at the ceiling: its own on a driven
    /// frame, the set one on any other (a NaN is not a level).
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the narrower of
    /// the two views is not touched. No allocation.
    pub fn process(
        &self,
        lanes: BrickwallLanes<'_, F>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        let base = self.ceiling;
        match lanes.ceiling {
            // Each frame's ceiling is found once, for every channel.
            Some(lane) => {
                let lane = lane.samples();
                for i in 0..frames {
                    let ceiling = lane.get(i).and_then(|&x| amplitude(x)).unwrap_or(base);
                    for c in 0..width {
                        let x = input.plane(c).samples()[i];
                        output.plane_mut(c).samples_mut()[i] = clip(x, ceiling);
                    }
                }
            }
            None => {
                for c in 0..width {
                    let xs = &input.plane(c).samples()[..frames];
                    let mut plane = output.plane_mut(c);
                    let ys = &mut plane.samples_mut()[..frames];
                    for (y, &x) in ys.iter_mut().zip(xs) {
                        *y = clip(x, base);
                    }
                }
            }
        }
    }
}

impl<F: Float> Default for BrickwallBank<F> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
