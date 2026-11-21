//! [`FmNode`]: operators that modulate one another's phase, as an [`Algorithm`] says.
//!
//! The algorithm is `tutti_dsp::FmVoice`; the node holds the controls and the shape.

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{COEFF_INTERVAL, FmControls, FmPatch, FmVoice, Oversampling, OversamplingFactor};
use tutti_graph::Shape;
use tutti_units::{Amplitude, Hz, ModulationIndex, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelConfig, KernelNode};

/// What an [`FmNode`] is made from: its patch, its starting controls and the oversampling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FmConfig {
    patch: FmPatch,
    controls: FmControls,
    oversampling: OversamplingFactor,
}

impl FmConfig {
    /// A voice of `patch` at the default controls ([`FmControls::DEFAULT`]), run at twice the rate.
    #[must_use]
    pub const fn new(patch: FmPatch) -> Self {
        Self {
            patch,
            controls: FmControls::DEFAULT,
            oversampling: OversamplingFactor::X2,
        }
    }

    /// The frequency it starts at.
    #[must_use]
    pub const fn frequency(mut self, frequency: Hz) -> Self {
        self.controls.frequency = frequency;
        self
    }

    /// The output level it starts at.
    #[must_use]
    pub const fn level(mut self, level: Amplitude) -> Self {
        self.controls.level = level;
        self
    }

    /// The index of modulation it starts at.
    #[must_use]
    pub const fn index(mut self, index: ModulationIndex) -> Self {
        self.controls.index = index;
        self
    }

    /// The feedback it starts at.
    #[must_use]
    pub const fn feedback(mut self, feedback: ModulationIndex) -> Self {
        self.controls.feedback = feedback;
        self
    }

    /// Runs the operators at `factor` times the rate: the higher it is, the less of the
    /// sidebands that reach past the Nyquist fold back.
    #[must_use]
    pub const fn oversampling(mut self, factor: OversamplingFactor) -> Self {
        self.oversampling = factor;
        self
    }
}

/// A phase-modulation synthesiser: a source with no inputs and one (mono) output, the sum of
/// the carriers of an [`FmPatch`] ([`FmVoice`]).
///
/// Its params are the frequency ([`UnitParam::Frequency`](tutti_units::UnitParam::Frequency),
/// in [`Hz`], which each operator's ratio multiplies), the output level
/// ([`Volume`](tutti_units::UnitParam::Volume), in [`Amplitude`]), the index of modulation
/// (`Index` at index 0, in [`ModulationIndex`]) and the feedback (`Index` at index 1). The
/// operators start at phase 0, and their phases run on across blocks and changes of
/// frequency.
///
/// It reports no latency and no tail: it has no input, and the half-band filters' delay is a
/// fixed offset of the phase of what it plays ([`FmVoice::delay`]). The node renders silence
/// until the graph prepares it, and for a rate no half-band filter fits. A [`KernelNode`];
/// made from an [`FmConfig`] (`FmNode::from`).
pub type FmNode<F> = KernelNode<FmKernel<F>>;

impl<F: Float> From<FmConfig> for FmNode<F> {
    fn from(config: FmConfig) -> Self {
        KernelNode::from(
            KernelConfig::new(
                FmKernel {
                    patch: config.patch,
                    oversampling: Oversampling::new(config.oversampling),
                    voice: None,
                },
                FmSet,
            )
            .controls(config.controls),
        )
    }
}

/// The FM voice's controls: the frequency, the level, the index and the feedback.
#[derive(Clone, Copy, Debug)]
pub struct FmSet;

control_set! {
    FmSet(_s): FmControls = FmControls::DEFAULT;
    live: (all);
    Frequency => Hz = frequency,
    Volume => Amplitude = level,
    Index[0] => ModulationIndex = index,
    Index[1] => ModulationIndex = feedback;

    fn defaults(&self) -> FmControls {
        FmControls::DEFAULT
    }
}

/// The FM voice as a [`Kernel`]: an [`FmVoice`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct FmKernel<F: Float> {
    patch: FmPatch,
    oversampling: Oversampling,
    /// `None` until prepared, and at a rate no half-band filter fits.
    voice: Option<FmVoice<F>>,
}

impl<F: Float> Kernel for FmKernel<F> {
    type Float = F;
    type Set = FmSet;

    const GRID: usize = COEFF_INTERVAL;

    /// One mono output and no inputs; `Heavy` (the default cost): a sine for each operator
    /// at each frame of the oversampled rate.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }

    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock) {
        self.voice = self.patch.voice(self.oversampling, rate, max_block).ok();
    }

    fn reset(&mut self) {
        if let Some(voice) = &mut self.voice {
            voice.reset();
        }
    }

    fn retarget(&mut self, controls: &FmControls) {
        if let Some(voice) = &mut self.voice {
            voice.retarget(controls);
        }
    }

    fn render(
        &mut self,
        _: Planes<'_, F>,
        _: ControlSignals<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let mut out = output.plane_mut(0);
        match &mut self.voice {
            Some(voice) => voice.process(out),
            None => out.samples_mut().fill(F::ZERO),
        }
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests;
