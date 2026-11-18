//! [`SosBank`]: a [`Sos`] run as a cascade of state-variable sections per channel, its
//! coefficients [`SosCoefficients`] gliding between designs.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::ChannelLayout;

use super::{Sos, svf_form};
use crate::{Glide, Lanes, Lerp, SvfCoefficients, svf_step};

/// The most sections a [`SosCoefficients`] holds: order 16 is eight.
pub const MAX_SECTIONS: usize = 8;

/// The coefficients [`SosBank`] reads: one [`SvfCoefficients`] per section of a [`Sos`], in
/// the arithmetic `F` it runs in. A blend is section by section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SosCoefficients<F: Lanes> {
    sections: [SvfCoefficients<F>; MAX_SECTIONS],
    len: usize,
}

impl<F: Float + Lanes> SosCoefficients<F> {
    /// The section forms of `sos` (`svf_form`) narrowed once to `F`, or
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
            d: F::ZERO,
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
/// section in the float `F`, every channel read through one shared [`SosCoefficients`].
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
    pub fn process(
        &mut self,
        coefficients: &mut Glide<SosCoefficients<F>>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .state
            .len()
            .min(input.channels())
            .min(output.channels());
        let blended = coefficients.blended(frames);
        let first = coefficients.taken().saturating_add(1);
        // The frames the glide still blends read a set of their own, once for every channel.
        for i in 0..blended {
            let set = coefficients.at(first + i as u32);
            for (c, state) in self.state.iter_mut().enumerate().take(width) {
                let x = input.plane(c).samples()[i];
                output.plane_mut(c).samples_mut()[i] = run(&set, state, x);
            }
        }
        let target = coefficients.target();
        for (c, state) in self.state.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[blended..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[blended..frames].iter_mut().zip(x) {
                *y = run(&target, state, x);
            }
        }
        coefficients.advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

/// One frame `x` through the sections of `set`, over the channel's `state`.
#[inline(always)]
fn run<F: Float + Lanes>(set: &SosCoefficients<F>, state: &mut [[F; 2]; MAX_SECTIONS], x: F) -> F {
    set.sections[..set.len]
        .iter()
        .zip(state.iter_mut())
        .fold(x, |v, (s, st)| svf_step(s, st, v))
}
