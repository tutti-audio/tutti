//! [`AdditiveNode`]: a bank of sine partials at ratios of one frequency.
//!
//! The algorithm is `tutti_dsp::AdditiveBank`; the node holds the controls and the shape.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{AdditiveBank, AdditiveControls, BlockMismatch, COEFF_INTERVAL, Partials};
use tutti_graph::{Cost, Shape};
use tutti_units::{Amplitude, Hz, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelConfig, KernelNode};

/// What an [`AdditiveNode`] is made from: its partials and its starting controls.
#[derive(Clone, Debug, PartialEq)]
pub struct AdditiveConfig {
    partials: Partials,
    controls: AdditiveControls,
}

impl AdditiveConfig {
    /// A bank of `partials` at the default controls ([`AdditiveControls::DEFAULT`]).
    #[must_use]
    pub const fn new(partials: Partials) -> Self {
        Self {
            partials,
            controls: AdditiveControls::DEFAULT,
        }
    }

    /// The frequency it starts at.
    #[must_use]
    pub fn frequency(mut self, frequency: Hz) -> Self {
        self.controls.frequency = frequency;
        self
    }

    /// The output level it starts at.
    #[must_use]
    pub fn level(mut self, level: Amplitude) -> Self {
        self.controls.level = level;
        self
    }
}

/// An additive synthesiser: a source with no inputs and one (mono) output, the sum of
/// [`Partials`] ([`AdditiveBank`]), each a sine at its ratio of the frequency.
///
/// A partial at or above the Nyquist is dropped, so the output is band-limited by construction.
/// Its params are the frequency ([`UnitParam::Frequency`](tutti_units::UnitParam::Frequency),
/// in [`Hz`]) and the output level ([`Volume`](tutti_units::UnitParam::Volume), in
/// [`Amplitude`]). The partials start at phase 0, and their phases run on across blocks.
///
/// No latency and no tail. The node renders silence until the graph prepares it. A
/// [`KernelNode`]; made from an [`AdditiveConfig`] (`AdditiveNode::from`).
pub type AdditiveNode<F> = KernelNode<AdditiveKernel<F>>;

impl<F: Float> From<AdditiveConfig> for AdditiveNode<F> {
    fn from(config: AdditiveConfig) -> Self {
        KernelNode::from(
            KernelConfig::new(
                AdditiveKernel {
                    partials: config.partials,
                    bank: None,
                },
                AdditiveSet,
            )
            .controls(config.controls),
        )
    }
}

/// The additive bank's controls: the frequency and the level.
#[derive(Clone, Copy, Debug)]
pub struct AdditiveSet;

control_set! {
    AdditiveSet(_s): AdditiveControls = AdditiveControls::DEFAULT;
    live: (all);
    Frequency => Hz = frequency,
    Volume => Amplitude = level;

    fn defaults(&self) -> AdditiveControls {
        AdditiveControls::DEFAULT
    }
}

/// The additive bank as a [`Kernel`]: an [`AdditiveBank`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct AdditiveKernel<F: Float> {
    partials: Partials,
    /// `None` until prepared.
    bank: Option<AdditiveBank<F>>,
}

impl<F: Float> Kernel for AdditiveKernel<F> {
    type Float = F;
    type Set = AdditiveSet;

    const GRID: usize = COEFF_INTERVAL;

    /// One mono output and no inputs; a sine a frame for each partial, `Moderate`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = Some(AdditiveBank::new(self.partials.clone(), rate));
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &AdditiveControls) {
        if let Some(bank) = &mut self.bank {
            bank.retarget(controls);
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let (_, mut output) = block.split();
        let mut out = output.plane_mut(0);
        match &mut self.bank {
            Some(bank) => bank.process(out),
            None => out.samples_mut().fill(F::ZERO),
        }
        Ok(())
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests;
