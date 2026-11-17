//! [`SosBank`]: a [`Sos`] run as a cascade of state-variable sections per channel, its
//! coefficients [`SosCoefficients`] gliding between designs.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::ChannelLayout;

use super::{Sos, svf_form};
use crate::{Glide, Lanes, Lerp, SvfCoefficients, svf_step};

/// The most sections a [`SosCoefficients`] holds: order 16, as [`MAX_ORDER`](super::MAX_ORDER)
/// allows, is eight.
pub const MAX_SECTIONS: usize = 8;

/// The coefficients [`SosBank`] reads: one [`SvfCoefficients`] per section of a [`Sos`], in
/// the arithmetic `F` it runs in. A blend is section by section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SosCoefficients<F: Lanes> {
    sections: [SvfCoefficients<F>; MAX_SECTIONS],
    len: usize,
}

impl<F: Float + Lanes> SosCoefficients<F> {
    /// The section forms of `sos` ([`svf_form`](super::svf_form)) narrowed once to `F`, or
    /// `None` for a cascade of more than [`MAX_SECTIONS`] sections.
    #[must_use]
    pub fn new(sos: &Sos) -> Option<Self> {
        let sections = sos.sections();
        if sections.len() > MAX_SECTIONS {
            return None;
        }
        let mut set = Self::THROUGH;
        for (to, b) in set.sections.iter_mut().zip(sections) {
            *to = SvfCoefficients::from(&svf_form(b));
        }
        set.len = sections.len();
        Some(set)
    }

    /// No sections: the input as it is.
    pub const THROUGH: Self = Self {
        sections: [SvfCoefficients {
            a1: F::ZERO,
            a2: F::ZERO,
            a3: F::ZERO,
            m0: F::ONE,
            m1: F::ZERO,
            m2: F::ZERO,
        }; MAX_SECTIONS],
        len: 0,
    };
}

impl<F: Float + Lanes> Lerp for SosCoefficients<F> {
    /// Each section blended linearly; the sections past the shorter cascade are the longer's.
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let mut out = *to;
        for i in 0..self.len.min(to.len) {
            out.sections[i] = self.sections[i].lerp(&to.sections[i], k, n);
        }
        out
    }
}

/// The state of a cascade per channel: Simper's integrator pair `[ic1eq, ic2eq]` of each
/// section in precision `F`, every channel read through one shared [`SosCoefficients`].
///
/// The width is fixed when it is built. A channel runs every section of a frame before the
/// next, so its state stays in registers.
#[derive(Clone, Debug, PartialEq)]
pub struct SosBank<F> {
    state: Box<[[[F; 2]; MAX_SECTIONS]]>,
}

impl<F: Float + Lanes> SosBank<F> {
    /// A silent bank `width` channels wide. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout) -> Self {
        Self {
            state: vec![[[F::ZERO; 2]; MAX_SECTIONS]; usize::from(width.count())]
                .into_boxed_slice(),
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.state.fill([[F::ZERO; 2]; MAX_SECTIONS]);
    }

    /// Runs `input` into `output`, frame `i` at `coefficients.at(taken + 1 + i)`, then
    /// takes the block's steps from the glide. Channel `c` of `input` feeds channel `c` of
    /// `output`; a channel past the bank's width or the narrower view is not touched. No
    /// allocation.
    pub fn process<S: Float>(
        &mut self,
        coefficients: &mut Glide<SosCoefficients<F>>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .state
            .len()
            .min(input.channels())
            .min(output.channels());
        let blended = coefficients.blended(frames);
        let first = coefficients.taken().saturating_add(1);
        let target = coefficients.target();
        for (c, state) in self.state.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            for (i, (y, x)) in y.iter_mut().zip(x).enumerate() {
                let set = if i < blended {
                    coefficients.at(first + i as u32)
                } else {
                    target
                };
                let mut v = F::from_f64(x.to_f64());
                for (s, st) in set.sections[..set.len].iter().zip(state.iter_mut()) {
                    v = svf_step(s, st, v);
                }
                *y = S::from_f64(v.to_f64());
            }
        }
        coefficients.advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}
