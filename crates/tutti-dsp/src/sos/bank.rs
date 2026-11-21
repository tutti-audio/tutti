//! [`SosBank`]: a [`Sos`] run as a cascade of state-variable sections per channel, its
//! coefficients [`SosCoefficients`] ramping between designs.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, InputOutput, Planes, PlanesMut};

use super::{Sos, svf_form};
use crate::{Lerp, Ramp, SimdLanes, SvfCoefficients, svf_step};

/// The most sections a [`SosCoefficients`] holds: a 31-band graphic equalizer's.
pub const MAX_SECTIONS: usize = 32;

/// The coefficients [`SosBank`] reads: one [`SvfCoefficients`] per section of a [`Sos`], in
/// the arithmetic `F` it runs in. A blend is section by section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SosCoefficients<F: SimdLanes> {
    sections: [SvfCoefficients<F>; MAX_SECTIONS],
    len: usize,
}

impl<F: Float + SimdLanes> SosCoefficients<F> {
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

impl<F: Float + SimdLanes> Lerp for SosCoefficients<F> {
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

impl<F: Float + SimdLanes> SosBank<F> {
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
    /// takes the block's steps from the ramp. Channel `c` of `input` feeds channel `c` of
    /// `output`. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(
        &mut self,
        coefficients: &mut Ramp<SosCoefficients<F>>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        expect_widths(block, self.state.len(), self.state.len())?;
        let (input, mut output) = block.split();
        self.run(coefficients, &input, &mut output);
        Ok(())
    }

    /// [`process`](Self::process) over views the caller has checked: the same frames and
    /// channels on both sides, at most the bank's width (a key narrower than the bank is
    /// filtered on its own channels).
    pub(crate) fn run(
        &mut self,
        coefficients: &mut Ramp<SosCoefficients<F>>,
        input: &Planes<'_, F>,
        output: &mut PlanesMut<'_, '_, F>,
    ) {
        debug_assert_eq!(input.frames(), output.frames());
        let frames = input.frames().get();
        let width = input.channels();
        debug_assert!(width <= self.state.len() && output.channels() == width);
        let blended = coefficients.blended_frames(frames);
        let first = coefficients.steps_taken().saturating_add(1);
        // The frames the ramp still blends read a set of their own, once for every channel.
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
fn run<F: Float + SimdLanes>(
    set: &SosCoefficients<F>,
    state: &mut [[F; 2]; MAX_SECTIONS],
    x: F,
) -> F {
    set.sections[..set.len]
        .iter()
        .zip(state.iter_mut())
        .fold(x, |v, (s, st)| svf_step(s, st, v))
}
