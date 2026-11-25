//! [`RampedSos`]: a [`Sos`] design run as a state-variable cascade whose coefficients ramp
//! between designs, the machinery under [`CutFilterKernel`](crate::CutFilterKernel) and
//! [`EqKernel`](crate::EqKernel).

use tutti_buffers::ChannelTopology;
use tutti_buffers::{Float, InputOutput};
use tutti_dsp::BlockMismatch;
use tutti_dsp::{MAX_SECTIONS, Ramp, SimdLanes, Sos, SosBank, SosCoefficients, SosError};
use tutti_units::SampleRate;

/// The steps the coefficients take to a new design: one grid period.
pub(super) const RAMP_STEPS: u32 = 16;

/// The cascade of a [`Sos`] per channel, the design in progress and the ramp of the
/// coefficients every channel shares.
#[derive(Clone, Debug)]
pub(super) struct RampedSos<F: Float + SimdLanes> {
    bank: SosBank<F>,
    coefficients: Ramp<SosCoefficients<F>>,
    /// The design in progress, its room for [`MAX_SECTIONS`] sections held.
    scratch: Sos,
}

impl<F: Float + SimdLanes> RampedSos<F> {
    /// A silent cascade on the bus `input`, passing the input until a design is
    /// [`start`](Self::start)ed or [`restart`](Self::restart)ed.
    pub(super) fn new(input: ChannelTopology) -> Self {
        Self {
            bank: SosBank::new(input.width()),
            coefficients: Ramp::new(SosCoefficients::THROUGH),
            scratch: Sos::new(Vec::with_capacity(MAX_SECTIONS), SampleRate::SR_48K),
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

    /// At rest on `first`, where the node is built.
    pub(super) fn start(&mut self, first: SosCoefficients<F>) {
        self.coefficients = Ramp::new(first);
    }

    /// A design in progress for `rate`.
    pub(super) fn at_rate(&mut self, rate: SampleRate) {
        self.scratch = Sos::new(Vec::with_capacity(MAX_SECTIONS), rate);
    }

    /// Back to `first` at rest, with later designs ramping over [`RAMP_STEPS`].
    pub(super) fn restart(&mut self, first: SosCoefficients<F>) {
        self.coefficients = Ramp::new(first).steps(RAMP_STEPS);
    }

    /// Back to silence.
    pub(super) fn reset(&mut self) {
        self.bank.reset();
    }

    /// Ramps to `design`.
    pub(super) fn retarget(&mut self, design: SosCoefficients<F>) {
        self.coefficients.retarget(design);
    }

    /// The coefficients on the design at once.
    pub(super) fn jump_to_target(&mut self) {
        self.coefficients.jump_to_target();
    }

    /// Runs the block's input into its output.
    pub(super) fn render(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        self.bank.process(&mut self.coefficients, block)
    }
}
