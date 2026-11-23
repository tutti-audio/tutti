//! [`DecorrelatorNode`]: velvet-noise decorrelation of every channel.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, Decorrelator, VelvetDensity, VelvetLength};
use tutti_graph::{Cost, Shape};
use tutti_units::{SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};
use crate::params::NoControls;

/// What a [`DecorrelatorNode`] is made from: its width and its sequences.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecorrelatorConfig {
    width: ChannelLayout,
    density: VelvetDensity,
    length: VelvetLength,
    seed: u64,
}

impl DecorrelatorConfig {
    /// A decorrelator of `width` channels at
    /// [`VelvetDensity::DEFAULT`] and [`VelvetLength::DEFAULT`], seed 0.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            density: VelvetDensity::DEFAULT,
            length: VelvetLength::DEFAULT,
            seed: 0,
        }
    }

    /// The impulses a second of each channel's sequence, from 1 to 20000.
    #[must_use]
    pub fn density(mut self, density: VelvetDensity) -> Self {
        self.density = density;
        self
    }

    /// How long each channel's sequence is, up to a second.
    #[must_use]
    pub fn length(mut self, length: VelvetLength) -> Self {
        self.length = length;
        self
    }

    /// The seed the channels' sequences are drawn from: each channel has its own.
    #[must_use]
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

/// A velvet-noise decorrelator of any width ([`Decorrelator`]): each channel through a sparse
/// FIR of `floor(length * density)` impulses of `+1` or `-1`, one in each period of
/// `rate / density` frames at a random place in it, under an envelope that falls 60 dB over the
/// length and scaled so white noise keeps its power.
///
/// The channels draw independent sequences from the seed, so one signal sent to every channel
/// comes out as that many decorrelated copies, whose correlation is the zero-lag inner product
/// of their sequences. The sequences are drawn at [`prepare`](Kernel::prepare), from the rate
/// the node runs at; a density past the rate is one impulse a frame. No latency (the first
/// impulse's delay is the decorrelation, not processing); the tail is the latest impulse. A
/// [`KernelNode`]. Made from a [`DecorrelatorConfig`] (`DecorrelatorNode::from`).
///
/// # Panics
///
/// `DecorrelatorNode::from` panics past 16 channels: a decorrelator is one bus.
pub type DecorrelatorNode<F> = KernelNode<DecorrelatorKernel<F>>;

impl<F: Float> From<DecorrelatorConfig> for DecorrelatorNode<F> {
    fn from(config: DecorrelatorConfig) -> Self {
        Self::with_kernel(DecorrelatorKernel::new(config), NoControls, ())
    }
}

/// The decorrelator as a [`Kernel`]: a [`Decorrelator`] drawn at the prepared rate.
#[derive(Clone, Debug)]
pub struct DecorrelatorKernel<F: Float> {
    config: DecorrelatorConfig,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<Decorrelator<F>>,
}

impl<F: Float> DecorrelatorKernel<F> {
    fn new(config: DecorrelatorConfig) -> Self {
        Self { config, bank: None }
    }
}

impl<F: Float> Kernel for DecorrelatorKernel<F> {
    type Float = F;
    type Set = NoControls;

    const GRID: usize = 16;

    /// `width` channels in and out, `Heavy`: a multiply-add for each impulse a frame, a
    /// hundred a channel at the default density.
    fn shape(&self) -> Shape {
        let bus = unassigned_bus(self.config.width, "a decorrelator");
        Shape::through(bus).cost(Cost::Heavy)
    }

    /// Draws the sequences at `rate`: a re-prepare empties the rings. `prepare` has no refusal
    /// path, so a density past the rate is lowered to it ([`VelvetDensity::clamped_to_rate`]:
    /// one impulse a frame); the bank refuses it only at a rate under 1 Hz.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        let c = self.config;
        let density = c.density.clamped_to_rate(rate);
        self.bank = Some(
            Decorrelator::new(c.width, density, c.length, rate, c.seed)
                .expect("a density at most the rate, at a rate of at least 1 Hz"),
        );
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, (): &()) {}

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

    fn tail(&self) -> Tail {
        self.bank
            .as_ref()
            .map_or(Tail::Unknown, |bank| Tail::Finite(bank.tail()))
    }
}

#[cfg(test)]
mod tests;
