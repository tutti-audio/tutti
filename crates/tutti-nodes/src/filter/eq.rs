//! [`EqKernel`]: the kernel of the bell and the tilt, a one-section closed-form design run
//! as a state-variable section.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{Lanes, Ring, Sos, SosError, TAIL_FLOOR};
use tutti_graph::Shape;
use tutti_types::{BusTopology, MaxBlock};
use tutti_units::{Db, SampleRate, Samples, Tail};

use super::glided::{GLIDE_STEPS, GlidedSos, filter_shape};
use crate::kernel::{ControlLanes, Kernel};
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
        fn ring_out(&self, rate: SampleRate, floor: Db) -> Result<Ring, SosError>;

        /// The ring-out of every held controls at `rate`: a bound on `ring_out` over the
        /// ranges, proven from the designs or [`Ring::Unbounded`].
        ///
        /// # Errors
        ///
        /// The design's refusal of the ranges.
        fn ring_ceiling(rate: SampleRate, floor: Db) -> Result<Ring, SosError>;
    }
}

/// The controls of a one-section equalizer that [`EqKernel`] designs from: the bell's and
/// the tilt's, which are the only implementations.
pub trait EqControls:
    sealed::Sealed + Copy + core::fmt::Debug + PartialEq + Send + 'static
{
}

/// An equalizer of any width as a [`Kernel`]: the one section `C` designs, run as a
/// state-variable cascade whose coefficients glide.
///
/// The design is made on the stream's 16-frame grid and reached along a line in the section's
/// coefficients over the next 16 frames with the state kept. The tail is the ring-out bound
/// over every control, found in `prepare`; each block narrows it to the controls in force.
/// Made through the node aliases ([`BellFilterNode`](crate::BellFilterNode),
/// [`TiltFilterNode`](crate::TiltFilterNode)).
#[derive(Clone, Debug)]
pub struct EqKernel<C: EqControls, F: Float + Lanes> {
    input: BusTopology,
    sos: GlidedSos<F>,
    rate: SampleRate,
    /// The tail over every control at `rate`.
    ceiling: Tail,
    controls: core::marker::PhantomData<C>,
}

impl<C: EqControls, F: Float + Lanes> EqKernel<C, F> {
    /// A kernel on the bus `input`, its output where it is; at a placeholder 48 kHz until
    /// `prepare`, which an unprepared node renders nothing before.
    pub(super) fn new(input: BusTopology) -> Self {
        let mut kernel = Self {
            input,
            sos: GlidedSos::new(input),
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

    /// The ring-out over every control: `C::ring_ceiling`.
    fn ceiling_at(&self) -> Tail {
        match C::ring_ceiling(self.rate, TAIL_FLOOR) {
            Ok(Ring::Finite(t)) => {
                Tail::Finite(Samples((t.get() * self.rate.get()).ceil() as usize + 1))
            }
            _ => Tail::Unbounded,
        }
    }

    /// The ring-out at `controls`, read within their ranges.
    fn ring(&self, controls: &C) -> Tail {
        match controls.held(self.rate).ring_out(self.rate, TAIL_FLOOR) {
            Ok(Ring::Finite(t)) => {
                Tail::Finite(Samples((t.get() * self.rate.get()).ceil() as usize))
            }
            _ => Tail::Unbounded,
        }
    }
}

impl<C: EqControls, F: Float + Lanes> Kernel for EqKernel<C, F> {
    type Float = F;
    type Set = C::Set;

    const GRID: usize = GLIDE_STEPS as usize;

    fn shape(&self) -> Shape {
        filter_shape(self.input)
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
        self.sos.aim(design);
    }

    fn render(
        &mut self,
        input: Planes<'_, F>,
        _: ControlLanes<'_, F>,
        output: PlanesMut<'_, '_, F>,
    ) {
        self.sos.render(input, output);
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
