//! [`CrossoverBank`]: one Linkwitz-Riley band tree per channel, all channels at one
//! [`CrossoverModel`], over buffers views.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::{ChannelLayout, MAX_CHANNELS};

use super::CrossoverModel;
use super::CrossoverState;
use super::bands::MAX_BANDS;

/// The state of a crossover per channel, every channel read through one
/// [`CrossoverModel`]. The width is fixed when it is built.
///
/// A model of `bands` bands makes `bands * width` output planes, band-major: plane
/// `band * width + channel` is the channel's band. The model is whole at every frame
/// (a blend of a split's low-pass, high-pass and all-pass is not one, so the bands would
/// stop summing to an all-pass inside it): a crossover that moves is designed again
/// between two frames and the sections run on from their words.
#[derive(Clone, Debug)]
pub struct CrossoverBank {
    state: Box<[CrossoverState]>,
}

impl CrossoverBank {
    /// A silent bank `width` channels wide. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout) -> Self {
        Self {
            state: vec![CrossoverState::default(); usize::from(width.count())].into_boxed_slice(),
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.state.fill(CrossoverState::default());
    }

    /// Runs `input` into `output` at `model`: channel `c` of `input` makes planes
    /// `c`, `width + c`, ... of `output`, one a band, for as many frames as the shorter
    /// view holds.
    ///
    /// A channel past the bank's width, the input's or [`MAX_CHANNELS`], and a band's plane
    /// past the output's, is not touched. No allocation.
    pub fn process<S: Float>(
        &mut self,
        model: &CrossoverModel,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self.state.len();
        let bands = model.bands();
        let channels = width.min(input.channels()).min(MAX_CHANNELS);
        // Every channel's band planes, taken from the output once per block.
        let mut planes: [[&mut [S]; MAX_BANDS]; MAX_CHANNELS] =
            core::array::from_fn(|_| core::array::from_fn(|_| <&mut [S]>::default()));
        let mut reached = [0usize; MAX_CHANNELS];
        for (p, plane) in output.iter_mut().enumerate().take(width * bands) {
            let (band, channel) = (p / width, p % width);
            if channel < channels {
                planes[channel][band] = &mut plane.into_samples_mut()[..frames];
                reached[channel] = band + 1;
            }
        }
        for (c, state) in self.state.iter_mut().enumerate().take(channels) {
            let x = &input.plane(c).samples()[..frames];
            let ys = &mut planes[c][..reached[c]];
            for (i, &v) in x.iter().enumerate() {
                let split = state.tick_f64(model, v.to_f64());
                for (y, &band) in ys.iter_mut().zip(&split) {
                    y[i] = S::from_f64(band);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
