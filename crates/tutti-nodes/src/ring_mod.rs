//! [`RingModNode`]: the input multiplied by a sine carrier.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, RingModControls, RingModulator};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};

/// What a [`RingModNode`] is made from: its width and its starting carrier frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingModConfig {
    width: ChannelLayout,
    controls: RingModControls,
}

impl RingModConfig {
    /// A ring modulator of `width` channels at [`RingModControls::DEFAULT`] (440 Hz).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
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
        // A placeholder rate: an unprepared node renders nothing.
        let kernel = RingModKernel {
            width: config.width,
            modulator: RingModulator::new(SampleRate::SR_48K),
            float: core::marker::PhantomData,
        };
        Self::with_kernel(kernel, RingModSet, config.controls)
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

impl<F: Float> Kernel for RingModKernel<F> {
    type Float = F;
    type Set = RingModSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Moderate`: a sine per sample and channel.
    fn shape(&self) -> Shape {
        let bus = unassigned_bus(self.width, "a ring modulator");
        Shape::through(bus).cost(Cost::Moderate)
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
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.modulator.process(block)
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
    use tutti_units::Tail;
    use tutti_units::UnitParam;

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(RingModNode::from(RingModConfig::new(2u16)));
    }

    /// The shape is the width in and out, `Moderate`, with no tail, and declares the carrier
    /// as `Frequency`; the default is 440 Hz.
    #[test]
    fn the_shape_declares_the_frequency() {
        let n = prepared(RingModNode::from(RingModConfig::new(3u16)));
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

    /// Every channel of a three-wide node is processed: a constant input times the carrier
    /// `sin(2 pi 440 n / fs)` is nonzero on each channel from frame 1 on.
    #[test]
    fn every_channel_of_a_three_wide_node_is_modulated() {
        let mut n = prepared(RingModNode::from(RingModConfig::new(3u16)));
        let ones = [1.0f32; 64];
        let out = crate::test_support::drive_block(&mut n, &[&ones, &ones, &ones]);
        assert_eq!(out.len(), 3);
        for (c, ch) in out.iter().enumerate() {
            assert!(
                ch[1..].iter().any(|v| v.abs() > 0.1),
                "channel {c} is silent"
            );
        }
    }

    /// An empty layout is no channels, not one.
    #[test]
    fn an_empty_layout_has_no_channels() {
        let n = RingModNode::<f32>::from(RingModConfig::new(ChannelLayout::EMPTY));
        assert_eq!(n.shape().audio_in().count(), 0);
    }
}
