//! [`SvfBank`]: one state-variable filter per channel, all channels at one set of
//! coefficients, over buffers views.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, InputOutput, Planes, PlanesMut};

use super::kernel::{SvfCoefficients, svf_step};
use crate::channel_group::group_size;
use crate::{Ramp, SimdLanes};

/// The state of an SVF per channel: Simper's trapezoidal integrator pair
/// `[ic1eq, ic2eq]` in the float `F`, every channel read through one shared
/// [`SvfCoefficients`].
///
/// The width is fixed when it is built. [`process`](Self::process) runs a block of
/// audio through it while the coefficients ramp toward their target, and the held
/// remainder of the block up to eight channels side by side: a recursive filter is
/// latency-bound on one channel (each sample waits for the last), so independent
/// channels fill that latency. Measured on the f64 SVF at width 6, one block: all six as
/// one group 0.49 us, as 3 + 3 0.60 us, one channel at a time 1.33 us.
#[derive(Clone, Debug, PartialEq)]
pub struct SvfBank<F> {
    state: Box<[[F; 2]]>,
}

impl<F: Float + SimdLanes> SvfBank<F> {
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
    /// takes the block's steps from the ramp.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`. Channels run in groups of up to
    /// eight, each group's planes taken once per block; while the ramp moves, a group reads
    /// the coefficients once per frame. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(
        &mut self,
        coefficients: &mut Ramp<SvfCoefficients<F>>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        expect_widths(block, self.state.len(), self.state.len())?;
        let (input, mut output) = block.split();
        self.run(coefficients, &input, &mut output);
        Ok(())
    }

    /// The channels the bank filters.
    pub(crate) fn channels(&self) -> usize {
        self.state.len()
    }

    /// [`process`](Self::process) over views the caller has checked: the same frames and
    /// channels on both sides, at most the bank's width (a key narrower than the bank is
    /// filtered on its own channels).
    pub(crate) fn run(
        &mut self,
        coefficients: &mut Ramp<SvfCoefficients<F>>,
        input: &Planes<'_, F>,
        output: &mut PlanesMut<'_, '_, F>,
    ) {
        debug_assert_eq!(input.frames(), output.frames());
        let frames = input.frames().get();
        let width = input.channels();
        debug_assert!(width <= self.state.len() && output.channels() == width);
        let mut first = 0;
        while first < width {
            let n = group_size(width - first);
            macro_rules! group {
                ($n:literal) => {
                    group::<F, $n>(
                        coefficients,
                        &mut self.state[first..first + $n],
                        first,
                        input,
                        output,
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
/// states in registers: the frames the ramp blends at a coefficient set per frame, the
/// rest at its target.
#[inline]
fn group<F: Float + SimdLanes, const N: usize>(
    ramp: &Ramp<SvfCoefficients<F>>,
    state: &mut [[F; 2]],
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
    let mut st: [[F; 2]; N] = core::array::from_fn(|k| state[k]);
    macro_rules! frame {
        ($i:expr, $c:expr) => {
            for k in 0..N {
                ys[k][$i] = svf_step($c, &mut st[k], xs[k][$i]);
            }
        };
    }
    let blended = ramp.blended_frames(frames);
    for (i, step) in (0..blended).zip(ramp.steps_taken().saturating_add(1)..) {
        let c = ramp.at(step);
        frame!(i, &c);
    }
    let c = ramp.target();
    for i in blended..frames {
        frame!(i, &c);
    }
    state.copy_from_slice(&st);
}

#[cfg(test)]
mod tests;
