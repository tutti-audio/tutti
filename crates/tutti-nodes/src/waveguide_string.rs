//! [`WaveguideStringNode`]: a string as two travelling waves, excited by its input.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, LowestPitch, WaveguideBank, WaveguideControls,
    WaveguidePositions,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, Normalized, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};

/// What a [`WaveguideStringNode`] is made from: its width, the lowest pitch it plays and where its
/// controls start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveguideStringConfig {
    width: ChannelLayout,
    lowest: LowestPitch,
    positions: WaveguidePositions,
    controls: WaveguideControls,
}

impl WaveguideStringConfig {
    /// A string on each of `width` channels, down to 20 Hz,
    /// excited and read at [`WaveguidePositions::DEFAULT`], at [`WaveguideControls::DEFAULT`]
    /// (220 Hz, a decay of 2 s).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            lowest: LowestPitch::DEFAULT,
            positions: WaveguidePositions::DEFAULT,
            controls: WaveguideControls::DEFAULT,
        }
    }

    /// Where along the string, from the nut, the input enters.
    #[must_use]
    pub fn excitation(mut self, position: Normalized) -> Self {
        self.positions.excitation = position;
        self
    }

    /// Where along the string, from the nut, the output is read.
    #[must_use]
    pub fn pickup(mut self, position: Normalized) -> Self {
        self.positions.pickup = position;
        self
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

/// A string of two travelling waves on every channel ([`WaveguideBank`]): two rails of delay
/// carry the waves between a nut and a bridge that each reflect with an inversion, a gain and a
/// two-point average. The input enters at the excitation position and the output is the sum of
/// the waves at the pickup position (each a fraction of the string from the nut, set in the
/// config and rounded to a whole frame of the rail). A pickup at a node of a mode leaves that
/// mode out.
///
/// The pitch is `fs / (2 M + 1 + d)` exactly, the rails' frames, the ends' frame and the
/// all-pass's phase delay at the pitch. The pitch (`UnitParam::Frequency`, a [`Hz`]) and the
/// decay (`UnitParam::Decay`, a [`Seconds`], the time a round trip's gain takes to fall 60 dB) are
/// live cells that land whole on the stream grid (16 frames). A pitch above an eighth of the
/// rate or below the lowest is held there.
///
/// No latency. Its tail is unbounded as a ceiling and the loop's ring-out to
/// [`TAIL_FLOOR`](tutti_dsp::TAIL_FLOOR) at the controls in force. A [`KernelNode`]. Made from a [`WaveguideStringConfig`]
/// (`WaveguideStringNode::from`).
///
/// # Panics
///
/// `WaveguideStringNode::from` panics past 16 channels, where the node's shape is built: a string
/// is one bus.
pub type WaveguideStringNode<F> = KernelNode<WaveguideStringKernel<F>>;

impl<F: Float> From<WaveguideStringConfig> for WaveguideStringNode<F> {
    /// The line is sized at the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    fn from(config: WaveguideStringConfig) -> Self {
        Self::with_kernel(
            WaveguideStringKernel::new(config.width, config.lowest, config.positions),
            WaveguideStringSet,
            config.controls,
        )
    }
}

/// The string's controls: the pitch and the decay.
#[derive(Clone, Copy, Debug)]
pub struct WaveguideStringSet;

control_set! {
    WaveguideStringSet(_s): WaveguideControls = WaveguideControls::DEFAULT;
    live: (all);
    Frequency => Hz = frequency { whole },
    Decay => Seconds = decay { whole };

    fn defaults(&self) -> WaveguideControls {
        WaveguideControls::DEFAULT
    }
}

/// The string as a [`Kernel`]: a [`WaveguideBank`] sized at the prepared rate.
#[derive(Clone, Debug)]
pub struct WaveguideStringKernel<F: Float> {
    width: ChannelLayout,
    lowest: LowestPitch,
    positions: WaveguidePositions,
    controls: WaveguideControls,
    rate: SampleRate,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<WaveguideBank<F>>,
}

impl<F: Float> WaveguideStringKernel<F> {
    fn new(width: ChannelLayout, lowest: LowestPitch, positions: WaveguidePositions) -> Self {
        Self {
            width,
            lowest,
            positions,
            controls: WaveguideControls::DEFAULT,
            rate: SampleRate::SR_48K,
            bank: None,
        }
    }
}

impl<F: Float> Kernel for WaveguideStringKernel<F> {
    type Float = F;
    type Set = WaveguideStringSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Moderate`: two rings, an all-pass and two averages per
    /// sample and channel.
    fn shape(&self) -> Shape {
        let bus = unassigned_bus(self.width, "a string");
        Shape::through(bus).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        let mut bank = WaveguideBank::new(self.positions, self.lowest, rate, self.width);
        bank.retarget(&self.controls);
        self.bank = Some(bank);
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &WaveguideControls) {
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

    fn tail_of(&mut self, controls: &WaveguideControls) -> Tail {
        self.bank
            .as_ref()
            .map_or(Tail::Unknown, |bank| bank.tail(controls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::Node;
    use tutti_graph::contract::{assert_declared_tail, prepared};
    use tutti_units::{Latency, Samples, UnitParam};

    const RATE: SampleRate = SampleRate::SR_48K;

    fn node(frequency: f64, decay: f64) -> WaveguideStringNode<f32> {
        WaveguideStringNode::from(
            WaveguideStringConfig::new(2u16)
                .frequency(Hz::new(frequency))
                .decay(Seconds::new(decay)),
        )
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
