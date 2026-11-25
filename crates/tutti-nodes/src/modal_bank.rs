//! [`ModalBankNode`]: a struck body as a bank of damped resonators, excited by its input.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, ModalBank, ModalControls, Modes};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};

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
    Frequency => Hz = frequency { whole, no_ring },
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
        let bus = unassigned_bus(self.width, "a modal bank");
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

    fn jump_to_target(&mut self) {}

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
    use crate::test_support::reported_tail;
    use tutti_dsp::Mode;
    use tutti_graph::ParamNode;
    use tutti_graph::contract::{assert_declared_tail, prepared};
    use tutti_units::{Gain, UnitParam};

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

    /// A bank has at least one mode.
    #[test]
    fn a_bank_has_at_least_one_mode() {
        assert!(Modes::try_from(&[][..]).is_err());
    }

    /// The tail reads the decay and no frequency: moving the frequency alone leaves the
    /// reported tail, which is what its `no_ring` flag promises.
    #[test]
    fn the_frequency_does_not_change_the_reported_tail() {
        let impulse = tutti_fixtures::signal::impulse::<f32>(tutti_units::Samples::new(64));
        let reported = |hz: f64| {
            let n = node();
            assert!(n.param_set().set(UnitParam::Frequency, hz));
            let mut n = prepared(n, RATE, 64);
            reported_tail(&mut n, &[&impulse, &impulse])
        };
        let set = ModalBankSet {
            start: ModalControls {
                frequency: Hz::new(440.0),
                decay: Seconds::new(1.0),
            },
        };
        assert_eq!(
            crate::test_support::non_ringing(&set),
            [tutti_units::UnitAddr::first(UnitParam::Frequency)]
        );
        let base = reported(440.0);
        assert!(matches!(base, Some(Tail::Finite(_))), "{base:?}");
        for hz in [20.0, 100.0, 5_000.0, 20_000.0] {
            assert_eq!(reported(hz), base, "{hz} Hz");
        }
    }

    /// The declared tail covers the ring-out.
    #[test]
    fn the_declared_tail_covers_the_ring_out() {
        let mut n = prepared(node(), RATE, 64);
        assert_declared_tail(&mut n, RATE);
    }
}
