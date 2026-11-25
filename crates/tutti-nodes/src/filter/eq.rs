//! [`EqKernel`]: the kernel of the bell and the tilt, a one-section closed-form design run
//! as a state-variable section.

use tutti_buffers::Float;
use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, DecayTime, SimdLanes, Sos, SosError, TAIL_FLOOR};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, SampleRate, Samples, Tail};

use super::ramped::{RAMP_STEPS, RampedSos};
use crate::kernel::{ControlSignals, Kernel};
use crate::params::ControlSet;

/// What a controls type must give [`EqKernel`], kept out of reach of an implementation.
pub(super) mod sealed {
    use super::*;

    /// A one-section closed-form design read from controls.
    pub trait Sealed {
        /// The control set that reads and writes these controls.
        type Set: ControlSet<Controls = Self>;

        /// The controls a kernel starts from.
        const DEFAULT: Self;

        /// These controls held within the ranges they are read in at `rate`.
        #[must_use]
        fn held(&self, rate: SampleRate) -> Self;

        /// The design at these held controls, into `into`: no allocation.
        ///
        /// # Errors
        ///
        /// The design's refusal of the controls.
        fn design_into(&self, rate: SampleRate, into: &mut Sos) -> Result<(), SosError>;

        /// The ring-out of the design at these held controls.
        ///
        /// # Errors
        ///
        /// The design's refusal of the controls.
        fn decay_time(&self, rate: SampleRate, floor: Db) -> Result<DecayTime, SosError>;

        /// The ring-out of every held controls at `rate`: a bound on `decay_time` over the
        /// ranges, proven from the designs or [`DecayTime::Unbounded`].
        ///
        /// # Errors
        ///
        /// The design's refusal of the ranges.
        fn decay_ceiling(rate: SampleRate, floor: Db) -> Result<DecayTime, SosError>;
    }
}

/// The controls of a one-section equalizer that [`EqKernel`] designs from: the bell's and
/// the tilt's, which are the only implementations.
pub trait EqControls:
    sealed::Sealed + Copy + core::fmt::Debug + PartialEq + Send + 'static
{
}

/// An equalizer of any width as a [`Kernel`]: the one section `C` designs, run as a
/// state-variable cascade whose coefficients ramp.
///
/// The design is made on the stream's 16-frame grid and reached along a line in the section's
/// coefficients over the next 16 frames with the state kept. The tail is the ring-out bound
/// over every control, found in `prepare`; each block narrows it to the controls in force.
/// Made through the node aliases ([`BellFilterNode`](crate::BellFilterNode),
/// [`TiltFilterNode`](crate::TiltFilterNode)).
#[derive(Clone, Debug)]
pub struct EqKernel<C: EqControls, F: Float + SimdLanes> {
    input: ChannelTopology,
    sos: RampedSos<F>,
    rate: SampleRate,
    /// The tail over every control at `rate`.
    ceiling: Tail,
    controls: core::marker::PhantomData<C>,
}

impl<C: EqControls, F: Float + SimdLanes> EqKernel<C, F> {
    /// A kernel on the bus `input`, its output where it is; at a placeholder 48 kHz until
    /// `prepare`, which an unprepared node renders nothing before.
    pub(super) fn new(input: ChannelTopology) -> Self {
        let mut kernel = Self {
            input,
            sos: RampedSos::new(input),
            rate: SampleRate::SR_48K,
            ceiling: Tail::Unknown,
            controls: core::marker::PhantomData,
        };
        let first = kernel.design(&C::DEFAULT);
        kernel.sos.start(first);
        kernel.ceiling = kernel.ceiling_at();
        kernel
    }

    /// The coefficients at `controls`, read within their ranges; the design in force for
    /// controls that are not numbers. No allocation.
    fn design(&mut self, controls: &C) -> tutti_dsp::SosCoefficients<F> {
        let (held, rate) = (controls.held(self.rate), self.rate);
        self.sos.design(|into| held.design_into(rate, into))
    }

    /// The ring-out over every control: `C::decay_ceiling`.
    fn ceiling_at(&self) -> Tail {
        match C::decay_ceiling(self.rate, TAIL_FLOOR) {
            Ok(DecayTime::Finite(t)) => Tail::Finite(Samples::new(
                (t.get() * self.rate.get()).ceil() as usize + 1,
            )),
            _ => Tail::Unbounded,
        }
    }

    /// The ring-out at `controls`, read within their ranges.
    fn ring(&self, controls: &C) -> Tail {
        match controls.held(self.rate).decay_time(self.rate, TAIL_FLOOR) {
            Ok(DecayTime::Finite(t)) => {
                Tail::Finite(Samples::new((t.get() * self.rate.get()).ceil() as usize))
            }
            _ => Tail::Unbounded,
        }
    }
}

impl<C: EqControls, F: Float + SimdLanes> Kernel for EqKernel<C, F> {
    type Float = F;
    type Set = C::Set;

    const GRID: usize = RAMP_STEPS as usize;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.sos.at_rate(rate);
        let first = self.design(&C::DEFAULT);
        self.sos.restart(first);
        self.ceiling = self.ceiling_at();
    }

    fn reset(&mut self) {
        self.sos.reset();
    }

    fn retarget(&mut self, controls: &C) {
        let design = self.design(controls);
        self.sos.retarget(design);
    }

    fn jump_to_target(&mut self) {
        self.sos.jump_to_target();
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.sos.render(block)
    }

    /// The longest ring-out over the controls, found in `prepare`.
    fn tail(&self) -> Tail {
        self.ceiling
    }

    /// The ring-out at `controls`, in closed form. No simulation.
    fn tail_of(&mut self, controls: &C) -> Tail {
        self.ring(controls)
    }
}
