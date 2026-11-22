//! The spring reverb, [`SpringReverbNode`].
//!
//! [`SpringReverb`] is the spring over buffers views (the all-pass cascades, the loop delay and
//! its low-pass); this file holds the kernel that retargets it, the cells and how the graph
//! reaches them.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, SpringControls, SpringLayout, SpringLength, SpringReverb, SpringStages,
};
use tutti_graph::{MAX_NODE_TAIL, Shape};
use tutti_units::{Hz, Mix, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`SpringReverbNode`] is made from: its width, its layout and where its controls
/// start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringReverbConfig {
    width: ChannelLayout,
    layout: SpringLayout,
    controls: SpringControls,
}

impl SpringReverbConfig {
    /// A spring of `width` channels at
    /// [`SpringLayout::DEFAULT`] and [`SpringControls::DEFAULT`]: forty stages, a 40 ms loop,
    /// a decay of 2 s, a 4 kHz tone and full dispersion.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            layout: SpringLayout::DEFAULT,
            controls: SpringControls::DEFAULT,
        }
    }

    /// The all-passes in each of the two cascades: the more, the longer the chirp.
    #[must_use]
    pub fn stages(mut self, stages: SpringStages) -> Self {
        self.layout.stages = stages;
        self
    }

    /// The loop's delay: the time between the spring's echoes, before the chirp's own delay.
    #[must_use]
    pub fn length(mut self, length: SpringLength) -> Self {
        self.layout.length = length;
        self
    }

    /// The RT60 it starts at, from 100 ms to 60 s; see [`SpringControls::decay`].
    #[must_use]
    pub fn decay(mut self, decay: Seconds) -> Self {
        self.controls.decay = decay;
        self
    }

    /// The cutoff of the loop's low-pass it starts at, at least 20 Hz.
    #[must_use]
    pub fn tone(mut self, tone: Hz) -> Self {
        self.controls.tone = tone;
        self
    }

    /// How dispersed the chirp starts: `0` a plain delay, `1` the paper's coefficients.
    #[must_use]
    pub fn dispersion(mut self, dispersion: Mix) -> Self {
        self.controls.dispersion = dispersion;
        self
    }
}

/// A spring reverb of any width (Valimaki, Parker and Abel, "Parametric spring reverberation
/// effect"): every channel its own spring, wet only.
///
/// A [`KernelNode`] over [`SpringReverbKernel`], the spring being [`SpringReverb`]'s: two
/// cascades of stretched all-passes (the chirp, high frequencies ahead of low) in a loop with
/// a delay and a one-pole low-pass. Its blend with its input, for use as an insert, is the
/// graph's mix (`Editor::set_mix`); it declares no latency. Made from a
/// [`SpringReverbConfig`] (`SpringReverbNode::from`).
///
/// # The controls
///
/// The ports are the decay (an RT60 in seconds, `UnitParam::Decay`), the tone (the loop
/// low-pass's cutoff, `UnitParam::Cutoff`) and the dispersion (`UnitParam::Diffusion`). Each
/// glides in over 20 ms of 32-frame chunks (the chunk is the node's grid), the loop staying stable throughout. The stage count and the loop's length are fixed by the
/// config.
///
/// # The tail
///
/// The declared tail is the ceiling: the longest decay at the lowest tone. The node narrows it
/// each block to what the controls in force ring, and once the input has been silent for that
/// long it cuts the room off and parks ([`KernelNode`]).
///
/// # Panics
///
/// `SpringReverbNode::from` panics past 16 channels: a spring is one bus.
///
/// `F` is the float of the graph it runs in, and of the all-passes and the loop.
pub type SpringReverbNode<F> = KernelNode<SpringReverbKernel<F>>;

impl<F: Float> From<SpringReverbConfig> for SpringReverbNode<F> {
    fn from(config: SpringReverbConfig) -> Self {
        Self::with_kernel(
            SpringReverbKernel::new(config.width, config.layout),
            SpringReverbSet,
            config.controls,
        )
    }
}

/// The spring's controls: the decay, the tone and the dispersion.
#[derive(Clone, Copy, Debug)]
pub struct SpringReverbSet;

control_set! {
    SpringReverbSet(_s): SpringControls = SpringControls::DEFAULT;
    live: (all);
    Decay => Seconds = decay { whole },
    Cutoff => Hz = tone { whole },
    Diffusion => Mix = dispersion { whole };

    fn defaults(&self) -> SpringControls {
        SpringControls::DEFAULT
    }
}

/// The spring as a [`Kernel`]: a [`SpringReverb`] sized at the prepared rate.
///
/// The spring's state is `F`, the float of the graph.
#[derive(Clone, Debug)]
pub struct SpringReverbKernel<F: Float> {
    width: ChannelLayout,
    layout: SpringLayout,
    rate: SampleRate,
    spring: SpringReverb<F>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float> SpringReverbKernel<F> {
    /// A spring at a placeholder rate: an unprepared node renders nothing.
    fn new(width: ChannelLayout, layout: SpringLayout) -> Self {
        let rate = SampleRate::SR_48K;
        Self {
            width,
            layout,
            rate,
            spring: SpringReverb::new(width, rate, layout),
            applied: false,
        }
    }
}

impl<F: Float> Kernel for SpringReverbKernel<F> {
    type Float = F;
    type Set = SpringReverbSet;

    /// A cell that moves reaches the render at a chunk's start, wherever the blocks fall.
    const GRID: usize = SpringReverb::<F>::CHUNK;

    /// `width` channels in and out; `Heavy` (the default cost), as the stages are many.
    fn shape(&self) -> Shape {
        let bus = BusTopology::unassigned(self.width)
            .expect("a spring reverb is at most 16 channels wide");
        Shape::through(bus)
    }

    /// Sizes the lines at `rate`: a re-prepare empties them.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.spring = SpringReverb::new(self.width, rate, self.layout);
        self.applied = false;
    }

    fn reset(&mut self) {
        self.spring.reset();
    }

    /// The first design after `prepare` lands at once; a later one glides in over 20 ms.
    fn retarget(&mut self, controls: &SpringControls) {
        self.spring.retarget(controls);
        if !self.applied {
            self.spring.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.spring.process(block)
    }

    /// The spring's ceiling, or [`Tail::Unbounded`] when that is past what a graph takes as a
    /// finite figure ([`MAX_NODE_TAIL`] at the prepared rate).
    fn tail(&self) -> Tail {
        match self.spring.tail() {
            Tail::Finite(n) if n > MAX_NODE_TAIL.to_samples_ceil(self.rate) => Tail::Unbounded,
            tail => tail,
        }
    }

    fn tail_of(&mut self, controls: &SpringControls) -> Tail {
        self.spring.tail_of(controls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{drive_block, prepared};
    use tutti_graph::Node;
    use tutti_graph::contract::assert_param_fork;
    use tutti_units::UnitParam;

    fn node(width: u16) -> SpringReverbNode<f32> {
        prepared(SpringReverbNode::from(SpringReverbConfig::new(width)))
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(SpringReverbNode::<f32>::from(SpringReverbConfig::new(2u16)));
    }

    /// The shape is the width in and out and declares the decay, the tone and the dispersion
    /// as `Decay`, `Cutoff` and `Diffusion`, at the defaults 2 s, 4 kHz and 1.
    #[test]
    fn the_shape_declares_decay_tone_and_dispersion() {
        let n = node(3);
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Decay, UnitParam::Cutoff, UnitParam::Diffusion][..]
        );
        let d = SpringReverbNode::<f32>::from(SpringReverbConfig::new(1u16));
        assert_eq!(
            (d.param_base(0), d.param_base(1), d.param_base(2)),
            (Some(2.0), Some(4_000.0), Some(1.0))
        );
    }

    /// An empty layout is no channels, not one.
    #[test]
    fn an_empty_layout_has_no_channels() {
        let n = SpringReverbNode::<f32>::from(SpringReverbConfig::new(ChannelLayout::EMPTY));
        assert_eq!(n.shape().audio_in().count(), 0);
    }

    /// Started at no dispersion the node delays an impulse by the cascades' `M K = 40 * 4 =
    /// 160` frames at unity (the mean of two equal delays), and not before.
    #[test]
    fn a_dry_spring_is_a_delay_of_the_cascade() {
        let mut n = prepared(SpringReverbNode::<f32>::from(
            SpringReverbConfig::new(1u16).dispersion(Mix::DRY),
        ));
        let mut x = vec![0.0f32; 64];
        x[0] = 1.0;
        let y = drive_block(&mut n, &[&x]).remove(0);
        assert!(y.iter().all(|v| *v == 0.0), "the chain is 160 frames long");
        let y = drive_block(&mut n, &[&[0.0f32; 128]]).remove(0);
        // Frame 160 of the stream is frame 96 of the second block.
        assert!((y[96] - 1.0).abs() < 1e-6, "{}", y[96]);
    }
}
