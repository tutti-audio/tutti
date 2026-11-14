//! [`SvfBank`]: one state-variable filter per channel, all channels at one set of
//! coefficients, over buffers views.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::ChannelLayout;

use super::kernel::{SvfCoefficients, svf_step};
use crate::control::lane_group;
use crate::{Glide, Lanes};

/// The state of an SVF per channel: Simper's trapezoidal integrator pair
/// `[ic1eq, ic2eq]` in precision `F`, every channel read through one shared
/// [`SvfCoefficients`].
///
/// The width is fixed when it is built. [`process`](Self::process) runs a block of
/// audio through it while the coefficients glide toward their target, and the held
/// remainder of the block up to eight channels side by side: a recursive filter is
/// latency-bound on one channel (each sample waits for the last), so independent
/// channels fill that latency. Measured on the f64 SVF at width 6, one block: all six as
/// one group 0.49 us, as 3 + 3 0.60 us, one channel at a time 1.33 us.
#[derive(Clone, Debug, PartialEq)]
pub struct SvfBank<F> {
    state: Box<[[F; 2]]>,
}

impl<F: Float + Lanes> SvfBank<F> {
    /// A silent bank `width` channels wide. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout) -> Self {
        Self {
            state: vec![[F::ZERO; 2]; usize::from(width.count())].into_boxed_slice(),
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.state.fill([F::ZERO; 2]);
    }

    /// Runs `input` into `output`, frame `i` at `coefficients.at(taken + 1 + i)`, then
    /// takes the block's steps from the glide.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's
    /// width, or past the narrower of the two views, is not touched. Channels run in
    /// groups of up to eight, each group's planes taken once per block; while the glide
    /// moves, a group reads the coefficients once per frame. No allocation.
    pub fn process<S: Float>(
        &mut self,
        coefficients: &mut Glide<SvfCoefficients<F>>,
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
                        coefficients,
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
        coefficients.advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

/// Channels `first..first + N` over the first `frames` frames, sample-inner, the group's
/// states in registers: the frames the glide blends at a coefficient set per frame, the
/// rest at its target.
#[inline]
fn group<F: Float + Lanes, S: Float, const N: usize>(
    glide: &Glide<SvfCoefficients<F>>,
    state: &mut [[F; 2]],
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
    let mut st: [[F; 2]; N] = core::array::from_fn(|k| state[k]);
    macro_rules! frame {
        ($i:expr, $c:expr) => {
            for k in 0..N {
                let y = svf_step($c, &mut st[k], F::from_f64(xs[k][$i].to_f64()));
                ys[k][$i] = S::from_f64(y.to_f64());
            }
        };
    }
    let blended = glide.blended(frames);
    for (i, step) in (0..blended).zip(glide.taken().saturating_add(1)..) {
        let c = glide.at(step);
        frame!(i, &c);
    }
    let c = glide.target();
    for i in blended..frames {
        frame!(i, &c);
    }
    state.copy_from_slice(&st);
}

#[cfg(test)]
mod tests;
