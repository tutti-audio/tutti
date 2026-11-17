//! [`EqKernel`]: the kernel of the bell and the tilt, a one-section closed-form design run
//! as a state-variable section.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{Glide, MAX_SECTIONS, Ring, Sos, SosBank, SosCoefficients, SosError, TAIL_FLOOR};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_types::{BusTopology, Db, MaxBlock, SampleRate, Samples, Tail};

use crate::kernel::{ControlLanes, Kernel};
use crate::params::ControlSet;

/// The steps the coefficients take to a new design: one grid period.
const GLIDE_STEPS: u32 = 16;

/// The controls of a one-section equalizer that [`EqKernel`] designs from.
pub trait EqControls: Copy + core::fmt::Debug + PartialEq + Send + 'static {
    /// The control set that reads and writes these controls.
    type Set: ControlSet<Controls = Self>;

    /// The controls a kernel starts from.
    const DEFAULT: Self;

    /// These controls held within the ranges they are read in at `rate`.
    #[must_use]
    fn held(self, rate: SampleRate) -> Self;

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

    /// The ring-out of every held controls at `rate`: a bound on [`ring_out`](Self::ring_out)
    /// over the ranges, proven from the designs or [`Ring::Unbounded`].
    ///
    /// # Errors
    ///
    /// The design's refusal of the ranges.
    fn ring_ceiling(rate: SampleRate, floor: Db) -> Result<Ring, SosError>;
}

/// An equalizer of any width as a [`Kernel`]: a [`SosBank`] over the one section `C` designs,
/// and the glide of the coefficients every channel shares.
///
/// The design is made on the stream's 16-frame grid and reached along a line in the section's
/// coefficients over the next 16 frames with the state kept. The tail is the ring-out bound
/// over every control, [`EqControls::ring_ceiling`], found in `prepare`; each block narrows it to the controls in force. Made
/// through the node aliases ([`BellFilterNode`](crate::BellFilterNode),
/// [`TiltFilterNode`](crate::TiltFilterNode)).
#[derive(Clone, Debug)]
pub struct EqKernel<C: EqControls> {
    input: BusTopology,
    bank: SosBank<f64>,
    coefficients: Glide<SosCoefficients<f64>>,
    /// The design in progress, its room for a section held.
    scratch: Sos,
    rate: SampleRate,
    landed: bool,
    /// The tail over every control at `rate`.
    ceiling: Tail,
    controls: core::marker::PhantomData<C>,
}

impl<C: EqControls> EqKernel<C> {
    /// A kernel on the bus `input`, its output where it is; at a placeholder 48 kHz until
    /// `prepare`, which an unprepared node renders nothing before.
    pub(super) fn new(input: BusTopology) -> Self {
        let rate = SampleRate::SR_48K;
        let mut kernel = Self {
            input,
            bank: SosBank::new(input.layout()),
            coefficients: Glide::landed(SosCoefficients::THROUGH),
            scratch: Sos::new(Vec::with_capacity(MAX_SECTIONS), rate),
            rate,
            landed: false,
            ceiling: Tail::Unknown,
            controls: core::marker::PhantomData,
        };
        kernel.coefficients = Glide::landed(kernel.design(&C::DEFAULT));
        kernel.ceiling = kernel.ceiling_at();
        kernel
    }

    /// The coefficients at `controls`, read within their ranges; the design in force for
    /// controls that are not numbers. No allocation.
    fn design(&mut self, controls: &C) -> SosCoefficients<f64> {
        controls
            .held(self.rate)
            .design_into(self.rate, &mut self.scratch)
            .ok()
            .and_then(|()| SosCoefficients::new(&self.scratch))
            .unwrap_or_else(|| self.coefficients.target())
    }

    /// The ring-out over every control: [`EqControls::ring_ceiling`].
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

impl<C: EqControls> Kernel for EqKernel<C> {
    type Set = C::Set;

    const GRID: usize = GLIDE_STEPS as usize;

    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        Shape::buses(
            Buses::main(self.input),
            Buses::NONE.with(output).expect("one bus"),
        )
        .with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.scratch = Sos::new(Vec::with_capacity(MAX_SECTIONS), rate);
        self.coefficients = Glide::landed(self.design(&C::DEFAULT)).by(GLIDE_STEPS);
        self.landed = false;
        self.ceiling = self.ceiling_at();
    }

    fn reset(&mut self) {
        self.coefficients.settle();
        self.bank.reset();
    }

    fn retarget(&mut self, controls: &C) {
        let design = self.design(controls);
        self.coefficients.aim(design);
        if !self.landed {
            self.coefficients.settle();
            self.landed = true;
        }
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        self.bank.process(&mut self.coefficients, input, output);
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
