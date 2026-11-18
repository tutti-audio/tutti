//! Mid/side coding ([`MidSide`]) and stereo width ([`WidthBank`]) over a left/right pair.
//!
//! The mid is the half sum and the side the half difference:
//! `M = (L + R) / 2`, `S = (L - R) / 2`, so `L = M + S` and `R = M - S`. A width `w`
//! scales the side and keeps the mid: `L' = M + w S`, `R' = M - w S`. Width 0 is the mono
//! sum, 1 the pair as it is, 2 doubles the side.

use tutti_buffers::{Float, Planes, PlanesMut, Sample};
use tutti_units::{SampleRate, StereoWidth};

use crate::{GAIN_GLIDE, Glide, Lerp, Steps};

/// The direction of a mid/side conversion of a stereo pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MidSide {
    /// Left/right in, mid/side out: channel 0 is `(L + R) / 2`, channel 1 `(L - R) / 2`.
    Encode,
    /// Mid/side in, left/right out: channel 0 is `M + S`, channel 1 `M - S`.
    Decode,
}

impl MidSide {
    /// Converts the first two channels of `input` into the first two of `output`; nothing
    /// is written when either view has fewer than two. The frames are the fewer of the two
    /// views'. No allocation.
    ///
    /// Decoding an encoding returns each channel within `S::EPSILON * max(|L|, |R|)`
    /// (the half sum, the half difference and their sum each round once; halving is
    /// exact), and exactly where the sums are exact.
    pub fn process<S: Float>(self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        if input.channels() < 2 || output.channels() < 2 {
            return;
        }
        let frames = input.frames().get().min(output.frames().get());
        let (a, b) = (input.plane(0), input.plane(1));
        let (a, b) = (&a.samples()[..frames], &b.samples()[..frames]);
        let mut planes = output.iter_mut();
        let (Some(mut first), Some(mut second)) = (planes.next(), planes.next()) else {
            return;
        };
        let (first, second) = (
            &mut first.samples_mut()[..frames],
            &mut second.samples_mut()[..frames],
        );
        let half = S::from_f64(0.5);
        for (((&a, &b), o0), o1) in a.iter().zip(b).zip(first).zip(second) {
            match self {
                Self::Encode => {
                    *o0 = (a + b) * half;
                    *o1 = (a - b) * half;
                }
                Self::Decode => {
                    *o0 = a + b;
                    *o1 = a - b;
                }
            }
        }
    }
}

/// The control of a [`WidthBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WidthControls {
    /// The scale of the side; a negative value reads as 0.
    pub width: StereoWidth,
}

impl WidthControls {
    /// The pair as it is.
    pub const DEFAULT: Self = Self {
        width: StereoWidth::NATURAL,
    };
}

/// Stereo width of a left/right pair in the float `F`: the side scaled by a width that glides
/// over [`GAIN_GLIDE`].
///
/// Frame `i` of a block reads the glide at step `taken + 1 + i`, then the block's steps
/// are taken, so a write renders the same whatever the blocks. The pair is two channels:
/// a view with fewer renders nothing.
#[derive(Clone, Copy, Debug)]
pub struct WidthBank<F: Float> {
    /// The width in force, narrowed to `F` once per aim.
    width: Glide<F>,
    steps: u32,
}

impl<F: Float + Lerp> WidthBank<F> {
    /// A pair at unity width at `rate`, to be aimed.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let steps = u32::try_from(GAIN_GLIDE.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            width: Glide::landed(F::from_f32(StereoWidth::NATURAL.get())),
            steps,
        }
    }

    /// Lands the width on its target now.
    pub fn settle(&mut self) {
        self.width.settle();
    }

    /// Aims at `controls`: the width glides over [`GAIN_GLIDE`]. A width that is not
    /// finite leaves the aim as it is. No allocation.
    pub fn aim(&mut self, controls: &WidthControls) {
        let w = controls.width.get();
        if w.is_finite() {
            self.width
                .aim_by(F::from_f32(w.max(0.0)), Steps::Fixed(self.steps));
        }
    }

    /// Runs `input` into `output` at the width in force. No allocation.
    pub fn process<S: Float>(&mut self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        if input.channels() < 2 || output.channels() < 2 {
            return;
        }
        let frames = input.frames().get().min(output.frames().get());
        let (l, r) = (input.plane(0), input.plane(1));
        let (l, r) = (&l.samples()[..frames], &r.samples()[..frames]);
        let mut planes = output.iter_mut();
        let (Some(mut left), Some(mut right)) = (planes.next(), planes.next()) else {
            return;
        };
        let (out_l, out_r) = (
            &mut left.samples_mut()[..frames],
            &mut right.samples_mut()[..frames],
        );
        let half = F::from_f32(0.5);
        let blended = self.width.blended(frames);
        let taken = self.width.taken();
        let held = self.width.target();
        if blended == 0 && held == F::from_f32(StereoWidth::NATURAL.get()) {
            out_l.copy_from_slice(l);
            out_r.copy_from_slice(r);
        } else {
            for (i, (((&l, &r), o_l), o_r)) in l.iter().zip(r).zip(out_l).zip(out_r).enumerate() {
                let w = if i < blended {
                    let step = taken.saturating_add(1).saturating_add(i as u32);
                    self.width.at(step)
                } else {
                    held
                };
                let (l, r) = (l.convert_to::<F>(), r.convert_to::<F>());
                let (mid, side) = ((l + r) * half, (l - r) * half);
                *o_l = (mid + w * side).convert_to::<S>();
                *o_r = (mid - w * side).convert_to::<S>();
            }
        }
        self.width
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests;
