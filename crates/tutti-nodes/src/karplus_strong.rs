//! [`KarplusStrongNode`]: a plucked string excited by its input.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, KarplusStrongBank, KarplusStrongControls, LowestPitch,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`KarplusStrongNode`] is made from: its width, the lowest pitch it plays and where its
/// controls start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KarplusStrongConfig {
    width: ChannelLayout,
    lowest: LowestPitch,
    controls: KarplusStrongControls,
}

impl KarplusStrongConfig {
    /// A string on each of `width` channels, down to 20 Hz,
    /// at [`KarplusStrongControls::DEFAULT`] (220 Hz, a decay of 2 s).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            lowest: LowestPitch::DEFAULT,
            controls: KarplusStrongControls::DEFAULT,
        }
    }

    /// The lowest pitch the string plays: it sizes the delay line.
    #[must_use]
    pub fn lowest(mut self, lowest: LowestPitch) -> Self {
        self.lowest = lowest;
        self
    }

    /// The pitch it starts at.
    #[must_use]
    pub fn frequency(mut self, frequency: Hz) -> Self {
        self.controls.frequency = frequency;
        self
    }

    /// The decay time it starts at.
    #[must_use]
    pub fn decay(mut self, decay: Seconds) -> Self {
        self.controls.decay = decay;
        self
    }
}

/// A plucked string on every channel ([`KarplusStrongBank`]): the input is summed into a loop of
/// a delay line, a fractional all-pass and a two-point average, and the loop's sum is the
/// output. Any signal routed in excites it: an impulse or a burst of noise is a pluck.
///
/// The pitch is `fs / (N + 1/2 + d)` exactly, the loop's whole frames, the average's half frame
/// and the all-pass's phase delay at the pitch. The pitch (`UnitParam::Frequency`, a [`Hz`]) and
/// the decay (`UnitParam::Decay`, a [`Seconds`], the time the loop's gain takes to fall 60 dB)
/// are live cells that land whole on the stream grid (16 frames). A pitch above a sixth of the
/// rate or below the lowest is held there.
///
/// No latency. Its tail is unbounded as a ceiling and the loop's ring-out to
/// [`TAIL_FLOOR`](tutti_dsp::TAIL_FLOOR) at the controls in force. A [`KernelNode`]. Made from a [`KarplusStrongConfig`]
/// (`KarplusStrongNode::from`).
///
/// # Panics
///
/// `KarplusStrongNode::from` panics past 16 channels, where the node's shape is built: a string
/// is one bus.
pub type KarplusStrongNode<F> = KernelNode<KarplusStrongKernel<F>>;

impl<F: Float> From<KarplusStrongConfig> for KarplusStrongNode<F> {
    /// The line is sized at the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    fn from(config: KarplusStrongConfig) -> Self {
        Self::with_kernel(
            KarplusStrongKernel::new(config.width, config.lowest),
            KarplusStrongSet,
            config.controls,
        )
    }
}

/// The string's controls: the pitch and the decay.
#[derive(Clone, Copy, Debug)]
pub struct KarplusStrongSet;

control_set! {
    KarplusStrongSet(_s): KarplusStrongControls = KarplusStrongControls::DEFAULT;
    live: (all);
    Frequency => Hz = frequency { whole },
    Decay => Seconds = decay { whole };

    fn defaults(&self) -> KarplusStrongControls {
        KarplusStrongControls::DEFAULT
    }
}

/// The string as a [`Kernel`]: a [`KarplusStrongBank`] sized at the prepared rate.
#[derive(Clone, Debug)]
pub struct KarplusStrongKernel<F: Float> {
    width: ChannelLayout,
    lowest: LowestPitch,
    controls: KarplusStrongControls,
    rate: SampleRate,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<KarplusStrongBank<F>>,
}

impl<F: Float> KarplusStrongKernel<F> {
    fn new(width: ChannelLayout, lowest: LowestPitch) -> Self {
        Self {
            width,
            lowest,
            controls: KarplusStrongControls::DEFAULT,
            rate: SampleRate::SR_48K,
            bank: None,
        }
    }
}

impl<F: Float> Kernel for KarplusStrongKernel<F> {
    type Float = F;
    type Set = KarplusStrongSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Moderate`: a ring, an all-pass and a multiply-add per
    /// sample and channel.
    fn shape(&self) -> Shape {
        let bus =
            BusTopology::unassigned(self.width).expect("a string is at most 16 channels wide");
        Shape::through(bus).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        let mut bank = KarplusStrongBank::new(self.lowest, rate, self.width);
        bank.retarget(&self.controls);
        self.bank = Some(bank);
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &KarplusStrongControls) {
        self.controls = *controls;
        if let Some(bank) = &mut self.bank {
            bank.retarget(controls);
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        match &mut self.bank {
            Some(bank) => bank.process(block),
            None => Ok(()),
        }
    }

    /// A decay is any time, so the ceiling is unbounded; [`tail_of`](Kernel::tail_of) narrows it
    /// to the ring-out at the controls in force.
    fn tail(&self) -> Tail {
        Tail::Unbounded
    }

    fn tail_of(&mut self, controls: &KarplusStrongControls) -> Tail {
        self.bank
            .as_ref()
            .map_or(Tail::Unknown, |bank| bank.tail(controls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::Node;
    use tutti_graph::contract::{assert_declared_tail, assert_param_fork, prepared};
    use tutti_units::{Latency, Samples, UnitParam};

    const RATE: SampleRate = SampleRate::SR_48K;

    fn node(frequency: f64, decay: f64) -> KarplusStrongNode<f32> {
        KarplusStrongNode::from(
            KarplusStrongConfig::new(2u16)
                .frequency(Hz::new(frequency))
                .decay(Seconds::new(decay)),
        )
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(node(440.0, 1.0));
    }

    /// The shape is the width in and out, with no latency, and declares the pitch and the decay
    /// with the config's values as their bases.
    #[test]
    fn the_shape_declares_the_pitch_and_the_decay_and_no_latency() {
        let n = prepared(node(440.0, 1.5), RATE, 64);
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.latency, Latency::new(Samples::ZERO));
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Frequency, UnitParam::Decay][..]
        );
        assert_eq!((n.param_base(0), n.param_base(1)), (Some(440.0), Some(1.5)));
        assert_eq!(shape.tail, Tail::Unbounded);
    }

    /// The declared tail covers the ring-out.
    #[test]
    fn the_declared_tail_covers_the_ring_out() {
        let mut n = prepared(node(440.0, 0.2), RATE, 64);
        assert_declared_tail(&mut n, RATE);
    }
}
