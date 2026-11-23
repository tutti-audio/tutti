//! [`ModalBankNode`]: a struck body as a bank of damped resonators, excited by its input.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, ModalBank, ModalControls, Modes};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`ModalBankNode`] is made from: its width and its modes.
#[derive(Clone, Debug, PartialEq)]
pub struct ModalBankConfig {
    width: ChannelLayout,
    modes: Modes,
}

impl ModalBankConfig {
    /// A bank of `modes` on each of `width` channels.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, modes: Modes) -> Self {
        Self {
            width: width.into(),
            modes,
        }
    }
}

/// A bank of damped resonators on every channel ([`ModalBank`]): each mode is the
/// impulse-invariant two-pole resonator of a frequency, a decay (the time to fall 60 dB) and a
/// gain, so a unit impulse in rings the sum of the modes' damped sinusoids. Any signal routed in
/// excites it. The output has no direct term, and a mode at or above Nyquist is silent.
///
/// The first mode's frequency (`UnitParam::Frequency`, a [`Hz`]) and decay (`UnitParam::Decay`,
/// a [`Seconds`]) are live cells that land whole on the stream grid (16 frames), and every
/// other mode keeps its ratio to the first's. They start at the first mode's.
///
/// No latency. Its tail is the slowest mode's ring-out to [`TAIL_FLOOR`](tutti_dsp::TAIL_FLOOR)
/// at the decay in force. A [`KernelNode`]. Made from a [`ModalBankConfig`]
/// (`ModalBankNode::from`).
///
/// # Panics
///
/// `ModalBankNode::from` panics past 16 channels, where the node's shape is built: a bank is
/// one bus.
pub type ModalBankNode<F> = KernelNode<ModalBankKernel<F>>;

impl<F: Float> From<ModalBankConfig> for ModalBankNode<F> {
    fn from(config: ModalBankConfig) -> Self {
        let start = ModalControls::of_first(&config.modes);
        Self::with_kernel(
            ModalBankKernel::new(config.width, config.modes),
            ModalBankSet { start },
            start,
        )
    }
}

/// The bank's controls: the first mode's frequency and decay.
#[derive(Clone, Copy, Debug)]
pub struct ModalBankSet {
    start: ModalControls,
}

control_set! {
    ModalBankSet(s): ModalControls = s.start;
    live: (all);
    Frequency => Hz = frequency { whole },
    Decay => Seconds = decay { whole };

    fn defaults(&self) -> ModalControls {
        self.start
    }
}

/// The bank as a [`Kernel`]: a [`ModalBank`] built at the prepared rate.
#[derive(Clone, Debug)]
pub struct ModalBankKernel<F: Float> {
    width: ChannelLayout,
    modes: Modes,
    controls: ModalControls,
    rate: SampleRate,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<ModalBank<F>>,
}

impl<F: Float> ModalBankKernel<F> {
    fn new(width: ChannelLayout, modes: Modes) -> Self {
        Self {
            width,
            controls: ModalControls::of_first(&modes),
            modes,
            rate: SampleRate::SR_48K,
            bank: None,
        }
    }
}

impl<F: Float> Kernel for ModalBankKernel<F> {
    type Float = F;
    type Set = ModalBankSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Heavy`: two multiply-adds per mode, sample and channel.
    fn shape(&self) -> Shape {
        let bus = ChannelTopology::unassigned(self.width)
            .expect("a modal bank is at most 16 channels wide");
        Shape::through(bus).cost(Cost::Heavy)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        let mut bank = ModalBank::new(&self.modes, self.width, rate);
        bank.retarget(&self.controls);
        self.bank = Some(bank);
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &ModalControls) {
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

    /// The bank's ring-out at an unbounded decay ratio is unbounded: the ceiling is
    /// [`Tail::Unbounded`].
    fn tail(&self) -> Tail {
        Tail::Unbounded
    }

    fn tail_of(&mut self, controls: &ModalControls) -> Tail {
        self.bank
            .as_ref()
            .map_or(Tail::Unknown, |bank| bank.tail(controls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_dsp::Mode;
    use tutti_graph::Node;
    use tutti_graph::contract::{assert_declared_tail, assert_param_fork, prepared};
    use tutti_units::{Gain, Latency, Samples, UnitParam};

    const RATE: SampleRate = SampleRate::SR_48K;

    fn modes() -> Modes {
        let modes: Vec<Mode> = [(440.0, 0.2, 1.0), (1_130.0, 0.1, -0.5)]
            .iter()
            .map(|&(f, t, g)| Mode {
                frequency: Hz::new(f),
                decay: Seconds::new(t),
                gain: Gain::new(g),
            })
            .collect();
        Modes::try_from(&modes[..]).unwrap()
    }

    fn node() -> ModalBankNode<f32> {
        ModalBankNode::from(ModalBankConfig::new(2u16, modes()))
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(node());
    }

    /// The shape is the width in and out, with no latency, and declares the first mode's
    /// frequency and decay with its values as their bases.
    #[test]
    fn the_shape_declares_the_first_modes_frequency_and_decay() {
        let n = prepared(node(), RATE, 64);
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
        assert_eq!((n.param_base(0), n.param_base(1)), (Some(440.0), Some(0.2)));
        assert!(
            Modes::try_from(&[][..]).is_err(),
            "a bank has at least one mode"
        );
    }

    /// The declared tail covers the ring-out.
    #[test]
    fn the_declared_tail_covers_the_ring_out() {
        let mut n = prepared(node(), RATE, 64);
        assert_declared_tail(&mut n, RATE);
    }
}
