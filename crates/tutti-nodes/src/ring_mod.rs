//! [`RingModNode`]: the input multiplied by a sine carrier.

use core::num::NonZeroU16;
use tutti_buffers::{BusTopology, ChannelLayout, MaxBlock};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{COEFF_INTERVAL, RingModControls, RingModulator};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`RingModNode`] is made from: its width and its starting carrier frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingModConfig {
    width: NonZeroU16,
    controls: RingModControls,
}

impl RingModConfig {
    /// A ring modulator of `width` channels at [`RingModControls::DEFAULT`] (440 Hz).
    #[must_use]
    pub fn new(width: NonZeroU16) -> Self {
        Self {
            width,
            controls: RingModControls::DEFAULT,
        }
    }

    /// The carrier frequency it starts at.
    #[must_use]
    pub fn frequency(mut self, frequency: Hz) -> Self {
        self.controls.frequency = frequency;
        self
    }
}

/// A ring modulator of any width: `y = x sin(2 pi fc t)` on every channel, for
/// the carrier frequency `fc` ([`RingModulator`]).
///
/// A sine input at `f1` comes out as `f1 + fc` and `f1 - fc` with nothing at
/// `f1`. The carrier starts at phase 0 and its phase runs on across
/// blocks and frequency changes, so a change never jumps it. The frequency
/// (`UnitParam::Frequency`, a [`Hz`]) is a live cell that lands whole; a value that is not
/// finite reads as the last one that was. Wet/dry is the graph's blend of any node
/// (`NodeMix`), not a control of this one.
///
/// No latency and no tail. A [`KernelNode`]. Made from a [`RingModConfig`]
/// (`RingModNode::from`).
///
/// # Panics
///
/// `RingModNode::from` panics past 16 channels, where the node's shape is built: a ring modulator is one bus.
pub type RingModNode<F> = KernelNode<RingModKernel<F>>;

impl<F: Float> From<RingModConfig> for RingModNode<F> {
    fn from(config: RingModConfig) -> Self {
        Self::with_kernel(
            RingModKernel::new(ChannelLayout::from_count(config.width.get())),
            RingModSet,
            config.controls,
        )
    }
}

/// The ring modulator's control: the carrier frequency.
#[derive(Clone, Copy, Debug)]
pub struct RingModSet;

control_set! {
    RingModSet(_s): RingModControls = RingModControls::DEFAULT;
    live: (all);
    Frequency => Hz = frequency { whole };

    fn defaults(&self) -> RingModControls {
        RingModControls::DEFAULT
    }
}

/// The ring modulator as a [`Kernel`]: a [`RingModulator`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct RingModKernel<F: Float> {
    width: ChannelLayout,
    modulator: RingModulator,
    float: core::marker::PhantomData<fn() -> F>,
}

impl<F: Float> RingModKernel<F> {
    /// A modulator at a placeholder rate: an unprepared node renders nothing.
    fn new(width: ChannelLayout) -> Self {
        Self {
            width,
            modulator: RingModulator::new(SampleRate::SR_48K),
            float: core::marker::PhantomData,
        }
    }
}

impl<F: Float> Kernel for RingModKernel<F> {
    type Float = F;
    type Set = RingModSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Moderate`: a sine per sample and channel.
    fn shape(&self) -> Shape {
        let bus = BusTopology::unassigned(self.width)
            .expect("a ring modulator is at most 16 channels wide");
        Shape::through(bus).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.modulator = RingModulator::new(rate);
    }

    fn reset(&mut self) {
        self.modulator.reset();
    }

    fn retarget(&mut self, controls: &RingModControls) {
        self.modulator.retarget(controls);
    }

    fn render(
        &mut self,
        input: Planes<'_, F>,
        _: ControlSignals<'_, F>,
        output: PlanesMut<'_, '_, F>,
    ) {
        self.modulator.process(input, output);
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prepared;
    use tutti_graph::Node;
    use tutti_graph::contract::assert_param_fork;
    use tutti_units::UnitParam;

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(RingModNode::from(RingModConfig::new(
            NonZeroU16::new(2).unwrap(),
        )));
    }

    /// The shape is the width in and out, `Moderate`, with no tail, and declares the carrier
    /// as `Frequency`; the default is 440 Hz.
    #[test]
    fn the_shape_declares_the_frequency() {
        let n = prepared(RingModNode::from(RingModConfig::new(
            NonZeroU16::new(3).unwrap(),
        )));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Moderate);
        assert_eq!(shape.tail, Tail::None);
        assert_eq!(shape.params.as_slice(), &[UnitParam::Frequency][..]);
        assert_eq!(n.param_base(0), Some(440.0));
    }

    /// A width of one is one channel in and out.
    #[test]
    fn a_width_of_one_is_mono() {
        let n = RingModNode::<f32>::from(RingModConfig::new(NonZeroU16::MIN));
        assert_eq!(n.shape().audio_in().count(), 1);
    }
}
