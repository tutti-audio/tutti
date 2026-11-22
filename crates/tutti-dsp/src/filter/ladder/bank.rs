//! [`LadderBank`]: one four-stage ladder per channel, all channels at one
//! [`LadderBlend`], over buffers views.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, InputOutput, Planes, PlanesMut};

use super::kernel::LadderCoefficients;
use super::solve::nonlinear_ladder_step;
use super::{LadderModel, LadderType};
use crate::channel_group::group_size;
use crate::design::along;
use crate::{Lerp, Ramp, SimdLanes};

/// What a ladder's ramp blends: the coefficients and the drive, with the tap taken as the
/// output, in the arithmetic `F` the bank runs in. A blend keeps the target's tap, so a type
/// changes at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderBlend<F: Float + SimdLanes> {
    coefficients: LadderCoefficients<F>,
    drive: F,
    ty: LadderType,
}

impl<F: Float + SimdLanes> From<&LadderModel> for LadderBlend<F> {
    /// The model's own coefficients and drive ([`LadderModel::new`] sanitises the controls
    /// first), narrowed once to `F`.
    fn from(model: &LadderModel) -> Self {
        Self {
            coefficients: LadderCoefficients::from(model.design()),
            drive: F::from_unit(model.drive()),
            ty: model.ladder_type(),
        }
    }
}

impl<F: Float + SimdLanes> Lerp for LadderBlend<F> {
    /// The coefficients and the drive blended linearly, `t = k / n` in `F`: an interpolated
    /// set is only ever used for one sample and never stored.
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = along::<F>(k, n);
        Self {
            coefficients: self.coefficients.lerp(&to.coefficients, t),
            drive: crate::lerp(self.drive, to.drive, t),
            ty: to.ty,
        }
    }
}

/// The state of a ladder per channel: its four stage words in the
/// float `F`, every channel read through one shared [`LadderBlend`]. Each sample solves the
/// nonlinear ladder's loop equation with a capped Newton iteration (see the crate's `solve`
/// module: a `tanh` at each stage's input and in the loop).
///
/// The width is fixed when it is built. [`process`](Self::process) runs a block of audio
/// through it while the design ramps toward its target and then holds. Channels run in groups
/// of up to eight side by side: a recursive filter is latency-bound on one channel (each sample
/// waits for the last), so independent channels fill that latency.
#[derive(Clone, Debug, PartialEq)]
pub struct LadderBank<F> {
    state: Box<[[F; 4]]>,
}

impl<F: Float + SimdLanes> LadderBank<F> {
    /// A silent bank `width` channels wide. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout) -> Self {
        Self {
            state: vec![[F::ZERO; 4]; usize::from(width.count())].into_boxed_slice(),
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.state.fill([F::ZERO; 4]);
    }

    /// Runs `input` into `output`, frame `i` at `blend.at(taken + 1 + i)`, then takes the
    /// block's steps from the ramp.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`. Channels run in groups of up to
    /// eight, each group's planes taken once per block; while the ramp moves, a group reads
    /// the blend once per frame. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(
        &mut self,
        blend: &mut Ramp<LadderBlend<F>>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let width = self.state.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let mut first = 0;
        while first < width {
            let n = group_size(width - first);
            macro_rules! group {
                ($n:literal) => {
                    group::<F, $n>(
                        blend,
                        &mut self.state[first..first + $n],
                        first,
                        &input,
                        &mut output,
                        frames,
                    )
                };
            }
            match n {
                8 => group!(8),
                7 => group!(7),
                6 => group!(6),
                5 => group!(5),
                4 => group!(4),
                3 => group!(3),
                2 => group!(2),
                _ => group!(1),
            }
            first += n;
        }
        blend.advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

/// Channels `first..first + N` over the first `frames` frames, sample-inner, the group's
/// states in registers: the frames the ramp blends at a design per frame, the rest at its
/// target.
#[inline]
fn group<F: Float + SimdLanes, const N: usize>(
    ramp: &Ramp<LadderBlend<F>>,
    state: &mut [[F; 4]],
    first: usize,
    input: &Planes<'_, F>,
    output: &mut PlanesMut<'_, '_, F>,
    frames: usize,
) {
    let xs: [&[F]; N] = core::array::from_fn(|k| &input.plane(first + k).samples()[..frames]);
    let mut outs = output.iter_mut().skip(first);
    // The group is within the width the caller took from both views, so every one of
    // its `N` channels has a plane.
    let ys: [&mut [F]; N] = core::array::from_fn(|_| {
        let plane = outs
            .next()
            .expect("a group's channels are below the output's width");
        &mut plane.into_samples_mut()[..frames]
    });
    let mut st: [[F; 4]; N] = core::array::from_fn(|k| state[k]);
    macro_rules! frame {
        ($i:expr, $d:expr) => {
            for k in 0..N {
                let x = xs[k][$i] * $d.drive;
                ys[k][$i] = nonlinear_ladder_step(&$d.coefficients, $d.ty, &mut st[k], x);
            }
        };
    }
    let blended = ramp.blended_frames(frames);
    for (i, step) in (0..blended).zip(ramp.steps_taken().saturating_add(1)..) {
        let d = ramp.at(step);
        frame!(i, &d);
    }
    let d = ramp.target();
    for i in blended..frames {
        frame!(i, &d);
    }
    state.copy_from_slice(&st);
}

#[cfg(test)]
mod tests;
