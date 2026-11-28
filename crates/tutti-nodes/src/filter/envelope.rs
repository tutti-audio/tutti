//! [`EnvelopeFilterNode`]: a state-variable filter whose cutoff follows the signal's level.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, EnvelopeFilterBank, EnvelopeFilterControls, Lerp, SimdLanes,
    SvfType,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, Q, SampleRate, Seconds, Semitones, Tail};

use super::filter_bus;
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What an [`EnvelopeFilterNode`] is made from: its width, its base cutoff and response, and
/// optionally its depth, resonance and envelope times.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeFilterConfig {
    width: ChannelLayout,
    ty: SvfType,
    controls: EnvelopeFilterControls,
}

impl EnvelopeFilterConfig {
    /// A filter of `width` channels and response `ty` whose cutoff is `base` for a silent
    /// input, at depth 0 (a fixed filter) until [`depth`](Self::depth) opens it, Butterworth,
    /// with a 10 ms attack and a 100 ms release.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, base: Hz, ty: SvfType) -> Self {
        Self {
            width: width.into(),
            ty,
            controls: EnvelopeFilterControls {
                base,
                ..EnvelopeFilterControls::DEFAULT
            },
        }
    }

    /// How far the cutoff moves at a full-scale envelope, in semitones (12 to the octave), up
    /// when positive. Read within [`EnvelopeFilterControls::MAX_DEPTH`].
    #[must_use]
    pub fn depth(mut self, depth: Semitones) -> Self {
        self.controls.depth = depth;
        self
    }

    /// The resonance of the filter.
    #[must_use]
    pub fn q(mut self, q: Q) -> Self {
        self.controls.q = q;
        self
    }

    /// The envelope's attack time constant.
    #[must_use]
    pub fn attack(mut self, time: Seconds) -> Self {
        self.controls.attack = time;
        self
    }

    /// The envelope's release time constant.
    #[must_use]
    pub fn release(mut self, time: Seconds) -> Self {
        self.controls.release = time;
        self
    }
}

/// An envelope filter of any width: a state-variable filter ([`SvfType`]) on every channel
/// at one cutoff, `base * 2^(depth / 12 * env)`, where `env` is the level of the loudest
/// channel followed with an attack and a release, in `[0, 1]` (0 silent, 1 at full scale).
///
/// The cutoff is designed once per 16 frames of the stream at the envelope then, and the
/// coefficients ramp to it as the SVF's do ([`EnvelopeFilterBank`]). At depth 0 it is a fixed
/// SVF at the base.
///
/// The base (`UnitParam::Cutoff`), the resonance (`UnitParam::Q`), the envelope's attack and
/// release (`UnitParam::Attack`, `UnitParam::Release`) and the depth (`UnitParam::Semitones`)
/// are live cells, sampled at the grid; the response is the node's build. The tail is [`Tail::Unknown`]: a
/// resonant filter rings for as long as its Q says. No latency. A [`KernelNode`]; made from
/// an [`EnvelopeFilterConfig`] (`EnvelopeFilterNode::from`).
///
/// `F` is the float of the graph it runs in, and of the envelope and the filter.
///
/// # Panics
///
/// `EnvelopeFilterNode::from` panics past 16 channels: a filter is one bus.
pub type EnvelopeFilterNode<F> = KernelNode<EnvelopeFilterKernel<F>>;

kernel_config!(EnvelopeFilterConfig => EnvelopeFilterNode<F> where [F: Float + SimdLanes + Lerp],
    set(_): EnvelopeFilterSet = EnvelopeFilterSet);

impl<F: Float + SimdLanes + Lerp> From<EnvelopeFilterConfig> for EnvelopeFilterNode<F> {
    fn from(config: EnvelopeFilterConfig) -> Self {
        Self::with_kernel(
            EnvelopeFilterKernel::new(filter_bus(config.width), config.ty),
            config.kernel_set(),
            config.controls,
        )
    }
}

/// The filter's controls: the base cutoff, the resonance, the envelope's two times and the
/// depth.
#[derive(Clone, Copy, Debug)]
pub struct EnvelopeFilterSet;

control_set! {
    EnvelopeFilterSet(_s): EnvelopeFilterControls = EnvelopeFilterControls::DEFAULT;
    live: (all);
    Cutoff => Hz = base { whole },
    Q => Q = q { whole },
    Attack => Seconds = attack { whole },
    Release => Seconds = release { whole },
    Semitones => Semitones = depth { whole };

    fn defaults(&self) -> EnvelopeFilterControls {
        EnvelopeFilterControls::DEFAULT
    }
}

/// The filter as a [`Kernel`]: an [`EnvelopeFilterBank`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct EnvelopeFilterKernel<F: Float + SimdLanes> {
    input: ChannelTopology,
    ty: SvfType,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<EnvelopeFilterBank<F>>,
}

impl<F: Float + SimdLanes + Lerp> EnvelopeFilterKernel<F> {
    fn new(input: ChannelTopology, ty: SvfType) -> Self {
        Self {
            input,
            ty,
            bank: None,
        }
    }
}

impl<F: Float + SimdLanes + Lerp> Kernel for EnvelopeFilterKernel<F> {
    type Float = F;
    type Set = EnvelopeFilterSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = Some(EnvelopeFilterBank::new(self.input.width(), self.ty, rate));
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &EnvelopeFilterControls) {
        let Some(bank) = &mut self.bank else { return };
        bank.retarget(controls);
    }

    fn jump_to_target(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.jump_to_target();
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

    fn tail(&self) -> Tail {
        Tail::Unknown
    }
}
