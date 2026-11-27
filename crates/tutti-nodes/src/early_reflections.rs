//! [`EarlyReflectionsNode`]: the reverb model's [`TapSet`] stage as a node.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, EarlyReflections, EarlyReflectionsError, TapSet};
use tutti_graph::{Buses, Cost, Shape};
use tutti_units::{SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};
use crate::params::NoControls;

/// What an [`EarlyReflectionsNode`] is made from: the width of its input and the taps.
#[derive(Clone, Debug, PartialEq)]
pub struct EarlyReflectionsConfig {
    width: ChannelLayout,
    taps: TapSet,
}

impl EarlyReflectionsConfig {
    /// A stage of `taps` (a rectangular room's come from [`ShoeboxRoom::taps`](tutti_dsp::ShoeboxRoom::taps)) over an input of `width` channels.
    ///
    /// # Errors
    ///
    /// [`EarlyReflectionsError`] for a tap that cannot be realized.
    pub fn new(
        width: impl Into<ChannelLayout>,
        taps: TapSet,
    ) -> Result<Self, EarlyReflectionsError> {
        taps.check()?;
        Ok(Self {
            width: width.into(),
            taps,
        })
    }
}

/// The early reflections of a reverb model: the mean of the input's channels through the taps
/// of a [`TapSet`] stage ([`EarlyReflections`]), one output channel for each channel a tap feeds.
///
/// Each tap is a delay (in fractional frames, the whole one read exactly and a fraction as the
/// blend of the two frames around it) and a signed gain, so an impulse puts each gain at its
/// delay. The stage is wet only (a tap at delay zero is its direct path); blend it with the
/// input through the graph's mix. The taps are fixed at construction. No latency; the tail is
/// the last tap's delay. A [`KernelNode`]. Made from an [`EarlyReflectionsConfig`]
/// (`EarlyReflectionsNode::from`).
///
/// # Panics
///
/// `EarlyReflectionsNode::from` panics past 16 channels of input: the input is one bus.
pub type EarlyReflectionsNode<F> = KernelNode<EarlyReflectionsKernel<F>>;

kernel_config!(EarlyReflectionsConfig => EarlyReflectionsNode<F> where [F: Float],
    set(_): NoControls = NoControls,
    fixed: ());

impl<F: Float> From<EarlyReflectionsConfig> for EarlyReflectionsNode<F> {
    /// Allocates the history ring.
    ///
    /// # Panics
    ///
    /// Never: [`EarlyReflectionsConfig::new`] checked the taps.
    fn from(config: EarlyReflectionsConfig) -> Self {
        let stage = EarlyReflections::try_from(&config.taps)
            .expect("the config's taps were checked where it was made");
        let (set, controls) = (config.kernel_set(), config.kernel_controls());
        Self::with_kernel(
            EarlyReflectionsKernel {
                width: config.width,
                stage,
            },
            set,
            controls,
        )
    }
}

/// The reflections as a [`Kernel`]: an [`EarlyReflections`], which holds no rate.
#[derive(Clone, Debug)]
pub struct EarlyReflectionsKernel<F: Float> {
    width: ChannelLayout,
    stage: EarlyReflections<F>,
}

impl<F: Float> Kernel for EarlyReflectionsKernel<F> {
    type Float = F;
    type Set = NoControls;

    const GRID: usize = 16;

    /// The input's `width` channels in, one out for each channel a tap feeds, `Moderate`: a read
    /// and a multiply-add for each tap a frame.
    fn shape(&self) -> Shape {
        let channels =
            u16::try_from(self.stage.channels()).expect("a stage has at most 8 channels");
        let input = ChannelTopology::unassigned(self.width)
            .expect("early reflections take at most 16 channels");
        let output = ChannelTopology::unassigned(ChannelLayout::from_count(channels))
            .expect("a stage has at most 8 channels");
        Shape::buses(Buses::main(input), Buses::main(output)).cost(Cost::Moderate)
    }

    /// A re-prepare empties the history.
    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {
        self.stage.reset();
    }

    fn reset(&mut self) {
        self.stage.reset();
    }

    fn retarget(&mut self, (): &()) {}

    fn jump_to_target(&mut self) {}

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.stage.process(block)
    }

    fn tail(&self) -> Tail {
        Tail::Finite(self.stage.tail())
    }
}

#[cfg(test)]
mod tests;
