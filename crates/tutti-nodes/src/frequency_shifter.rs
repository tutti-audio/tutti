//! [`FrequencyShifterNode`]: single-sideband modulation by a shift in hertz.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, FrequencyShifterBank, FrequencyShifterControls};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`FrequencyShifterNode`] is made from: its width and its starting shift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrequencyShifterConfig {
    width: ChannelLayout,
    controls: FrequencyShifterControls,
}

impl FrequencyShifterConfig {
    /// A frequency shifter of `width` channels at
    /// [`FrequencyShifterControls::DEFAULT`] (no shift).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: FrequencyShifterControls::DEFAULT,
        }
    }

    /// The shift it starts at: up when positive, down when negative.
    #[must_use]
    pub fn shift(mut self, shift: Hz) -> Self {
        self.controls.shift = shift;
        self
    }
}

/// A frequency shifter of any width: every frequency moves by the same number of hertz, so
/// harmonics stop being harmonic ([`FrequencyShifterBank`]).
///
/// A sine at `f` comes out at `f + s` for a shift `s` (at `|f + s|` when it is negative), with
/// an image at `s - f` under `tan(e / 2)` of it for the network's quadrature error `e`
/// ([`QUADRATURE_ERROR`](tutti_dsp::QUADRATURE_ERROR), 0.7 degrees: -44 dB) across the band
/// [`quadrature_band`](tutti_dsp::quadrature_band). The shift (`UnitParam::Frequency`, a
/// [`Hz`], signed) is a live cell that lands whole on the stream grid (16 frames), and the
/// carrier's phase runs on across blocks and shift changes, so a change never jumps it. Wet/dry
/// is the graph's blend of any node (`NodeMix`).
///
/// The network is a phase network, so no latency; it rings for the slowest pole's decay
/// ([`FrequencyShifterBank::tail`]). A [`KernelNode`]. Made from a [`FrequencyShifterConfig`]
/// (`FrequencyShifterNode::from`).
///
/// # Panics
///
/// `FrequencyShifterNode::from` panics past 16 channels, where the node's shape is built: a
/// shifter is one bus.
pub type FrequencyShifterNode<F> = KernelNode<FrequencyShifterKernel<F>>;

impl<F: Float> From<FrequencyShifterConfig> for FrequencyShifterNode<F> {
    fn from(config: FrequencyShifterConfig) -> Self {
        Self::with_kernel(
            FrequencyShifterKernel::new(config.width),
            FrequencyShifterSet,
            config.controls,
        )
    }
}

/// The shifter's control: the shift.
#[derive(Clone, Copy, Debug)]
pub struct FrequencyShifterSet;

control_set! {
    FrequencyShifterSet(_s): FrequencyShifterControls = FrequencyShifterControls::DEFAULT;
    live: (all);
    Frequency => Hz = shift { whole };

    fn defaults(&self) -> FrequencyShifterControls {
        FrequencyShifterControls::DEFAULT
    }
}

/// The shifter as a [`Kernel`]: a [`FrequencyShifterBank`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct FrequencyShifterKernel<F: Float> {
    width: ChannelLayout,
    bank: FrequencyShifterBank<F>,
}

impl<F: Float> FrequencyShifterKernel<F> {
    /// A shifter at a placeholder rate: an unprepared node renders nothing.
    fn new(width: ChannelLayout) -> Self {
        Self {
            width,
            bank: FrequencyShifterBank::new(width, SampleRate::SR_48K),
        }
    }
}

impl<F: Float> Kernel for FrequencyShifterKernel<F> {
    type Float = F;
    type Set = FrequencyShifterSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Moderate`: eight all-passes and two sines per sample.
    fn shape(&self) -> Shape {
        let bus = BusTopology::unassigned(self.width)
            .expect("a frequency shifter is at most 16 channels wide");
        Shape::through(bus).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = FrequencyShifterBank::new(self.width, rate);
    }

    fn reset(&mut self) {
        self.bank.reset();
    }

    fn retarget(&mut self, controls: &FrequencyShifterControls) {
        self.bank.retarget(controls);
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let (input, output) = block.split();
        self.bank.process(input, output);
        Ok(())
    }

    fn tail(&self) -> Tail {
        self.bank.tail()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::Node;
    use tutti_graph::contract::{assert_declared_tail, assert_param_fork, prepared};
    use tutti_units::{Latency, Samples, UnitParam};

    const RATE: SampleRate = SampleRate::SR_48K;

    /// A fork starts from the shift last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(FrequencyShifterNode::from(
            FrequencyShifterConfig::new(2u16).shift(Hz::new(-100.0)),
        ));
    }

    /// The shape is the width in and out with no latency and a finite tail, and declares the
    /// shift as `Frequency`, which starts as configured.
    #[test]
    fn the_shape_declares_the_shift_and_a_finite_tail() {
        let n = prepared(
            FrequencyShifterNode::<f32>::from(
                FrequencyShifterConfig::new(3u16).shift(Hz::new(-12.5)),
            ),
            RATE,
            64,
        );
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.latency, Latency::new(Samples::ZERO));
        assert!(matches!(shape.tail, Tail::Finite(_)));
        assert_eq!(shape.params.as_slice(), &[UnitParam::Frequency][..]);
        assert_eq!(n.param_base(0), Some(-12.5));
        let empty =
            FrequencyShifterNode::<f32>::from(FrequencyShifterConfig::new(ChannelLayout::EMPTY));
        assert_eq!(empty.shape().audio_in().count(), 0);
    }

    /// The declared tail covers the ring-out.
    #[test]
    fn the_declared_tail_covers_the_ring_out() {
        let mut n = prepared(
            FrequencyShifterNode::<f32>::from(FrequencyShifterConfig::new(1u16)),
            RATE,
            64,
        );
        assert_declared_tail(&mut n, RATE);
    }
}
