//! [`CrossoverBank`]: one Linkwitz-Riley band tree per channel, all channels at one ramp of
//! [`CrossoverBlend`], over buffers views.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, InputOutput};

use super::CrossoverBlend;
use super::CrossoverState;
use super::bands::MAX_BANDS;
use super::blend::Narrowed;
use crate::{Ramp, SimdLanes};

/// The state of a crossover per channel, every channel read through one ramp of
/// [`CrossoverBlend`]. The width is fixed when it is built.
///
/// A set of `bands` bands makes `bands * width` output planes, band-major: plane
/// `band * width + channel` is the channel's band. A split that moves ramps: each frame
/// runs at the coefficients of a split at some frequency in between (its integrator gain
/// moves linearly), so the bands are a Butterworth set at every frame, and the sections'
/// words are bounded for any motion (see the state's docs).
#[derive(Clone, Debug)]
pub struct CrossoverBank<F: Float + SimdLanes> {
    state: Box<[CrossoverState<F>]>,
}

impl<F: Float + SimdLanes> CrossoverBank<F> {
    /// A silent bank `width` channels wide. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout) -> Self {
        Self {
            state: vec![CrossoverState::<F>::default(); usize::from(width.count())]
                .into_boxed_slice(),
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.state.fill(CrossoverState::<F>::default());
    }

    /// Runs `input` into `output`, frame `i` at `blend.at(taken + 1 + i)`, then
    /// takes the block's steps from the ramp: channel `c` of `input` makes planes `c`,
    /// `width + c`, ... of `output`, one a band. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the input is not the bank's width in channels, or the output is not
    /// `width` times the ramp's bands.
    pub fn process(
        &mut self,
        blend: &mut Ramp<CrossoverBlend>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let width = self.state.len();
        let bands = blend.target().bands();
        expect_widths(block, width, width * bands)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let channels = width.min(MAX_CHANNELS);
        // Every channel's band planes, taken from the output once per block.
        let mut planes: [[&mut [F]; MAX_BANDS]; MAX_CHANNELS] =
            core::array::from_fn(|_| core::array::from_fn(|_| <&mut [F]>::default()));
        let mut reached = [0usize; MAX_CHANNELS];
        for (p, plane) in output.iter_mut().enumerate().take(width * bands) {
            let (band, channel) = (p / width, p % width);
            if channel < channels {
                planes[channel][band] = &mut plane.into_samples_mut()[..frames];
                reached[channel] = band + 1;
            }
        }
        let blended = blend.blended_frames(frames);
        // The frames the ramp blends read a set per frame: frame-outer, so it is solved once.
        for (i, step) in (0..blended).zip(blend.steps_taken().saturating_add(1)..) {
            let c = blend.at(step).narrowed::<F>();
            for (ch, state) in self.state.iter_mut().enumerate().take(channels) {
                let x = input.plane(ch).samples()[i];
                put(state, &c, x, &mut planes[ch][..reached[ch]], i);
            }
        }
        // The rest runs at the target, channel-outer.
        let c = blend.target().narrowed::<F>();
        for (ch, state) in self.state.iter_mut().enumerate().take(channels) {
            let x = &input.plane(ch).samples()[..frames];
            let ys = &mut planes[ch][..reached[ch]];
            for (i, v) in x.iter().enumerate().skip(blended) {
                put(state, &c, *v, ys, i);
            }
        }
        blend.advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

/// Frame `i` of a channel's bands: `x` through `state` at `c`, into the planes it reaches.
#[inline]
fn put<F: Float + SimdLanes>(
    state: &mut CrossoverState<F>,
    c: &Narrowed<F>,
    x: F,
    ys: &mut [&mut [F]],
    i: usize,
) {
    let split = state.tick(c, x);
    for (y, &band) in ys.iter_mut().zip(&split) {
        y[i] = band;
    }
}

#[cfg(test)]
mod tests;
