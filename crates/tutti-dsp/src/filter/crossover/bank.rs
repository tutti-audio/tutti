//! [`CrossoverBank`]: one Linkwitz-Riley band tree per channel, all channels at one glide of
//! [`CrossoverDesign`], over buffers views.

use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, Planes, PlanesMut};

use super::CrossoverDesign;
use super::CrossoverState;
use super::bands::MAX_BANDS;
use super::design::Narrowed;
use crate::{Glide, SimdLanes};

/// The state of a crossover per channel, every channel read through one glide of
/// [`CrossoverDesign`]. The width is fixed when it is built.
///
/// A set of `bands` bands makes `bands * width` output planes, band-major: plane
/// `band * width + channel` is the channel's band. A split that moves glides: each frame
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

    /// Runs `input` into `output`, frame `i` at `coefficients.at(taken + 1 + i)`, then
    /// takes the block's steps from the glide: channel `c` of `input` makes planes `c`,
    /// `width + c`, ... of `output`, one a band, for as many frames as the shorter view
    /// holds.
    ///
    /// A channel past the bank's width, the input's or [`MAX_CHANNELS`], and a band's plane
    /// past the output's, is not touched. No allocation.
    pub fn process(
        &mut self,
        coefficients: &mut Glide<CrossoverDesign>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self.state.len();
        let bands = coefficients.target().bands();
        let channels = width.min(input.channels()).min(MAX_CHANNELS);
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
        let blended = coefficients.blended(frames);
        // The frames the glide blends read a set per frame: frame-outer, so it is solved once.
        for (i, step) in (0..blended).zip(coefficients.taken().saturating_add(1)..) {
            let c = coefficients.at(step).narrowed::<F>();
            for (ch, state) in self.state.iter_mut().enumerate().take(channels) {
                let x = input.plane(ch).samples()[i];
                put(state, &c, x, &mut planes[ch][..reached[ch]], i);
            }
        }
        // The rest runs at the target, channel-outer.
        let c = coefficients.target().narrowed::<F>();
        for (ch, state) in self.state.iter_mut().enumerate().take(channels) {
            let x = &input.plane(ch).samples()[..frames];
            let ys = &mut planes[ch][..reached[ch]];
            for (i, v) in x.iter().enumerate().skip(blended) {
                put(state, &c, *v, ys, i);
            }
        }
        coefficients.advance(u32::try_from(frames).unwrap_or(u32::MAX));
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
