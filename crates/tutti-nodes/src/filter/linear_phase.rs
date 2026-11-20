//! [`LinearPhaseEqNode`]: a linear-phase equalizer, the taps of a
//! [`LinearPhaseDesign`](tutti_dsp::LinearPhaseDesign) run through the convolver engine.

use tutti_buffers::{ChannelLayout, Mono};
use tutti_dsp::{FftFloat, Fir, Lerp};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::{Amplitude, Latency, Samples, Tail};

use crate::ConvolverNode;

/// The convolver's block when [`LinearPhaseEqConfig::block_size`] sets none: 512 frames.
const DEFAULT_BLOCK: Samples = Samples::new(512);

/// What a [`LinearPhaseEqNode`] is made from: its width and the taps of the response it applies,
/// and optionally the convolver's block.
#[derive(Clone, Debug, PartialEq)]
pub struct LinearPhaseEqConfig {
    width: ChannelLayout,
    taps: Vec<f64>,
    block_size: Samples,
}

impl LinearPhaseEqConfig {
    /// An equalizer of `width` channels applying `fir`, a type I filter (its length odd, its
    /// taps symmetric), to each channel, with a convolver block of 512 frames.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, fir: &Fir) -> Self {
        Self {
            width: width.into(),
            taps: fir.taps().collect(),
            block_size: DEFAULT_BLOCK,
        }
    }

    /// The convolver's head partition, rounded up to a power of two: the latency the engine
    /// adds to the filter's own delay, and the granularity its cost is spread over.
    #[must_use]
    pub fn block_size(self, block_size: Samples) -> Self {
        Self { block_size, ..self }
    }
}

/// A linear-phase equalizer of any width: one [`Fir`] applied to every channel by the
/// uniformly partitioned [`ConvolverNode`] engine, with no second FFT convolution.
///
/// The response is the taps' (design them with [`LinearPhaseDesign`](tutti_dsp::LinearPhaseDesign)),
/// whose phase is linear, so every frequency is delayed alike and the magnitude is the target's.
/// The taps are the node's build: a band that moves is a new [`Fir`] designed on the control
/// side and a re-declared node, which the session crossfades in.
///
/// **Latency** is the filter's group delay `(N - 1) / 2` for `N` taps plus the convolver's block,
/// declared in [`Shape`] for the whole output; the impulse response peaks at exactly that
/// frame. **Tail** is the remaining `(N - 1) / 2`, counted once the latency is compensated:
/// the response ends `N - 1` frames after its first, and the convolver's block precedes
/// the peak by `(N - 1) / 2`. Both are exact, a convolution being linear and finite.
///
/// The output is the filtered signal alone, scaled by an output [`gain`](Self::gain) that starts at 1. A node
/// of a [`LinearPhaseEqConfig`] (`LinearPhaseEqNode::from`).
///
/// # Example
///
/// ```
/// use tutti_buffers::ChannelLayout;
/// use tutti_dsp::{LinearPhaseDesign, MatchedBell, Tilt};
/// use tutti_nodes::{LinearPhaseEqConfig, LinearPhaseEqNode};
/// use tutti_units::{Db, Hz, Q, SampleRate};
///
/// let rate = SampleRate::SR_48K;
/// let bands = [
///     MatchedBell::new(Q::new(1.0), Db::new(6.0)).design(Hz::new(1_000.0), rate).unwrap(),
///     Tilt::new(Db::new(-3.0)).design(Hz::new(4_000.0), rate).unwrap(),
/// ];
/// let fir = LinearPhaseDesign::new(Hz::new(100.0)).design(rate, &bands).unwrap();
/// let node = LinearPhaseEqNode::<f32>::from(LinearPhaseEqConfig::new(ChannelLayout::STEREO, &fir));
/// assert_eq!(node.latency_samples().get(), 512 + (fir.len().get() - 1) / 2);
/// ```
///
/// # Panics
///
/// `LinearPhaseEqNode::from` panics for a width of no channels or past a bus's width.
#[derive(Clone, Debug)]
pub struct LinearPhaseEqNode<F: FftFloat + Lerp> {
    convolver: ConvolverNode<F>,
    /// The filter's group delay, `(N - 1) / 2` frames.
    delay: Samples,
}

impl<F: FftFloat + Lerp> From<LinearPhaseEqConfig> for LinearPhaseEqNode<F> {
    fn from(config: LinearPhaseEqConfig) -> Self {
        assert!(
            config.taps.len() % 2 == 1,
            "a linear-phase filter has an odd number of taps"
        );
        let convolver =
            ConvolverNode::shared_ir(config.width, Mono::new(&config.taps), config.block_size)
                .expect("a linear-phase equalizer is one to sixteen channels wide");
        Self {
            convolver,
            delay: Samples::new((config.taps.len() - 1) / 2),
        }
    }
}

impl<F: FftFloat + Lerp> LinearPhaseEqNode<F> {
    /// The output gain's cell: the convolver's wet gain, 1 until stored; a change ramps across
    /// the next block and is floored at 0.
    #[must_use]
    pub fn gain(&self) -> Param<Amplitude> {
        self.convolver.gain()
    }

    /// The latency the node declares: the filter's delay and the convolver's block.
    #[must_use]
    pub fn latency_samples(&self) -> Samples {
        self.convolver.latency_samples() + self.delay
    }
}

impl<F: FftFloat + Lerp> Node<F> for LinearPhaseEqNode<F> {
    /// The convolver's ports, with its latency and tail re-counted around the filter's delay.
    fn shape(&self) -> Shape {
        let mut shape = self.convolver.shape();
        shape.latency = Latency::new(self.latency_samples());
        shape.tail = Tail::Finite(self.delay);
        shape
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.convolver.param_base(k)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.convolver.prepare(p);
    }

    fn process(&mut self, cx: &Cx<'_>, io: Io<'_, F>) -> Status {
        self.convolver.process(cx, io)
    }

    fn reset(&mut self) {
        Node::<F>::reset(&mut self.convolver);
    }
}

impl<F: FftFloat + Lerp> ParamNode<F> for LinearPhaseEqNode<F> {
    /// The convolver's: the wet gain.
    fn param_set(&self) -> ParamSet {
        self.convolver.param_set()
    }

    fn fork_fresh(&self) -> Self {
        Self {
            convolver: self.convolver.fork_fresh(),
            delay: self.delay,
        }
    }
}

/// Inserted as the convolver is: its [`ParamSet`] as its controls, and a fork over the same
/// spectra.
impl<F: FftFloat + Lerp> IntoNode<F> for LinearPhaseEqNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
