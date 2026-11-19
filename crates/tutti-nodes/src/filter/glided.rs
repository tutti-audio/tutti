//! [`GlidedSos`]: a [`Sos`] design run as a state-variable cascade whose coefficients glide
//! between designs, the machinery under [`CutFilterKernel`](crate::CutFilterKernel) and
//! [`EqKernel`](crate::EqKernel).

use tutti_buffers::BusTopology;
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{Glide, MAX_SECTIONS, SimdLanes, Sos, SosBank, SosCoefficients, SosError};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::SampleRate;

/// The steps the coefficients take to a new design: one grid period.
pub(super) const GLIDE_STEPS: u32 = 16;

/// The shape of a filter on the bus `input`: its output where it is, `Moderate`.
pub(super) fn filter_shape(input: BusTopology) -> Shape {
    let output = Bus::banded(input, BusRole::Main, 1).expect("one band is a bus");
    Shape::buses(
        Buses::main(input),
        Buses::NONE.with(output).expect("one bus"),
    )
    .with_cost(Cost::Moderate)
}

/// The cascade of a [`Sos`] per channel, the design in progress and the glide of the
/// coefficients every channel shares.
#[derive(Clone, Debug)]
pub(super) struct GlidedSos<F: Float + SimdLanes> {
    bank: SosBank<F>,
    coefficients: Glide<SosCoefficients<F>>,
    /// The design in progress, its room for [`MAX_SECTIONS`] sections held.
    scratch: Sos,
    /// Whether the first design after `prepare` has landed.
    landed: bool,
}

impl<F: Float + SimdLanes> GlidedSos<F> {
    /// A silent cascade on the bus `input`, passing the input until a design is
    /// [`start`](Self::start)ed or [`restart`](Self::restart)ed.
    pub(super) fn new(input: BusTopology) -> Self {
        Self {
            bank: SosBank::new(input.layout()),
            coefficients: Glide::landed(SosCoefficients::THROUGH),
            scratch: Sos::new(Vec::with_capacity(MAX_SECTIONS), SampleRate::SR_48K),
            landed: false,
        }
    }

    /// The coefficients `design` makes into the design in progress, or the ones in force
    /// when it refuses or they do not fit. No allocation.
    pub(super) fn design(
        &mut self,
        design: impl FnOnce(&mut Sos) -> Result<(), SosError>,
    ) -> SosCoefficients<F> {
        design(&mut self.scratch)
            .ok()
            .and_then(|()| SosCoefficients::new(&self.scratch))
            .unwrap_or_else(|| self.coefficients.target())
    }

    /// The design in progress, as the last [`design`](Self::design) left it.
    pub(super) fn scratch(&self) -> &Sos {
        &self.scratch
    }

    /// Landed on `first`, where the node is built.
    pub(super) fn start(&mut self, first: SosCoefficients<F>) {
        self.coefficients = Glide::landed(first);
    }

    /// A design in progress for `rate`.
    pub(super) fn at_rate(&mut self, rate: SampleRate) {
        self.scratch = Sos::new(Vec::with_capacity(MAX_SECTIONS), rate);
    }

    /// Back to the first design: `first` lands at once, later ones glide.
    pub(super) fn restart(&mut self, first: SosCoefficients<F>) {
        self.coefficients = Glide::landed(first).by(GLIDE_STEPS);
        self.landed = false;
    }

    /// Back to silence at the design in force.
    pub(super) fn reset(&mut self) {
        self.coefficients.settle();
        self.bank.reset();
    }

    /// Glides to `design`; the first one after `prepare` lands at once.
    pub(super) fn aim(&mut self, design: SosCoefficients<F>) {
        self.coefficients.aim(design);
        if !self.landed {
            self.coefficients.settle();
            self.landed = true;
        }
    }

    /// Runs `input` into `output`.
    pub(super) fn render(&mut self, input: Planes<'_, F>, output: PlanesMut<'_, '_, F>) {
        self.bank.process(&mut self.coefficients, input, output);
    }
}
