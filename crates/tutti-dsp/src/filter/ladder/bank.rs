//! [`LadderBank`]: one four-stage ladder per channel, all channels at one
//! [`LadderDesign`], over buffers views.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::ChannelLayout;

use super::kernel::{LadderCoefficients, ladder_step};
use super::{LadderModel, LadderType};
use crate::control::lane_group;
use crate::{Glide, Lanes, Lerp};

/// What a ladder's glide blends: the coefficients and the drive, with the tap taken as the
/// output, in the arithmetic `F` the bank runs in. A blend keeps the target's tap, so a type
/// changes at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderDesign<F: Float + Lanes> {
    coefficients: LadderCoefficients<F>,
    drive: F,
    ty: LadderType,
}

impl<F: Float + Lanes> From<&LadderModel> for LadderDesign<F> {
    /// The model's own coefficients and drive ([`LadderModel::new`] sanitises the controls
    /// first), narrowed once to `F`. The drive is an `f64` made of an `f32`, so narrowing it
    /// to `F` is exact.
    fn from(model: &LadderModel) -> Self {
        Self {
            coefficients: LadderCoefficients::from(model.coeffs()),
            drive: F::from_f64(model.drive()),
            ty: model.ladder_type(),
        }
    }
}

impl<F: Float + Lanes> Lerp for LadderDesign<F> {
    /// The coefficients and the drive blended linearly, `t = k / n` in `F`: an interpolated
    /// set is only ever used for one sample and never stored.
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = F::from_f64(f64::from(k)) / F::from_f64(f64::from(n));
        Self {
            coefficients: self.coefficients.lerp(&to.coefficients, t),
            drive: self.drive + (to.drive - self.drive) * t,
            ty: to.ty,
        }
    }
}

/// The state of a ladder per channel: its four stage words in precision `F`, every channel
/// read through one shared [`LadderDesign`].
///
/// The width is fixed when it is built. [`process`](Self::process) runs a block of audio
/// through it while the design glides toward its target and then holds. Channels run in groups
/// of up to eight side by side: a recursive filter is latency-bound on one channel (each sample
/// waits for the last), so independent channels fill that latency.
#[derive(Clone, Debug, PartialEq)]
pub struct LadderBank<F> {
    state: Box<[[F; 4]]>,
}

impl<F: Float + Lanes> LadderBank<F> {
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

    /// Runs `input` into `output`, frame `i` at `design.at(taken + 1 + i)`, then takes the
    /// block's steps from the glide.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's
    /// width, or past the narrower of the two views, is not touched. Channels run in groups
    /// of up to eight, each group's planes taken once per block; while the glide moves, a
    /// group reads the design once per frame. No allocation.
    pub fn process<S: Float>(
        &mut self,
        design: &mut Glide<LadderDesign<F>>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .state
            .len()
            .min(input.channels())
            .min(output.channels());
        let mut first = 0;
        while first < width {
            let n = lane_group(width - first);
            macro_rules! group {
                ($n:literal) => {
                    group::<F, S, $n>(
                        design,
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
        design.advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

/// Channels `first..first + N` over the first `frames` frames, sample-inner, the group's
/// states in registers: the frames the glide blends at a design per frame, the rest at its
/// target.
#[inline]
fn group<F: Float + Lanes, S: Float, const N: usize>(
    glide: &Glide<LadderDesign<F>>,
    state: &mut [[F; 4]],
    first: usize,
    input: &Planes<'_, S>,
    output: &mut PlanesMut<'_, '_, S>,
    frames: usize,
) {
    let xs: [&[S]; N] = core::array::from_fn(|k| &input.plane(first + k).samples()[..frames]);
    let mut outs = output.iter_mut().skip(first);
    // The group is within the width the caller took from both views, so every one of
    // its `N` channels has a plane.
    let ys: [&mut [S]; N] = core::array::from_fn(|_| {
        let plane = outs
            .next()
            .expect("a group's channels are below the output's width");
        &mut plane.into_samples_mut()[..frames]
    });
    let mut st: [[F; 4]; N] = core::array::from_fn(|k| state[k]);
    macro_rules! frame {
        ($i:expr, $d:expr) => {
            for k in 0..N {
                let x = F::from_f64(xs[k][$i].to_f64()) * $d.drive;
                let y = ladder_step(&$d.coefficients, $d.ty, &mut st[k], x, Float::tanh);
                ys[k][$i] = S::from_f64(y.to_f64());
            }
        };
    }
    let blended = glide.blended(frames);
    for (i, step) in (0..blended).zip(glide.taken().saturating_add(1)..) {
        let d = glide.at(step);
        frame!(i, &d);
    }
    let d = glide.target();
    for i in blended..frames {
        frame!(i, &d);
    }
    state.copy_from_slice(&st);
}

#[cfg(test)]
mod tests;
