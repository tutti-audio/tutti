//! [`ConvolverNode`]: [`Convolver`] as a graph node, N channels in and M out.
//!
//! One node for every shape: the channel counts are a property of the value, and how its
//! impulse responses lie over the channels is an [`IrLayout`]. A per-channel layout runs one
//! [`Convolver`] per channel; true stereo and ambisonic run a [`MatrixConvolver`] whose
//! paths are the same engine.
//!
//! The node outputs its wet signal alone, scaled by one wet-path gain shared by every
//! output channel: the blend with the dry input is the graph's (`Editor::set_mix`).
//!
//! **An IR's FFT partitions are stored once**, in its
//! [`IrSpectra`](tutti_dsp::IrSpectra) behind an `Arc`: a shared IR is read by every
//! channel, and a fork of the node ([`ParamNode::fork_fresh`]) reads the live node's, since
//! nothing writes them after they are built. Each channel and each fork keeps only its own
//! running state.

use tutti_buffers::{
    Bed, ChannelLayout, ChannelTopology, Fold, FoldError, InputOutput, MAX_CHANNELS, Mono, MonoMut,
    Planes, PlanesMut, Sample,
};
use tutti_graph::{
    Buses, Cx, HostValues, Initials, Inputs, IntoNode, Io, MAX_PORTS, Node, NodeConfig, NodeParts,
    Outputs, ParamNode, ParamSet, Prepare, Shape, Shared, Status,
};
use tutti_rt::Param;
use tutti_units::{Amplitude, Samples, UnitAddr, UnitParam};
use tutti_units::{Latency, Tail};

use super::{ImpulseResponses, IrLayout, MatrixLayout};
use crate::kernel::{slice_in, slice_out};
use crate::last_good::LastGood;
use crate::tail_clock::{Heard, TailClock, tail_frames};
use tutti_dsp::Ramp;
use tutti_dsp::{
    Convolver, FftFloat, Fir, Lerp, MatrixConvolver, MatrixConvolverConfig, MatrixConvolverError,
    MatrixResponses,
};

/// The head partition of a config that states none.
const DEFAULT_BLOCK: Samples = Samples::new(512);

/// The block a folded node's scratch holds before `prepare` sizes it to the graph's.
const FOLD_SCRATCH: usize = 512;

/// The tail of an FIR whose impulse response is `ir_length` samples long.
///
/// One less than the response, because a tail counts the frames produced *after*
/// the input stops: an IR of length `L` answers an impulse at frame 0 through
/// frame `L - 1`, so `L - 1` of them land past the input. Defined this way the
/// figure adds along a chain with no correction term, which is what
/// [`tutti_graph`] relies on.
///
/// A convolution is linear and finite, so this is exact rather than an estimate
/// — the only node here that can say that.
pub(super) fn ring_out(ir_length: Samples) -> Tail {
    match ir_length.get() {
        // An empty or single-sample IR produces nothing after its input stops.
        0 | 1 => Tail::None,
        n => Tail::Finite(Samples::new(n - 1)),
    }
}

/// The tail of an FIR of `ir_length` samples whose peak the node declares as `delay` frames of
/// its latency: [`ring_out`] less the delay, the frames left once the latency is compensated.
pub(super) fn ring_after(ir_length: Samples, delay: Samples) -> Tail {
    match ring_out(ir_length) {
        Tail::Finite(n) => match n.get().saturating_sub(delay.get()) {
            0 => Tail::None,
            left => Tail::Finite(Samples::new(left)),
        },
        other => other,
    }
}

/// Why a [`ConvolverNode`] could not be built from the layout and impulse responses given.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConvolverError {
    /// A bus of no channels: a node has at least one in and one out.
    #[error("a ConvolverNode needs at least one channel")]
    NoChannels,
    /// The number of impulse responses is not what the layout takes.
    #[error("the layout takes {want} impulse responses, not {got}")]
    IrCount {
        /// What the layout takes: the bus's width, or one shared response.
        want: usize,
        /// What was given.
        got: usize,
    },
    /// The matrix responses are for another shape than the layout and bus.
    #[error(
        "the layout is {want_inputs} in {want_outputs} out, the responses {inputs} in {outputs} out"
    )]
    MatrixShape {
        /// The layout's input channels.
        want_inputs: u16,
        /// The bus's output channels.
        want_outputs: u16,
        /// The responses' input channels.
        inputs: u16,
        /// The responses' output channels.
        outputs: u16,
    },
    /// A true-stereo layout on a bus that is not a pair.
    #[error("a true-stereo layout is two channels wide, not {width}")]
    TrueStereoWidth {
        /// The bus's channels.
        width: u16,
    },
    /// A folded node whose width has no fold to mono.
    #[error("a {width}-wide folded ConvolverNode has no fold to mono")]
    NoFold {
        /// The node's width.
        width: u16,
        /// Why the width does not fold.
        #[source]
        source: FoldError,
    },
}

impl From<MatrixConvolverError> for ConvolverError {
    fn from(error: MatrixConvolverError) -> Self {
        match error {
            MatrixConvolverError::NoChannels => Self::NoChannels,
            MatrixConvolverError::IrCount { want, got, .. } => Self::IrCount { want, got },
        }
    }
}

/// The convolution of a node: one engine per channel, the inputs folded to mono into one
/// engine per output channel, or one matrix of paths.
#[derive(Clone, Debug)]
enum Engine<F: FftFloat> {
    /// One convolver per channel; its length is the width.
    PerChannel(Vec<Convolver<F>>),
    /// Every input folded to mono, then one convolver per output channel.
    Folded {
        convolvers: Vec<Convolver<F>>,
        /// The fold of the input bus to mono.
        fold: Fold<F>,
        /// The folded mono block. [`FOLD_SCRATCH`] frames from construction, and the graph's
        /// block from `prepare`: a block is rendered in pieces of this length.
        mono: Vec<F>,
    },
    /// Every (input, output) path.
    Matrix(Box<MatrixConvolver<F>>),
}

impl<F: FftFloat> Engine<F> {
    fn ir_length(&self) -> Samples {
        match self {
            Self::PerChannel(cs) | Self::Folded { convolvers: cs, .. } => cs
                .iter()
                .map(Convolver::<F>::ir_length)
                .max()
                .unwrap_or(Samples::ZERO),
            Self::Matrix(m) => m.ir_length(),
        }
    }

    fn latency(&self) -> Samples {
        match self {
            Self::PerChannel(cs) | Self::Folded { convolvers: cs, .. } => cs[0].latency(),
            Self::Matrix(m) => m.latency(),
        }
    }

    fn fresh(&self) -> Self {
        match self {
            Self::PerChannel(cs) => {
                Self::PerChannel(cs.iter().map(Convolver::<F>::fresh).collect())
            }
            Self::Folded {
                convolvers,
                fold,
                mono,
            } => Self::Folded {
                convolvers: convolvers.iter().map(Convolver::<F>::fresh).collect(),
                fold: fold.clone(),
                mono: vec![F::ZERO; mono.len()],
            },
            Self::Matrix(m) => Self::Matrix(Box::new(m.fresh())),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::PerChannel(cs) | Self::Folded { convolvers: cs, .. } => {
                cs.iter_mut().for_each(Convolver::<F>::reset);
            }
            Self::Matrix(m) => m.reset(),
        }
    }
}

/// What a [`ConvolverNode`] is made from: the bus it convolves onto, how the impulse responses
/// lay over the channels ([`IrLayout`]), and the responses.
///
/// [`new`](Self::new) checks the responses against the layout and the bus, so the node a
/// config makes ([`From`]) cannot fail. The block is 512 frames unless
/// [`block_size`](Self::block_size) says otherwise.
///
/// The responses are a [`Shared`] handle the caller owns: two configs are equal when they have
/// the same buses, block and the same handle (one allocation), and a config over responses
/// loaded again is a new node. This holds for every layout, the matrix ones included
/// ([`matrix`](Self::matrix)).
///
/// An IR carries no rate and nothing resamples it (see [`ConvolverNode`]).
///
/// ```
/// use std::sync::Arc;
/// use tutti_buffers::{ChannelTopology, Mono};
/// use tutti_nodes::{ConvolverConfig, ConvolverNode, ImpulseResponses, IrLayout};
/// use tutti_units::Samples;
///
/// let ir = [1.0_f32, 0.5, 0.25];
/// let responses = Arc::new(ImpulseResponses::new(&[Mono::new(&ir)]));
/// let node = ConvolverNode::<f32>::from(
///     ConvolverConfig::new(ChannelTopology::STEREO, IrLayout::PerChannel, responses)
///         .unwrap()
///         .block_size(Samples::new(64)),
/// );
/// assert_eq!(node.latency_samples().get(), 64);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct ConvolverConfig<S: Sample> {
    output: ChannelTopology,
    input: ChannelTopology,
    paths: Paths<S>,
    block_size: Samples,
    zero_latency: bool,
}

/// The checked responses of a [`ConvolverConfig`], by what each layout needs to build its
/// engine: no check is left for the node to make.
#[derive(Clone, Debug, PartialEq)]
enum Paths<S: Sample> {
    /// [`IrLayout::PerChannel`]: the width, or one shared response.
    PerChannel(Shared<ImpulseResponses<S>>),
    /// [`IrLayout::Folded`]: the responses and the fold of the input bus to mono, in `f64`
    /// and cast to the graph's float when the node is built.
    Folded {
        irs: Shared<ImpulseResponses<S>>,
        fold: Fold<f64>,
    },
    /// A [`MatrixLayout`]: one response per path, in the caller's checked set.
    Matrix(Shared<MatrixResponses<S>>),
}

impl<S: Sample> ConvolverConfig<S> {
    /// A convolver onto the bus `output` with the responses `irs` laid over its channels as
    /// `layout` says: one per channel, or one shared by all ([`IrLayout`]).
    ///
    /// # Errors
    ///
    /// By name, before any convolver is built: [`ConvolverError::NoChannels`] for a bus of
    /// none, [`ConvolverError::IrCount`] when the responses are not the bus's width (or one),
    /// and [`ConvolverError::NoFold`] for a folded width with no fold to mono: only 1, 2, 4,
    /// 6, 8 and 12 have an order ([`Fold::between`] places any other by index and refuses to
    /// narrow it). The responses and the layout depend on each other: how many a layout takes
    /// follows from its bus, and a folded one needs a fold to mono for its width, so no type
    /// of one holds the others.
    pub fn new(
        output: ChannelTopology,
        layout: IrLayout,
        irs: impl Into<Shared<ImpulseResponses<S>>>,
    ) -> Result<Self, ConvolverError> {
        let irs = irs.into();
        let width = output.width();
        if width.count() == 0 {
            return Err(ConvolverError::NoChannels);
        }
        let want = usize::from(width.count());
        if irs.len() != want && irs.len() != 1 {
            return Err(ConvolverError::IrCount {
                want,
                got: irs.len(),
            });
        }
        let paths = match layout {
            IrLayout::PerChannel => Paths::PerChannel(irs),
            IrLayout::Folded => {
                let fold =
                    Fold::<f64>::between(&output, &Bed::Mono.topology()).map_err(|source| {
                        ConvolverError::NoFold {
                            width: width.count(),
                            source,
                        }
                    })?;
                Paths::Folded { irs, fold }
            }
        };
        Ok(Self {
            output,
            input: output,
            paths,
            block_size: DEFAULT_BLOCK,
            zero_latency: false,
        })
    }

    /// A convolver onto the bus `output` with one response for each path of `responses`,
    /// laid over the paths as `layout` says ([`MatrixLayout`]).
    ///
    /// The caller owns the checked set (a [`Shared`] of [`MatrixResponses`], whose
    /// [`new`](MatrixResponses::new) checks the response count against the shape), and the
    /// config keeps that handle: two configs made from one handle are the same structure.
    ///
    /// # Errors
    ///
    /// By name: [`ConvolverError::NoChannels`] for a bus of none,
    /// [`ConvolverError::TrueStereoWidth`] for a true-stereo layout off a pair, and
    /// [`ConvolverError::MatrixShape`] when the responses are for another number of
    /// channels in or out than the layout and bus have.
    pub fn matrix(
        output: ChannelTopology,
        layout: MatrixLayout,
        responses: impl Into<Shared<MatrixResponses<S>>>,
    ) -> Result<Self, ConvolverError> {
        let responses = responses.into();
        let input = layout.input(output);
        let (n_in, n_out) = (input.width(), output.width());
        if n_in.count() == 0 || n_out.count() == 0 {
            return Err(ConvolverError::NoChannels);
        }
        if layout == MatrixLayout::TrueStereo && n_out != ChannelLayout::STEREO {
            return Err(ConvolverError::TrueStereoWidth {
                width: n_out.count(),
            });
        }
        if responses.inputs() != n_in || responses.outputs() != n_out {
            return Err(ConvolverError::MatrixShape {
                want_inputs: n_in.count(),
                want_outputs: n_out.count(),
                inputs: responses.inputs().count(),
                outputs: responses.outputs().count(),
            });
        }
        Ok(Self {
            output,
            input,
            paths: Paths::Matrix(responses),
            block_size: DEFAULT_BLOCK,
            zero_latency: false,
        })
    }

    /// The convolver's head partition, rounded up to a power of two: the node's latency.
    #[must_use]
    pub fn block_size(self, block_size: Samples) -> Self {
        Self { block_size, ..self }
    }

    /// No latency ([`Convolver::zero_latency`]): the first `block_size` taps of every
    /// response run as a direct-form FIR, which costs `block_size` multiply-adds per frame
    /// per path, and the node's latency is zero. Off by default.
    #[must_use]
    pub fn zero_latency(self) -> Self {
        Self {
            zero_latency: true,
            ..self
        }
    }
}

impl<F: FftFloat + Lerp, S: Sample> From<ConvolverConfig<S>> for ConvolverNode<F> {
    /// The node, in the float `F` of the graph it runs in.
    fn from(config: ConvolverConfig<S>) -> Self {
        Self::over(&config)
    }
}

/// A convolver's wet gain is its only param, and a config has no value for it: the config is
/// all structure, and the gain starts at unity.
impl<F: FftFloat + Lerp, S: Sample> NodeConfig<F> for ConvolverConfig<S> {
    type Node = ConvolverNode<F>;

    fn build(&self, _: &HostValues) -> ConvolverNode<F> {
        ConvolverNode::over(self)
    }

    fn same_structure(&self, other: &Self) -> bool {
        self == other
    }

    fn initials(&self) -> Initials {
        wet_gain_initials()
    }
}

/// The initial values of a convolver: its wet gain at unity.
fn wet_gain_initials() -> Initials {
    Initials::new().param(UnitAddr::first(UnitParam::Volume), Amplitude::UNITY.get())
}

impl<F: FftFloat + Lerp> ConvolverNode<F> {
    /// The node of a checked `config`.
    fn over<S: Sample>(config: &ConvolverConfig<S>) -> Self {
        let ConvolverConfig {
            output,
            input,
            paths,
            block_size,
            zero_latency,
        } = config;
        let (output, input, block_size, zero_latency) =
            (*output, *input, *block_size, *zero_latency);
        let convolver = |ir: Mono<'_, S>| {
            if zero_latency {
                Convolver::zero_latency(ir, block_size)
            } else {
                Convolver::new(ir, block_size)
            }
        };
        // One convolver per output channel: a lone response is shared by all of them.
        let per_channel = |irs: &ImpulseResponses<S>| -> Vec<Convolver<F>> {
            let irs = irs.views();
            if irs.len() == 1 {
                let head = convolver(irs[0]);
                (0..usize::from(output.width().count()))
                    .map(|_| head.fresh())
                    .collect()
            } else {
                irs.iter().map(|ir| convolver(*ir)).collect()
            }
        };
        let engine = match paths {
            Paths::PerChannel(irs) => Engine::PerChannel(per_channel(irs)),
            Paths::Folded { irs, fold } => Engine::Folded {
                fold: fold.cast(),
                convolvers: per_channel(irs),
                // Sized before `prepare` so an unprepared node renders in pieces rather than
                // index an empty scratch; `prepare` resizes it to the graph's block.
                mono: vec![F::ZERO; FOLD_SCRATCH],
            },
            Paths::Matrix(responses) => {
                let matrix = MatrixConvolverConfig::new(responses).block_size(block_size);
                Engine::Matrix(Box::new(
                    if zero_latency {
                        matrix.zero_latency()
                    } else {
                        matrix
                    }
                    .build(),
                ))
            }
        };
        Self::assemble(engine, input, output)
    }
}

/// What a linear-phase [`ConvolverNode`] is made from: its bus and the [`Fir`] it applies, and
/// optionally the convolver's block; [`From`] makes the node.
///
/// The taps of `fir` (odd and symmetric, so the delay of its peak is `fir.delay()`) are applied
/// independently to each channel of the bus, like a shared response of a [`ConvolverConfig`],
/// with the delay declared as latency: the node's latency is the block plus `fir.delay()` and
/// its tail the response's remaining `fir.delay()` frames, so the graph compensates the delay
/// and the response peaks at exactly the declared latency. A band that moves is a new [`Fir`]
/// and a new node.
///
/// The filter is an [`Shared`]: two configs are equal when they share one [`Fir`].
///
/// # Example
///
/// ```
/// use tutti_buffers::ChannelTopology;
/// use tutti_dsp::{LinearPhaseDesign, MatchedBell, Tilt};
/// use tutti_nodes::{ConvolverNode, FirConvolverConfig};
/// use tutti_units::{Db, Hz, Q, SampleRate, Samples};
///
/// let rate = SampleRate::SR_48K;
/// let bands = [
///     MatchedBell::new(Q::new(1.0), Db::new(6.0)).design(Hz::new(1_000.0), rate).unwrap(),
///     Tilt::new(Db::new(-3.0)).design(Hz::new(4_000.0), rate).unwrap(),
/// ];
/// let fir = LinearPhaseDesign::new(Hz::new(100.0)).design(rate, &bands).unwrap();
/// let latency = 512 + (fir.len().get() - 1) / 2;
/// let node = ConvolverNode::<f32>::from(
///     FirConvolverConfig::new(ChannelTopology::STEREO, std::sync::Arc::new(fir)).unwrap(),
/// );
/// assert_eq!(node.latency_samples().get(), latency);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct FirConvolverConfig {
    input: ChannelTopology,
    fir: Shared<Fir>,
    block_size: Samples,
}

impl FirConvolverConfig {
    /// A filter on the bus `input` applying `fir`, with a block of 512 frames.
    ///
    /// # Errors
    ///
    /// [`ConvolverError::NoChannels`] for a bus of no channels: a node has at least one in
    /// and one out, which no `ChannelTopology` holds.
    pub fn new(
        input: ChannelTopology,
        fir: impl Into<Shared<Fir>>,
    ) -> Result<Self, ConvolverError> {
        if input.width().count() == 0 {
            return Err(ConvolverError::NoChannels);
        }
        Ok(Self {
            input,
            fir: fir.into(),
            block_size: DEFAULT_BLOCK,
        })
    }

    /// The convolver's head partition, rounded up to a power of two: the latency the engine
    /// adds to the filter's own delay.
    #[must_use]
    pub fn block_size(self, block_size: Samples) -> Self {
        Self { block_size, ..self }
    }
}

impl<F: FftFloat + Lerp> From<FirConvolverConfig> for ConvolverNode<F> {
    /// The node, in the float `F` of the graph it runs in.
    fn from(config: FirConvolverConfig) -> Self {
        Self::over_fir(&config)
    }
}

/// A convolver's wet gain is its only param, and a config has no value for it: the config is
/// all structure, and the gain starts at unity.
impl<F: FftFloat + Lerp> NodeConfig<F> for FirConvolverConfig {
    type Node = ConvolverNode<F>;

    fn build(&self, _: &HostValues) -> ConvolverNode<F> {
        ConvolverNode::over_fir(self)
    }

    fn same_structure(&self, other: &Self) -> bool {
        self == other
    }

    fn initials(&self) -> Initials {
        wet_gain_initials()
    }
}

impl<F: FftFloat + Lerp> ConvolverNode<F> {
    /// The node of a checked `config`: its taps as one shared response, and the filter's
    /// delay declared as latency.
    fn over_fir(config: &FirConvolverConfig) -> Self {
        let taps: Vec<f64> = config.fir.taps().collect();
        let irs = ImpulseResponses::new(&[Mono::new(&taps)]);
        let layout = ConvolverConfig {
            output: config.input,
            input: config.input,
            paths: Paths::PerChannel(Shared::new(irs)),
            block_size: config.block_size,
            zero_latency: false,
        };
        Self::over(&layout).with_declared_delay(config.fir.delay())
    }
}

/// Convolution reverb as a graph node, N channels in and M out.
///
/// Built from a [`ConvolverConfig`] at fixed channel counts, with an [`IrLayout`] saying which
/// impulse response each path from an input to an output has. Latency is one FFT block (none with [`zero_latency`](ConvolverConfig::zero_latency)), the same on every output, declared
/// in its [`Shape`] for the **whole** output. Its tail is the longest IR's ring-out.
///
/// The output is the wet signal alone, scaled by one wet-path [`Amplitude`] shared by every
/// output channel, read once per block. A gain that moves between blocks ramps across the
/// next piece the node renders, so a render is bit-identical across block splits for a
/// constant gain, and for a gain driven on the `Volume` port, which is per frame. A finite
/// gain is applied as it is, so a huge one (1e20) passes and the output is not guarded; a
/// non-finite one reads as the last finite. The blend with the dry input is the graph's:
/// `Editor::set_mix` mixes the output with the input delayed by the latency declared here.
///
/// # Silence, parking and resume
///
/// The output is exact zeros past the latency plus the longest IR's ring-out after the last
/// non-zero input frame. A block wholly past it, with silent input, answers
/// [`Status::Silent`] without rendering, which is what lets the graph park the node. When
/// input resumes after a gap at least that long, every output of the earlier input has been
/// written, so the engine restarts from zero at the first non-zero frame and the gain starts
/// flat on its current value. The render is then the same bits whatever the block sizes and
/// whether the graph parked the node: a block is cut where the tail ends and where the
/// engine restarts, and a resumed burst is a fresh node's.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, it offers the wet gain as its one param
/// ([`UnitParam::Volume`]), also set through [`gain`](Self::gain). A fork has the gain as
/// it is when forked, shares no cell with the live node, and shares its IR
/// spectra, which are read-only.
///
/// # The sample rate is not this node's to fix
///
/// Unlike the other rate-dependent nodes in this crate, `prepare` fixes no
/// coefficient here, and the node stores no rate. `Mono` views carry no rate, so the node cannot tell what rate they were
/// measured at and has nothing to resample from or to.
///
/// The consequence is the caller's to avoid: an IR captured at 44.1 kHz and
/// convolved at 48 kHz plays back 8.8% short and correspondingly bright — a
/// reverb tail that decays too fast, which reads as a different room rather
/// than as an error. Supply IRs already at the graph's rate.
#[derive(Clone)]
pub struct ConvolverNode<F: FftFloat + Lerp> {
    engine: Engine<F>,
    /// The input bus: the layout's input for the output bus.
    inputs: ChannelTopology,
    /// The output bus, the config's.
    outputs: ChannelTopology,
    /// The wet path's linear gain, floored at 0.
    gain: Param<Amplitude>,
    /// The latency the node declares: the engine's block plus [`declared_delay`](Self::declared_delay).
    latency_samples: Samples,
    /// The delay of the response's peak that the node declares as latency: a linear-phase
    /// filter's `(N - 1) / 2` ([`FirConvolverConfig`]), zero for any other response.
    declared_delay: Samples,
    /// The ring-out owed after the last non-silent input frame: the longest IR's, past the
    /// latency. It says when a block is wholly past the tail and the node parks.
    clock: TailClock,
    /// The gain the previous block ended on — where this block's ramp starts.
    /// `None` until the first block and after `reset`, which then starts on the
    /// current value instead of ramping in from nothing.
    last: Option<Amplitude>,
    /// Last finite gain. It does not feed recursive state here, but the block's
    /// end is the next block's ramp start, so a NaN would carry into the block
    /// after it (see [`LastGood`]).
    good: LastGood<F>,
}

impl<F: FftFloat + Lerp> std::fmt::Debug for ConvolverNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvolverNode")
            .field("engine", &self.engine)
            .field("gain", &self.gain)
            .field("latency_samples", &self.latency_samples)
            .field("declared_delay", &self.declared_delay)
            .field("last", &self.last)
            .field("good", &self.good)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat + Lerp> ConvolverNode<F> {
    /// The node over `engine`, which a config has checked.
    fn assemble(engine: Engine<F>, inputs: ChannelTopology, outputs: ChannelTopology) -> Self {
        let latency_samples = engine.latency();
        let clock = TailClock::new(
            tail_frames(ring_out(engine.ir_length())),
            latency_samples.get() as u64,
        );
        Self {
            engine,
            inputs,
            outputs,
            gain: Param::new(Amplitude::UNITY),
            latency_samples,
            declared_delay: Samples::ZERO,
            clock,
            last: None,
            good: LastGood::fixed(F::ONE),
        }
    }

    /// This node with `delay` of its response's peak declared as latency: the latency is the
    /// engine's block plus `delay`, and the tail and the silence clock count from there.
    fn with_declared_delay(mut self, delay: Samples) -> Self {
        let ring = ring_after(self.engine.ir_length(), delay);
        self.declared_delay = delay;
        self.latency_samples = self.engine.latency() + delay;
        self.clock = TailClock::new(tail_frames(ring), self.latency_samples.get() as u64);
        self
    }

    /// The channels in.
    pub fn inputs(&self) -> ChannelLayout {
        self.inputs.width()
    }

    /// The channels out.
    pub fn outputs(&self) -> ChannelLayout {
        self.outputs.width()
    }

    /// A handle on the shared wet-path [`Amplitude`] cell, for every channel.
    ///
    /// `1.0` is unity. The cell is read floored at 0, so the wet path never inverts.
    #[must_use]
    pub fn gain(&self) -> Param<Amplitude> {
        self.gain.handle()
    }

    /// The node's latency in [`Samples`] — one FFT block (zero with [`ConvolverConfig::zero_latency`]), plus the declared delay of a
    /// [`FirConvolverConfig`] node, the same on every output.
    ///
    /// Partitioned convolution cannot emit a sample until its first block is
    /// full, so this delay is inherent. A graph mixing this against a dry path
    /// must compensate it, or the two arrive misaligned and comb-filter.
    pub fn latency_samples(&self) -> Samples {
        self.latency_samples
    }

    /// Scale one output channel's already-convolved block `out` by the wet gain, in
    /// place, ramped across the block (see [`begin_block`](Self::begin_block)).
    #[inline]
    fn apply_gain(out: &mut [F], ramp: &Ramp<F>, held: Option<Amplitude>) {
        if let Some(gain) = held {
            // Nothing moved: a constant in the loop.
            let gain = F::from_unit(gain);
            for o in out {
                *o *= gain;
            }
            return;
        }
        for (i, o) in out.iter_mut().enumerate() {
            *o *= ramp.at(u32::try_from(i + 1).unwrap_or(u32::MAX));
        }
    }

    /// Scale `out` by the gain the graph drives on the wet-gain port, frame by
    /// frame, floored at 0 like the setter. A non-finite value reads as the
    /// last finite one (see [`LastGood`]).
    #[inline]
    fn apply_driven_gain(out: &mut [F], signal: &[F], good: &mut LastGood<F>) {
        for (o, &g) in out.iter_mut().zip(signal) {
            *o *= good.read(g).max(F::ZERO);
        }
    }

    /// Read the gain once, and ramp from where the previous block ended. It
    /// scales the output directly — a stepped gain is a click — so a change
    /// between blocks ramps across the next one and lands exactly on the new
    /// value (a block of one takes it at once).
    fn begin_block(&mut self, size: usize) -> (Ramp<F>, Option<Amplitude>) {
        // The cell's gain is floored at 0 and narrows once, and a gain that is not finite
        // reads as the last finite one (`from_unit` would saturate an infinity to a finite
        // value).
        let cell = self.gain.load();
        let floored = Amplitude::new(cell.get().max(0.0));
        let gain = Amplitude::new(
            self.good
                .read_if(F::from_unit(floored), cell.get().is_finite())
                .into(),
        );
        let from = self.last.unwrap_or(gain);
        self.last = Some(gain);
        // The ramp runs over the block: its ends narrow once per block.
        let mut ramp = Ramp::new(F::from_unit(from));
        ramp.retarget_in(F::from_unit(gain), u32::try_from(size).unwrap_or(u32::MAX));
        let held = (from.get() == gain.get()).then_some(gain);
        (ramp, held)
    }
}

/// How one piece of a block is scaled by the wet gain.
enum WetGain<'a, F: FftFloat + Lerp> {
    /// The per-frame values on the wet-gain param port.
    Driven(&'a [F]),
    /// The cell's value, ramped from where the previous block ended; the
    /// value is `Some` when the ramp is flat.
    Ramped(Ramp<F>, Option<Amplitude>),
}

impl<F: FftFloat + Lerp> ConvolverNode<F> {
    /// Frames `at..at + size` of the block (`size` at most the `mono`
    /// scratch's length): convolve, then scale each channel's run
    /// by the wet gain. The gain is read once, here, and ramped if it moved.
    fn render(
        &mut self,
        at: usize,
        size: usize,
        inputs: &Inputs<'_, F>,
        outputs: &mut Outputs<'_, '_, F>,
        driven: Option<&[F]>,
    ) {
        let input = |c: usize| &inputs.get(c)[at..at + size];
        // A driven gain is the graph's signal, which it declicks itself: the
        // block ramp restarts from the value after it.
        let gain = match driven {
            Some(signal) => {
                self.last = None;
                WetGain::Driven(&signal[at..at + size])
            }
            None => {
                let (ramp, held) = self.begin_block(size);
                WetGain::Ramped(ramp, held)
            }
        };
        let n_in = usize::from(self.inputs.width().count());
        let n = usize::from(self.outputs.width().count());
        match &mut self.engine {
            Engine::PerChannel(convolvers) => {
                for (c, convolver) in convolvers.iter_mut().enumerate() {
                    convolver.process(
                        Mono::new(input(c)),
                        MonoMut::new(&mut outputs.get(c)[at..at + size]),
                    );
                }
            }
            Engine::Folded {
                convolvers,
                fold,
                mono,
            } => {
                {
                    // Folded once per block, not once per output channel.
                    // The inputs, a block long each, as one table on the stack: a
                    // fold exists only at a width with an order, at most one bus.
                    let width = usize::from(fold.src_layout().count()).min(MAX_CHANNELS);
                    let mut table: [&[F]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
                    for (c, slot) in table.iter_mut().enumerate().take(width) {
                        *slot = input(c);
                    }
                    let folded = Planes::new(&table[..width]).and_then(|planes| {
                        fold.fold_planar_channel(planes, 0, MonoMut::new(&mut mono[..size]))
                    });
                    debug_assert!(
                        folded.is_ok(),
                        "the inputs are a block long and the fold's width: {folded:?}"
                    );
                    if folded.is_err() {
                        // Not folded: silent, never the previous block's mono.
                        mono[..size].fill(F::ZERO);
                    }
                    for (c, convolver) in convolvers.iter_mut().enumerate() {
                        convolver.process(
                            Mono::new(&mono[..size]),
                            MonoMut::new(&mut outputs.get(c)[at..at + size]),
                        );
                    }
                }
            }
            Engine::Matrix(matrix) => {
                let frames = at..at + size;
                let (heard, heard_n) = slice_in((0..n_in).map(|c| inputs.get(c)), &frames);
                let (mut wet, wet_n) = slice_out(outputs.iter_mut(), &frames);
                let rendered = {
                    // The planes reborrowed for the block alone, so `wet` is free after it.
                    let mut views: [&mut [F]; MAX_PORTS] =
                        core::array::from_fn(|_| Default::default());
                    for (view, plane) in views.iter_mut().zip(&mut wet[..wet_n]) {
                        *view = &mut **plane;
                    }
                    Planes::new(&heard[..heard_n])
                        .and_then(|input| {
                            PlanesMut::new(&mut views[..wet_n])
                                .and_then(|output| InputOutput::paired(input, output))
                        })
                        .map(|mut block| matrix.process(&mut block))
                };
                debug_assert!(
                    matches!(rendered, Ok(Ok(()))),
                    "every plane is a block long and the node's width: {rendered:?}"
                );
                if !matches!(rendered, Ok(Ok(()))) {
                    // Not rendered: silent, never stale output.
                    for plane in &mut wet[..wet_n] {
                        plane.fill(F::ZERO);
                    }
                }
            }
        }
        // Every channel reads the driven control signal from the same last finite value.
        let good = self.good;
        for c in 0..n {
            let out = &mut outputs.get(c)[at..at + size];
            match &gain {
                WetGain::Driven(signal) => {
                    self.good = good;
                    Self::apply_driven_gain(out, signal, &mut self.good);
                }
                WetGain::Ramped(ramp, held) => Self::apply_gain(out, ramp, *held),
            }
        }
    }
}

// The params a [`ConvolverNode`] lets the graph modulate: the wet gain.
tutti_graph::declare_params!([F: FftFloat + Lerp] ConvolverNode<F>, n {
    Volume => Amplitude { whole } = &n.gain,
});

impl<F: FftFloat + Lerp> ConvolverNode<F> {
    /// Renders frames `span` of the block, in pieces of the scratch's length (one piece in a
    /// graph, whose blocks fit it; a longer block driven by hand renders in several).
    fn render_span(
        &mut self,
        span: std::ops::Range<usize>,
        inputs: &Inputs<'_, F>,
        outputs: &mut Outputs<'_, '_, F>,
        driven: Option<&[F]>,
    ) {
        // A folded node renders in pieces its scratch holds; the others take the span whole.
        let step = match &self.engine {
            Engine::Folded { mono, .. } => mono.len().max(1),
            Engine::PerChannel(_) | Engine::Matrix(_) => usize::MAX,
        };
        let mut at = span.start;
        while at < span.end {
            let n = step.min(span.end - at);
            self.render(at, n, inputs, outputs, driven);
            at += n;
        }
    }

    /// Zeros the outputs of a silent run that began at `run_start` after `quiet_before` silent
    /// frames, from the frame it stops sounding (`owed` frames after the last input) to `end`.
    fn silence_past_tail(
        outputs: &mut Outputs<'_, '_, F>,
        run_start: usize,
        quiet_before: u64,
        owed: u64,
        end: usize,
    ) {
        let sounding = usize::try_from(owed.saturating_sub(quiet_before)).unwrap_or(usize::MAX);
        let from = run_start.saturating_add(sounding);
        if from >= end {
            return;
        }
        for c in 0..outputs.len() {
            outputs.get(c)[from..end].fill(F::ZERO);
        }
    }
}

impl<F: FftFloat + Lerp> Node<F> for ConvolverNode<F> {
    /// `N` in, `N` out; one FFT block of latency for the whole output (none for a
    /// [`zero_latency`](ConvolverConfig::zero_latency) node), and the
    /// longest IR's ring-out as its tail — exact rather than estimated, a
    /// convolution being linear and finite. The tail excludes the FFT block
    /// latency, which is declared on its own and compensated.
    fn shape(&self) -> Shape {
        Shape::buses(Buses::main(self.inputs), Buses::main(self.outputs))
            .latency(Latency::new(self.latency_samples))
            .tail(ring_after(self.engine.ir_length(), self.declared_delay))
            .param_ports(self.declared_ports())
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }

    fn prepare(&mut self, p: &Prepare) {
        if let Engine::Folded { mono, .. } = &mut self.engine {
            *mono = vec![F::ZERO; p.max_block().get()];
        }
    }

    /// Channel-outer over planar slices: each channel's convolver consumes its
    /// whole block in partition-sized runs (a matrix runs every path), then the
    /// gain runs over the block.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let size = io.frames();
        let n_in = usize::from(self.inputs.width().count());
        let all_silent = io.silent().covers(n_in);
        // The last frame of the block whose input is not zero, on any channel.
        let last = if all_silent {
            None
        } else {
            (0..n_in)
                .filter_map(|c| io.input(c)[..size].iter().rposition(|&s| s != F::ZERO))
                .max()
        };
        let ring = self.clock.ceiling();
        let silent_for = self.clock.silent_for();
        let owed = self.clock.advance(
            ring,
            Heard {
                size,
                last_input: last,
            },
        );
        if owed.live == 0 {
            // Wholly past the tail, with silent input: nothing rings, and the output is
            // silence (the graph zeroes it).
            return Status::Silent;
        }
        let driven = self
            .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
            .narrowed();
        let (inputs, mut outputs) = io.split();
        // Walk the block's silent runs: cut the output where a run stops sounding and restart
        // the engine where input resumes after one that long (see the type's docs).
        let owed_frames = ring.saturating_add(self.latency_samples.get() as u64);
        let nonzero = |n: usize| !all_silent && (0..n_in).any(|c| inputs.get(c)[n] != F::ZERO);
        // The silent run in progress: it began at `run_start`, after `quiet_before` silent
        // frames that came before it (`silent_for` for the block's first run).
        let (mut run_start, mut quiet_before, mut rendered) = (0usize, silent_for, 0usize);
        for n in 0..size {
            if !nonzero(n) {
                continue;
            }
            let gap = quiet_before.saturating_add((n - run_start) as u64);
            if gap >= owed_frames {
                self.render_span(rendered..n, &inputs, &mut outputs, driven);
                Self::silence_past_tail(&mut outputs, run_start, quiet_before, owed_frames, n);
                self.engine.reset();
                self.last = None;
                rendered = n;
            }
            run_start = n + 1;
            quiet_before = 0;
        }
        self.render_span(rendered..size, &inputs, &mut outputs, driven);
        Self::silence_past_tail(&mut outputs, run_start, quiet_before, owed_frames, size);
        Status::Modified
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.last = None;
        self.clock.clear();
    }
}

impl<F: FftFloat + Lerp> ParamNode<F> for ConvolverNode<F> {
    /// The wet gain.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A node over the same IR spectra (shared: they are read-only) with
    /// fresh running state — nothing of this node's history is copied — and
    /// the gain cell detached at its value now.
    fn fork_fresh(&self) -> Self {
        let mut gain = self.gain.clone();
        gain.detach();
        Self {
            engine: self.engine.fresh(),
            inputs: self.inputs,
            outputs: self.outputs,
            gain,
            latency_samples: self.latency_samples,
            declared_delay: self.declared_delay,
            clock: TailClock::new(self.clock.ceiling(), self.latency_samples.get() as u64),
            last: None,
            good: self.good,
        }
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that
/// detaches the gain ([`tutti_graph::param_parts`]).
impl<F: FftFloat + Lerp> IntoNode<F> for ConvolverNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convolution::test_nodes;
    use crate::test_support::{RATE, drive_block, prepared};
    use std::sync::Arc;
    use tutti_fixtures::signal::{decay_ir, room_ir};
    use tutti_graph::contract::drive;
    use tutti_units::SampleRate;
    use tutti_units::Seconds;

    /// A node has a channel: a bus of none, or no IR for it, is refused by name.
    #[test]
    fn a_node_of_no_channels_is_refused() {
        let empty = |irs: &[Mono<'_, f32>], layout: IrLayout| {
            ConvolverConfig::new(
                ChannelTopology::EMPTY,
                layout,
                Arc::new(ImpulseResponses::new(irs)),
            )
            .err()
        };
        assert_eq!(
            empty(&[], IrLayout::PerChannel),
            Some(ConvolverError::NoChannels)
        );
        assert_eq!(
            empty(&[], IrLayout::Folded),
            Some(ConvolverError::NoChannels)
        );
        assert_eq!(
            empty(&[Mono::new(&[1.0])], IrLayout::PerChannel),
            Some(ConvolverError::NoChannels)
        );
    }

    /// A per-channel node takes one response per channel, or one alone for them all; any
    /// other count is refused, not truncated or cycled.
    #[test]
    fn a_per_channel_node_takes_one_response_per_channel_or_one_alone() {
        let one = [1.0_f32];
        let irs = |n: usize| vec![Mono::new(&one[..]); n];
        let build =
            |n: usize| test_nodes::per_channel_node::<f32, f32>(&irs(n), Samples::new(64)).err();
        assert_eq!(build(3), None);
        assert_eq!(build(1), None);
        let config = |n: usize| {
            ConvolverConfig::new(
                ChannelTopology::STEREO,
                IrLayout::PerChannel,
                Arc::new(ImpulseResponses::new(&irs(n))),
            )
            .err()
        };
        assert_eq!(config(2), None);
        assert_eq!(config(1), None);
        assert_eq!(config(3), Some(ConvolverError::IrCount { want: 2, got: 3 }));
        assert_eq!(config(0), Some(ConvolverError::IrCount { want: 2, got: 0 }));
    }

    /// A folded node needs a width with a fold to mono: 3 has no order, 2 has.
    #[test]
    fn a_folded_width_with_no_fold_to_mono_is_refused() {
        let ir = [1.0_f32];
        assert!(matches!(
            test_nodes::folded_node::<f32, _>(
                &[Mono::new(&ir), Mono::new(&ir), Mono::new(&ir)],
                Samples::new(64)
            ),
            Err(ConvolverError::NoFold { width: 3, .. })
        ));
        assert!(
            test_nodes::folded_node::<f32, _>(&[Mono::new(&ir), Mono::new(&ir)], Samples::new(64))
                .is_ok()
        );
    }

    /// The "control changed between two blocks" experiment.
    ///
    /// Three nodes from `make` render `block1` identically. Then `change` is
    /// applied to two of them: the render `r` is of `block2` through `process` (the
    /// per-block read, which ramps), `j` through `tick` (a block of one per
    /// sample, so the change lands whole on the first sample, as a per-sample read
    /// would). `h`, of the third, never sees the change.
    pub(super) struct ChangeRun {
        pub(super) r: Vec<Vec<f32>>,
        pub(super) j: Vec<Vec<f32>>,
        pub(super) h: Vec<Vec<f32>>,
    }

    /// A node over `inputs`, one frame per call: a change lands whole on
    /// the first frame.
    fn drive_frames(node: &mut dyn Node, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
        let n = inputs[0].len();
        let mut out: Vec<Vec<f32>> = Vec::new();
        for i in 0..n {
            let frame: Vec<&[f32]> = inputs.iter().map(|s| &s[i..=i]).collect();
            let o = drive(node, RATE, Planes::new(&frame).unwrap(), &[]);
            out.resize(o.len(), Vec::with_capacity(n));
            for (c, v) in o.into_iter().enumerate() {
                out[c].push(v[0]);
            }
        }
        out
    }

    /// The change-between-blocks experiment for a graph node: `make` builds it unprepared,
    /// and each copy is [`prepared`]. `jumped` renders `block2` one frame per
    /// call ([`drive_frames`]).
    pub(super) fn change_between_node_blocks<N: Node>(
        make: impl Fn() -> N,
        change: impl Fn(&N),
        block1: &[&[f32]],
        block2: &[&[f32]],
    ) -> ChangeRun {
        let (mut ramped, mut jumped, mut held) =
            (prepared(make()), prepared(make()), prepared(make()));
        for n in [&mut ramped, &mut jumped, &mut held] {
            drive_block(n, block1);
        }
        change(&ramped);
        change(&jumped);
        let r = drive_block(&mut ramped, block2);
        let j = drive_frames(&mut jumped, block2);
        let h = drive_block(&mut held, block2);
        ChangeRun { r, j, h }
    }

    impl ChangeRun {
        /// The change is ramped *into* the block: on the block's first sample the
        /// ramped render is still near the unchanged one — at most half as far from
        /// it as the render that took the change whole — on every channel, and the
        /// change is audible there at all (or the comparison proves nothing).
        #[track_caller]
        pub(super) fn assert_ramps_in(&self, what: &str) {
            for c in 0..self.r.len() {
                let (r, j, h) = (self.r[c][0], self.j[c][0], self.h[c][0]);
                assert!(
                    (j - h).abs() > 1e-4,
                    "{what}: channel {c}: the change is inaudible on the first sample \
                     ({j} vs {h}), so this run cannot tell a ramp from a jump"
                );
                assert!(
                    (r - h).abs() < 0.5 * (j - h).abs(),
                    "{what}: channel {c}: the first sample after the change is {r}; \
                     unchanged it is {h}, jumped {j} — the change was not ramped in"
                );
            }
        }
    }

    /// The node offers one param to the graph, the wet gain as `Volume` (the mix
    /// is the graph's), and a fork keeps the gain as it was when forked and
    /// follows no live write.
    #[test]
    fn a_fork_keeps_the_gain_and_shares_no_cell() {
        let node = test_nodes::per_channel_node::<f32, _>(
            &[Mono::new(&[1.0, 0.5]), Mono::new(&[0.25])],
            Samples::new(64),
        )
        .unwrap();
        assert_eq!(node.param_set().params().count(), 1);
        assert!(node.param_set().set(UnitParam::Volume, 0.7));
        assert_eq!(node.gain().load(), Amplitude::new(0.7));
        node.gain().store(Amplitude::new(0.3));
        let fork = node.fork_fresh();
        node.gain().store(Amplitude::new(2.0));
        assert_eq!(fork.gain.load(), Amplitude::new(0.3));
    }

    /// A gain driven on the `Volume` port replaces the cell, frame by frame: a
    /// unit impulse response returns the input delayed by the latency, scaled by
    /// each frame's driven value, and a negative frame is floored at 0.
    #[test]
    fn a_driven_gain_scales_each_frame_and_floors_at_zero() {
        let mut node = prepared(test_nodes::mono_node::<f32, _>(
            Mono::new(&[1.0]),
            Samples::new(64),
        ));
        node.gain().store(Amplitude::new(5.0));
        let latency = node.latency_samples().get();
        let signal: Vec<f32> = (0..256).map(|i| [0.5, 1.0, -2.0, 0.25][i % 4]).collect();
        let out = drive(
            &mut node,
            RATE,
            Planes::new(&[&[1.0; 256]]).unwrap(),
            &[Some(&signal)],
        );
        for i in latency..256 {
            let want = signal[i].max(0.0);
            assert!(
                (out[0][i] - want).abs() < 1e-4,
                "frame {i} is {} want {want}",
                out[0][i]
            );
        }
    }

    /// A non-finite driven frame reads as the last finite one, and every channel
    /// reads the same one: a block opening on NaN holds the previous block's
    /// last value (0.5) on both channels of a stereo node.
    #[test]
    fn a_non_finite_driven_frame_holds_the_same_value_on_every_channel() {
        let mut node = prepared(
            test_nodes::per_channel_node::<f32, _>(
                &[Mono::new(&[1.0]), Mono::new(&[1.0])],
                Samples::new(64),
            )
            .unwrap(),
        );
        let ones = [1.0; 256];
        drive(
            &mut node,
            RATE,
            Planes::new(&[&ones, &ones]).unwrap(),
            &[Some(&[0.5; 256])],
        );
        let mut signal = [0.25; 256];
        signal[0] = f32::NAN;
        let out = drive(
            &mut node,
            RATE,
            Planes::new(&[&ones, &ones]).unwrap(),
            &[Some(&signal)],
        );
        assert_eq!(out[0][0], 0.5);
        assert_eq!(out[1][0], 0.5);
    }

    /// One IR is transformed once: every channel of a shared-IR node, and
    /// every channel of a fork, reads the same spectra.
    #[test]
    fn the_spectra_are_stored_once_and_a_fork_shares_them() {
        let ir = decay_ir(4_096, Seconds::new(0.3), SampleRate::SR_48K);
        let node =
            test_nodes::shared_ir_node::<f32, _>(6u16, Mono::new(&ir), Samples::new(256)).unwrap();
        let spectra = |n: &ConvolverNode<f32>| match &n.engine {
            Engine::PerChannel(cs) => cs
                .iter()
                .map(|c| Arc::clone(c.spectra()))
                .collect::<Vec<_>>(),
            Engine::Folded { .. } | Engine::Matrix(_) => {
                unreachable!("a shared IR is per channel")
            }
        };
        let all = spectra(&node);
        let first = Arc::clone(&all[0]);
        assert!(
            all.iter().all(|s| Arc::ptr_eq(s, &first)),
            "every channel reads the one transform"
        );
        let fork = node.fork_fresh();
        assert!(spectra(&fork).iter().all(|s| Arc::ptr_eq(s, &first)));
        drop(all);
        // Each channel, the fork and the node, and the local handle: one set of spectra.
        assert_eq!(Arc::strong_count(&first), 13);
    }

    /// A fork starts from a fresh node's state, not the live node's: after
    /// the live node has run, the fork renders what a new node does.
    #[test]
    fn a_fork_carries_none_of_the_live_state() {
        let ir = decay_ir(700, Seconds::new(0.3), SampleRate::SR_48K);
        let x = crate::test_support::noise(5, 512);
        let mut live = prepared(test_nodes::true_stereo_node::<f32, _>(
            Mono::new(&ir),
            Mono::new(&ir),
            Mono::new(&ir),
            Mono::new(&ir),
            Samples::new(64),
        ));
        drive_block(&mut live, &[&x, &x]);
        let mut fork = prepared(live.fork_fresh());
        let mut new = prepared(test_nodes::true_stereo_node::<f32, _>(
            Mono::new(&ir),
            Mono::new(&ir),
            Mono::new(&ir),
            Mono::new(&ir),
            Samples::new(64),
        ));
        let silent = [0.0f32; 512];
        assert_eq!(
            drive_block(&mut fork, &[&silent, &silent]),
            drive_block(&mut new, &[&silent, &silent])
        );
    }

    /// An IR long enough to be cut into levels, through the node at call
    /// lengths that are not a multiple of anything: the wet output is the
    /// direct convolution, `latency_samples` late, within round-off, and the
    /// same bits at every call length.
    ///
    /// The tolerance is 2 eps log2(n) |h|_2 |x|_2 (see the convolver's
    /// `tolerance`), about 3e-4 here, and that is the bound used.
    #[test]
    fn a_long_ir_cut_into_levels_convolves_at_any_block_size() {
        let ir: Vec<f32> = crate::test_support::noise::<f32>(1, 6000)
            .iter()
            .map(|v| v * 0.1)
            .collect();
        let x = crate::test_support::noise(2, 2500);
        let node = test_nodes::mono_node::<f32, _>(Mono::new(&ir), Samples::new(32));
        let lat = node.latency_samples().get();
        assert_eq!(lat, 32);
        let mut want = vec![0.0f64; x.len() + lat];
        for (n, &xn) in x.iter().enumerate() {
            for (k, &h) in ir.iter().enumerate() {
                if n + k + lat < want.len() {
                    want[n + k + lat] += f64::from(xn) * f64::from(h);
                }
            }
        }
        let mut renders = Vec::new();
        for lens in [&[1usize][..], &[7, 61, 64, 127], &[1000, 3]] {
            let mut node = prepared(test_nodes::mono_node::<f32, _>(
                Mono::new(&ir),
                Samples::new(32),
            ));
            let (mut at, mut k) = (0, 0);
            let mut out = Vec::new();
            while at < x.len() {
                let n = lens[k % lens.len()].min(x.len() - at);
                out.extend(drive_block(&mut node, &[&x[at..at + n]]).remove(0));
                at += n;
                k += 1;
            }
            for (i, (&g, &w)) in out.iter().zip(&want).enumerate() {
                assert!(
                    (f64::from(g) - w).abs() < 3e-4,
                    "calls {lens:?}: frame {i} is {g} want {w}"
                );
            }
            renders.push(out);
        }
        for r in &renders[1..] {
            assert!(
                r.iter()
                    .zip(&renders[0])
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "the render depends on the call lengths"
            );
        }
    }

    /// The wet gain is read floored at 0: a negative gain in the cell would invert the wet
    /// path's polarity, and renders as silence instead. A unit impulse response returns the
    /// input scaled by the gain.
    #[test]
    fn the_gain_is_floored_at_zero() {
        let render = |gain: f64| {
            let mut node = prepared(test_nodes::mono_node::<f32, _>(
                Mono::new(&[1.0]),
                Samples::new(64),
            ));
            node.gain().store(Amplitude::new(gain));
            let latency = node.latency_samples().get();
            let out = drive(
                &mut node,
                RATE,
                Planes::new(&[&[1.0; 256]]).unwrap(),
                &[None],
            );
            out[0][latency..].to_vec()
        };
        assert!(render(1.5).iter().all(|&y| (y - 1.5).abs() < 1e-6));
        assert!(render(-0.5).iter().all(|&y| y == 0.0));
    }

    #[test]
    fn room_ir_has_content() {
        let ir = room_ir(0.5, Seconds::new(1.0), SampleRate::SR_48K);
        assert!(!ir.is_empty());
        assert_eq!(ir[0], 1.0);
        assert!(ir.iter().any(|&x| x != 0.0));
    }
}

#[cfg(test)]
mod direct_sum {
    use super::*;
    use crate::convolution::test_nodes;
    use tutti_fixtures::signal::decay_ir;
    use tutti_graph::contract::{drive, prepared};
    use tutti_units::{SampleRate, Seconds};

    const BLOCK: usize = 64;
    const FRAMES: usize = 1500;

    fn signal(channels: usize, frames: usize) -> Vec<Vec<f32>> {
        let mut state = 0x1234_5678_u32;
        (0..channels)
            .map(|_| {
                (0..frames)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
                    })
                    .collect()
            })
            .collect()
    }

    /// `node` over `x` in `BLOCK`-frame blocks (the last one short), as a
    /// graph running at a `BLOCK`-frame quantum hands it.
    fn render(node: ConvolverNode<f32>, x: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let rate = SampleRate::SR_48K;
        let mut node = prepared(node, rate, BLOCK);
        let frames = x[0].len();
        let mut out = vec![Vec::with_capacity(frames); x.len()];
        let mut at = 0;
        while at < frames {
            let n = BLOCK.min(frames - at);
            let ins: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
            for (o, b) in
                out.iter_mut()
                    .zip(drive(&mut node, rate, Planes::new(&ins).unwrap(), &[]))
            {
                o.extend(b);
            }
            at += n;
        }
        out
    }

    /// Every configuration is the direct-form sum of its definition: output
    /// channel `o` is `gain * sum_k ir_o[k] * x_c[n - k]`, `latency_samples`
    /// late, with `c` the input channel that output reads (width 1, a shared
    /// IR per channel, one IR per channel, and the mono fold of a stereo input
    /// feeding two IRs; the fold is `tutti_buffers::Fold`'s, checked on its own). The sum is computed here in `f64` from the definition.
    ///
    /// The tolerance is `1e-4`: the FFT's twiddle factors come from the
    /// platform's `sin`/`cos`, and the sum has hundreds of products of
    /// `|x| <= 1`, `|h| <= 1`, whose round-off is well under it; a real
    /// fault (a wrong tap, channel or delay) moves samples by orders of
    /// magnitude more.
    #[test]
    fn every_configuration_is_the_direct_sum_of_its_definition() {
        let ir_a = decay_ir(700, Seconds::new(0.3), SampleRate::SR_48K);
        let ir_b = decay_ir(300, Seconds::new(0.5), SampleRate::SR_48K);
        /// The input channel of an output that reads the mono fold of every input.
        const FOLDED: usize = usize::MAX;
        let gain = 0.7_f64;
        // (name, node, per output channel: (the IR, the input channel it reads))
        let cases: Vec<(&str, ConvolverNode<f32>, Vec<(&[f32], usize)>)> = vec![
            (
                "width1",
                test_nodes::mono_node::<f32, _>(Mono::new(&ir_a), Samples::new(64)),
                vec![(&ir_a[..], 0)],
            ),
            (
                "shared_ir",
                test_nodes::shared_ir_node::<f32, _>(2u16, Mono::new(&ir_a), Samples::new(32))
                    .unwrap(),
                vec![(&ir_a[..], 0), (&ir_a[..], 1)],
            ),
            (
                "per_channel",
                test_nodes::per_channel_node::<f32, _>(
                    &[Mono::new(&ir_a), Mono::new(&ir_b)],
                    Samples::new(128),
                )
                .unwrap(),
                vec![(&ir_a[..], 0), (&ir_b[..], 1)],
            ),
            (
                "folded",
                test_nodes::folded_node::<f32, _>(
                    &[Mono::new(&ir_a), Mono::new(&ir_b)],
                    Samples::new(16),
                )
                .unwrap(),
                vec![(&ir_a[..], FOLDED), (&ir_b[..], FOLDED)],
            ),
        ];
        for (name, node, outputs) in cases {
            node.gain().store(Amplitude::new(gain));
            let lat = node.latency_samples().get();
            let x = signal(usize::from(node.inputs().count()), FRAMES);
            let mut mono = vec![0.0f32; FRAMES];
            if x.len() == 2 {
                Fold::between(
                    &tutti_buffers::Bed::Stereo.topology(),
                    &tutti_buffers::Bed::Mono.topology(),
                )
                .expect("stereo folds to mono")
                .fold_planar_channel(Planes::from_vecs(&x).unwrap(), 0, MonoMut::new(&mut mono))
                .unwrap();
            }
            let out = render(node, &x);
            assert_eq!(out.len(), outputs.len(), "{name}: width");
            for (o, (oc, (ir, input))) in out.iter().zip(&outputs).enumerate() {
                let feed = if *input == FOLDED { &mono } else { &x[*input] };
                for (n, &g) in oc.iter().enumerate() {
                    let want: f64 = n
                        .checked_sub(lat)
                        .map(|m| {
                            (0..=m.min(ir.len() - 1))
                                .map(|k| f64::from(ir[k]) * f64::from(feed[m - k]))
                                .sum::<f64>()
                        })
                        .unwrap_or(0.0)
                        * gain;
                    assert!(
                        (f64::from(g) - want).abs() < 1e-4,
                        "{name} out {o} frame {n}: {g} vs direct sum {want}"
                    );
                }
            }
        }
    }

    /// Six channels, six IRs: each channel is exactly a width-1 node with its
    /// own IR on its own input — so nothing bleeds between channels and the
    /// width is not secretly two.
    ///
    /// Distinct inputs per channel are what make "no bleed" follow from the
    /// equality: a channel that read any other channel's input would diverge.
    #[test]
    fn six_channels_convolve_independently() {
        let irs: Vec<Vec<f32>> = (0..6)
            .map(|c| {
                decay_ir(
                    200 + 90 * c,
                    tutti_units::Seconds::new(0.2 + 0.05 * c as f64),
                    SampleRate::SR_48K,
                )
            })
            .collect();
        let refs: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
        let wide = test_nodes::per_channel_node::<f32, _>(&refs, Samples::new(64)).unwrap();
        let shape = wide.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (6, 6)
        );
        assert_eq!(wide.outputs(), ChannelLayout::from(6u16));
        let x = signal(6, 900);
        let got = render(wide, &x);
        for c in 0..6 {
            let solo = test_nodes::mono_node::<f32, _>(Mono::new(&irs[c]), Samples::new(64));
            let want = render(solo, &x[c..=c]);
            for (i, (g, w)) in got[c].iter().zip(&want[0]).enumerate() {
                assert_eq!(g.to_bits(), w.to_bits(), "ch{c} frame {i}: {g} vs {w}");
            }
        }
    }

    /// A shared IR at width 6: an impulse on channel 3 comes out of channel 3
    /// alone, exactly `latency_samples` late (the graph's dry half is
    /// `node_mix.rs`'s).
    #[test]
    fn a_six_channel_shared_ir_keeps_each_channel_to_itself_and_aligned() {
        let node = test_nodes::shared_ir_node::<f32, _>(6u16, Mono::new(&[1.0]), Samples::new(64))
            .unwrap();
        let latency = node.latency_samples().get();
        assert_eq!(node.shape().latency, Latency::new(Samples::new(latency)));
        let mut x = vec![vec![0.0f32; 4 * latency]; 6];
        x[3][0] = 1.0;
        let out = render(node, &x);
        for (c, oc) in out.iter().enumerate() {
            for (i, &s) in oc.iter().enumerate() {
                let want = if c == 3 && i == latency { 1.0 } else { 0.0 };
                assert!(
                    (s - want).abs() < 1e-6,
                    "ch{c} frame {i}: {s}, expected {want}"
                );
            }
        }
    }

    /// Folded at width 6: every output is the engine's 6-to-1 fold of the frame
    /// convolved with that output's IR (the node is wet only, so the IR path shows alone).
    #[test]
    fn a_six_channel_fold_convolves_the_engine_fold() {
        let irs: Vec<Vec<f32>> = (0..6)
            .map(|c| decay_ir(150 + 40 * c, Seconds::new(0.3), SampleRate::SR_48K))
            .collect();
        let refs: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
        let node = test_nodes::folded_node::<f32, _>(&refs, Samples::new(32)).unwrap();
        let x = signal(6, 700);
        let got = render(node, &x);
        let to_mono = Fold::between(
            &tutti_buffers::Bed::Surround51.topology(),
            &tutti_buffers::Bed::Mono.topology(),
        )
        .unwrap();
        let mono: Vec<f32> = (0..700)
            .map(|i| {
                let frame: Vec<f32> = x.iter().map(|xc| xc[i]).collect();
                let mut mono = [0.0f32];
                to_mono.frame(&frame, &mut mono);
                mono[0]
            })
            .collect();
        for c in 0..6 {
            let solo = test_nodes::mono_node::<f32, _>(Mono::new(&irs[c]), Samples::new(32));
            let want = render(solo, std::slice::from_ref(&mono));
            for (i, (g, w)) in got[c].iter().zip(&want[0]).enumerate() {
                assert_eq!(g.to_bits(), w.to_bits(), "ch{c} frame {i}: {g} vs {w}");
            }
        }
    }

    /// A wet-gain change made between blocks is read by the next block
    /// and ramped across it (it scales the output directly, so a step
    /// clicks), ending exactly on the new value. (A mix change is the graph's:
    /// `tutti-graph/tests/mixing/mix.rs`.)
    #[test]
    fn a_gain_change_is_read_next_block_and_ramped_across_it() {
        use super::tests::change_between_node_blocks;
        use crate::test_support::noise;
        let ir = decay_ir(256, Seconds::new(0.2), SampleRate::SR_48K);
        let x = noise(21, 64 * 8);
        let (hist, block) = (&x[..64 * 7], &x[64 * 7..]);
        let make = || {
            test_nodes::shared_ir_node::<f32, _>(
                ChannelLayout::STEREO,
                Mono::new(&ir),
                Samples::new(64),
            )
            .unwrap()
        };
        let run = change_between_node_blocks(
            make,
            |n| n.gain().store(Amplitude::new(4.0)),
            &[hist, hist],
            &[block, block],
        );
        run.assert_ramps_in("gain");
        for c in 0..2 {
            assert_eq!(
                run.r[c][63].to_bits(),
                run.j[c][63].to_bits(),
                "gain: ch{c}: the block must end exactly on the new value"
            );
        }
    }
}

/// The matrix layouts and per-channel dual mono, against convolution's definition.
#[cfg(test)]
mod layouts {
    use super::*;
    use crate::convolution::test_nodes;
    use crate::test_support::{RATE, noise, prepared};
    use std::sync::Arc;
    use tutti_buffers::{AmbisonicFormat, Bed, ChannelTopology};
    use tutti_graph::contract::drive;

    const BLOCK: usize = 64;

    /// A response of one tap: `gain` at `delay`.
    fn tap(delay: usize, gain: f32) -> Vec<f32> {
        let mut ir = vec![0.0; delay + 1];
        ir[delay] = gain;
        ir
    }

    fn views(irs: &[Vec<f32>]) -> Vec<Mono<'_, f32>> {
        irs.iter().map(|ir| Mono::new(ir)).collect()
    }

    /// The node over `x` in calls of `lens` (cycled), prepared for 1024-frame blocks.
    fn render(node: &mut ConvolverNode<f32>, x: &[Vec<f32>], lens: &[usize]) -> Vec<Vec<f32>> {
        let frames = x[0].len();
        let mut out = vec![Vec::new(); x.len()];
        let (mut at, mut k) = (0, 0);
        while at < frames {
            let n = lens[k % lens.len()].min(frames - at);
            let ins: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
            for (o, b) in out
                .iter_mut()
                .zip(drive(node, RATE, Planes::new(&ins).unwrap(), &[]))
            {
                o.extend(b);
            }
            at += n;
            k += 1;
        }
        out
    }

    /// [`render`] in blocks of [`BLOCK`] for a node with `outputs` channels out.
    fn render_wide(node: &mut ConvolverNode<f32>, x: &[Vec<f32>], outputs: usize) -> Vec<Vec<f32>> {
        let frames = x[0].len();
        let mut out = vec![Vec::new(); outputs];
        let mut at = 0;
        while at < frames {
            let n = BLOCK.min(frames - at);
            let ins: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
            for (o, b) in out
                .iter_mut()
                .zip(drive(node, RATE, Planes::new(&ins).unwrap(), &[]))
            {
                o.extend(b);
            }
            at += n;
        }
        out
    }

    /// An impulse on channel `j` of a `width`-wide node, `frames` long.
    fn impulse_on(width: usize, j: usize, frames: usize) -> Vec<Vec<f32>> {
        let mut x = vec![vec![0.0f32; frames]; width];
        x[j][0] = 1.0;
        x
    }

    /// The four paths of a true stereo node are four distinct single taps. An impulse on
    /// one input puts exactly the two responses of its row on the two outputs, at the
    /// declared latency plus the tap's delay and with the tap's gain, so a swapped or
    /// dropped path is a different figure; both inputs at once are the sum, by linearity.
    #[test]
    fn true_stereo_routes_each_input_through_its_row_of_paths() {
        // `[LL, LR, RL, RR]`, `XY` being input `X` to output `Y`.
        let irs = [tap(3, 0.5), tap(9, -0.25), tap(14, 0.75), tap(6, 0.125)];
        let make = || {
            prepared(test_nodes::true_stereo_node::<f32, _>(
                Mono::new(&irs[0]),
                Mono::new(&irs[1]),
                Mono::new(&irs[2]),
                Mono::new(&irs[3]),
                Samples::new(BLOCK),
            ))
        };
        let lat = make().latency_samples().get();
        for j in 0..2 {
            let got = render(&mut make(), &impulse_on(2, j, 200), &[BLOCK]);
            for (o, g) in got.iter().enumerate() {
                let ir = &irs[j * 2 + o];
                let mut want = vec![0.0f32; 200];
                want[lat + ir.len() - 1] = ir[ir.len() - 1];
                for (n, (&g, &w)) in g.iter().zip(&want).enumerate() {
                    assert!((g - w).abs() < 1e-6, "in {j} out {o} frame {n}: {g} vs {w}");
                }
            }
        }
        let (a, b) = (noise(1, 300), noise(2, 300));
        let silent = vec![0.0f32; 300];
        let both = render(&mut make(), &[a.clone(), b.clone()], &[BLOCK]);
        let only_a = render(&mut make(), &[a, silent.clone()], &[BLOCK]);
        let only_b = render(&mut make(), &[silent, b], &[BLOCK]);
        for o in 0..2 {
            for n in 0..300 {
                let want = only_a[o][n] + only_b[o][n];
                assert!((both[o][n] - want).abs() < 1e-5, "out {o} frame {n}");
            }
        }
    }

    /// First-order ambisonic: sixteen paths, each with its own tap. An impulse on input `j`
    /// is, on every output `i`, the response of path `(j, i)`, and only that.
    #[test]
    fn an_ambisonic_impulse_on_an_input_is_each_outputs_path_response() {
        let format = AmbisonicFormat::ambix(tutti_buffers::AmbisonicOrder::of(1));
        let irs: Vec<Vec<f32>> = (0..16)
            .map(|p| tap(2 + 3 * p, 0.0625 * (p as f32 + 1.0)))
            .collect();
        let make = || {
            prepared(
                test_nodes::ambisonic_node::<f32, _>(
                    format,
                    ChannelTopology::from(format),
                    &views(&irs),
                    Samples::new(BLOCK),
                )
                .unwrap(),
            )
        };
        let lat = make().latency_samples().get();
        for j in 0..4 {
            let got = render(&mut make(), &impulse_on(4, j, 200), &[BLOCK]);
            for (i, g) in got.iter().enumerate() {
                let ir = &irs[j * 4 + i];
                let mut want = vec![0.0f32; 200];
                want[lat + ir.len() - 1] = ir[ir.len() - 1];
                for (n, (&g, &w)) in g.iter().zip(&want).enumerate() {
                    assert!((g - w).abs() < 1e-6, "in {j} out {i} frame {n}: {g} vs {w}");
                }
            }
        }
    }

    /// Per-channel at a stereo topology is dual mono: an impulse on L leaves its own IR on
    /// L and exactly nothing on R, and R likewise, whatever the other IR is.
    #[test]
    fn per_channel_stereo_is_dual_mono() {
        let (l, r) = (tap(4, 0.5), tap(11, -0.75));
        let make = || {
            prepared(ConvolverNode::<f32>::from(
                ConvolverConfig::new(
                    ChannelTopology::STEREO,
                    IrLayout::PerChannel,
                    Arc::new(ImpulseResponses::new(&views(&[l.clone(), r.clone()]))),
                )
                .unwrap()
                .block_size(Samples::new(BLOCK)),
            ))
        };
        let lat = make().latency_samples().get();
        let on_l = render(&mut make(), &impulse_on(2, 0, 200), &[BLOCK]);
        assert!((on_l[0][lat + 4] - 0.5).abs() < 1e-6);
        assert!(on_l[1].iter().all(|&v| v.abs() < 1e-6), "L leaked into R");
        let on_r = render(&mut make(), &impulse_on(2, 1, 200), &[BLOCK]);
        assert!((on_r[1][lat + 11] + 0.75).abs() < 1e-6);
        assert!(on_r[0].iter().all(|&v| v.abs() < 1e-6), "R leaked into L");
    }

    /// The ambisonic and true stereo nodes are the definition of a matrix convolution: output
    /// `i` is `sum_j x_j * h(j, i)`, `latency` late, on noise through long responses that
    /// the engine cuts into levels.
    #[test]
    fn a_matrix_node_is_the_direct_sum_of_its_paths() {
        let irs: Vec<Vec<f32>> = (0..4)
            .map(|p| {
                noise::<f32>(40 + p, 1500 + 700 * p as usize)
                    .iter()
                    .map(|v| v * 0.05)
                    .collect()
            })
            .collect();
        let x: Vec<Vec<f32>> = (0..2).map(|c| noise(3 + c, 2200)).collect();
        let mut node = prepared(test_nodes::true_stereo_node::<f32, _>(
            Mono::new(&irs[0]),
            Mono::new(&irs[1]),
            Mono::new(&irs[2]),
            Mono::new(&irs[3]),
            Samples::new(32),
        ));
        let lat = node.latency_samples().get();
        let got = render(&mut node, &x, &[100]);
        for (o, out) in got.iter().enumerate() {
            for (n, &g) in out.iter().enumerate() {
                let want: f64 = n.checked_sub(lat).map_or(0.0, |m| {
                    (0..2)
                        .map(|j| {
                            (0..=m.min(irs[j * 2 + o].len() - 1))
                                .map(|k| f64::from(irs[j * 2 + o][k]) * f64::from(x[j][m - k]))
                                .sum::<f64>()
                        })
                        .sum()
                });
                assert!(
                    (f64::from(g) - want).abs() < 3e-4,
                    "out {o} frame {n}: {g} vs {want}"
                );
            }
        }
    }

    /// A count of impulse responses the layout does not take, a true-stereo layout off a
    /// pair and responses of another shape are each refused by name.
    #[test]
    fn a_layout_that_does_not_fit_is_refused_by_name() {
        let one = [1.0f32];
        let irs = |n: usize| vec![Mono::new(&one[..]); n];
        let foa = AmbisonicFormat::ambix(tutti_buffers::AmbisonicOrder::of(1));
        let per_channel = |n: usize| {
            ConvolverConfig::new(
                ChannelTopology::STEREO,
                IrLayout::PerChannel,
                Arc::new(ImpulseResponses::new(&irs(n))),
            )
            .err()
        };
        let responses = |inputs: u16, outputs: u16, n: usize| {
            MatrixResponses::new(inputs.into(), outputs.into(), &irs(n))
        };
        let matrix = |output: ChannelTopology, layout: MatrixLayout, set: MatrixResponses<f32>| {
            ConvolverConfig::matrix(output, layout, Arc::new(set)).err()
        };

        assert_eq!(
            per_channel(3),
            Some(ConvolverError::IrCount { want: 2, got: 3 })
        );
        assert_eq!(
            responses(2, 2, 2).map_err(ConvolverError::from).err(),
            Some(ConvolverError::IrCount { want: 4, got: 2 })
        );
        assert_eq!(
            responses(4, 2, 4).map_err(ConvolverError::from).err(),
            Some(ConvolverError::IrCount { want: 8, got: 4 })
        );
        assert_eq!(
            matrix(
                ChannelTopology::MONO,
                MatrixLayout::TrueStereo,
                responses(1, 1, 1).unwrap()
            ),
            Some(ConvolverError::TrueStereoWidth { width: 1 })
        );
        assert_eq!(
            matrix(
                ChannelTopology::STEREO,
                MatrixLayout::Ambisonic(foa),
                responses(2, 2, 4).unwrap()
            ),
            Some(ConvolverError::MatrixShape {
                want_inputs: 4,
                want_outputs: 2,
                inputs: 2,
                outputs: 2
            })
        );
        assert_eq!(
            matrix(
                ChannelTopology::STEREO,
                MatrixLayout::TrueStereo,
                responses(2, 2, 4).unwrap()
            ),
            None
        );
    }

    /// First-order ambisonic in, stereo out: eight paths, each its own tap. An impulse on
    /// input `j` is, on each of the two outputs `i`, the response of path `(j, i)` and
    /// only that; the node's ports are four in and two out.
    #[test]
    fn first_order_ambisonic_to_stereo_routes_each_path_by_its_own_response() {
        let foa = AmbisonicFormat::ambix(tutti_buffers::AmbisonicOrder::of(1));
        let irs: Vec<Vec<f32>> = (0..8)
            .map(|p| tap(2 + 5 * p, 0.125 * (p as f32 + 1.0)))
            .collect();
        let make = || {
            prepared(
                test_nodes::ambisonic_node::<f32, _>(
                    foa,
                    ChannelTopology::from(Bed::Stereo),
                    &views(&irs),
                    Samples::new(BLOCK),
                )
                .unwrap(),
            )
        };
        let shape = make().shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (4, 2)
        );
        let lat = make().latency_samples().get();
        for j in 0..4 {
            let got = render_wide(&mut make(), &impulse_on(4, j, 200), 2);
            for (i, g) in got.iter().enumerate() {
                let ir = &irs[j * 2 + i];
                let mut want = vec![0.0f32; 200];
                want[lat + ir.len() - 1] = ir[ir.len() - 1];
                for (n, (&g, &w)) in g.iter().zip(&want).enumerate() {
                    assert!((g - w).abs() < 1e-6, "in {j} out {i} frame {n}: {g} vs {w}");
                }
            }
        }
    }

    /// A node's nodes of each layout, with responses long enough to be cut into levels.
    fn long_layouts() -> Vec<(&'static str, ConvolverNode<f32>)> {
        let long = |seed: u32, len: usize| -> Vec<f32> {
            noise::<f32>(seed, len).iter().map(|v| v * 0.05).collect()
        };
        let four: Vec<Vec<f32>> = (0..4)
            .map(|p| long(10 + p, 2500 + 90 * p as usize))
            .collect();
        let foa = AmbisonicFormat::ambix(tutti_buffers::AmbisonicOrder::of(1));
        let sixteen: Vec<Vec<f32>> = (0..16)
            .map(|p| long(50 + p, 600 + 40 * p as usize))
            .collect();
        let two: Vec<Vec<f32>> = (0..2).map(|p| long(90 + p, 3000)).collect();
        vec![
            (
                "true stereo",
                test_nodes::true_stereo_node::<f32, _>(
                    Mono::new(&four[0]),
                    Mono::new(&four[1]),
                    Mono::new(&four[2]),
                    Mono::new(&four[3]),
                    Samples::new(32),
                ),
            ),
            (
                "ambisonic",
                test_nodes::ambisonic_node::<f32, _>(
                    foa,
                    ChannelTopology::from(foa),
                    &views(&sixteen),
                    Samples::new(32),
                )
                .unwrap(),
            ),
            (
                "per channel",
                test_nodes::per_channel_node::<f32, _>(
                    &[Mono::new(&two[0]), Mono::new(&two[1])],
                    Samples::new(32),
                )
                .unwrap(),
            ),
        ]
    }

    /// Every call length, from one frame to past the partition and the whole stream, gives
    /// the same bits, in every layout.
    #[test]
    fn any_block_size_gives_the_same_bits_in_every_layout() {
        for (name, node) in long_layouts() {
            let width = usize::from(node.inputs().count());
            let x: Vec<Vec<f32>> = (0..width).map(|c| noise(5 + c as u32, 1800)).collect();
            let want = render(&mut prepared(node.fork_fresh()), &x, &[1800]);
            for lens in [&[1usize][..], &[7, 61, 64, 127], &[31, 33], &[900, 3]] {
                let got = render(&mut prepared(node.fork_fresh()), &x, lens);
                assert!(
                    got.iter()
                        .flatten()
                        .zip(want.iter().flatten())
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "{name}: calls {lens:?} differ from one call"
                );
            }
        }
    }

    /// A reset node renders the bits of a fresh one, and a clone made after running shares
    /// no state with the node it came from: rendering the clone first leaves the node's
    /// render what it would have been.
    #[test]
    fn reset_equals_fresh_and_a_clone_shares_nothing() {
        for (name, node) in long_layouts() {
            let width = usize::from(node.inputs().count());
            let warm: Vec<Vec<f32>> = (0..width).map(|c| noise(70 + c as u32, 700)).collect();
            let x: Vec<Vec<f32>> = (0..width).map(|c| noise(5 + c as u32, 900)).collect();
            let fresh = render(&mut prepared(node.fork_fresh()), &x, &[64]);

            let mut used = prepared(node.fork_fresh());
            render(&mut used, &warm, &[64]);
            Node::reset(&mut used);
            assert_eq!(render(&mut used, &x, &[64]), fresh, "{name}: reset");

            let mut a = prepared(node.fork_fresh());
            render(&mut a, &warm, &[64]);
            let mut b = a.clone();
            let from_clone = render(&mut b, &x, &[64]);
            let from_node = render(&mut a, &x, &[64]);
            assert_eq!(from_clone, from_node, "{name}: a clone is the node's state");
            assert_ne!(
                from_node, fresh,
                "{name}: the warm-up left nothing to share"
            );
        }
    }

    /// The declared latency is the partition and the tail is the longest response less one,
    /// in every layout: four taps at delays 40, 90, 10 and 200 make 200, and the long layouts'
    /// longest responses are 2770 (true stereo), 1200 (ambisonic, `600 + 40 * 15`) and 3000.
    #[test]
    fn latency_and_tail_are_as_declared() {
        let irs = [tap(40, 0.5), tap(90, -0.25), tap(10, 0.75), tap(200, 0.125)];
        let node = test_nodes::true_stereo_node::<f32, _>(
            Mono::new(&irs[0]),
            Mono::new(&irs[1]),
            Mono::new(&irs[2]),
            Mono::new(&irs[3]),
            Samples::new(64),
        );
        let shape = node.shape();
        assert_eq!(shape.latency, Latency::new(Samples::new(64)));
        assert_eq!(shape.tail, Tail::Finite(Samples::new(200)));
        for ((name, node), tail) in long_layouts().into_iter().zip([2769, 1199, 2999]) {
            let shape = node.shape();
            assert_eq!(shape.latency, Latency::new(Samples::new(32)), "{name}");
            assert_eq!(shape.tail, Tail::Finite(Samples::new(tail)), "{name}");
        }
    }

    /// An unprepared folded node renders (in pieces of its construction scratch) rather
    /// than index an empty one: an impulse on every input comes out of both outputs at the
    /// latency and nowhere before it.
    #[test]
    fn an_unprepared_folded_node_does_not_panic() {
        let mut node = test_nodes::folded_node::<f32, _>(
            &[Mono::new(&[1.0]), Mono::new(&[1.0])],
            Samples::new(32),
        )
        .unwrap();
        let lat = node.latency_samples().get();
        let out = drive(
            &mut node,
            RATE,
            Planes::new(&[&impulse_on(1, 0, 100)[0], &impulse_on(1, 0, 100)[0]]).unwrap(),
            &[],
        );
        for ch in &out {
            assert!(ch[..lat].iter().all(|&v| v.abs() < 1e-6));
            assert!(
                ch[lat].abs() > 0.1,
                "the impulse is at the latency: {}",
                ch[lat]
            );
        }
    }
}

#[cfg(test)]
mod in_an_f64_graph {
    use super::*;
    use crate::convolution::test_nodes;
    use crate::test_support::render_f64;
    use tutti_fixtures::signal::{assert_close, wide_signal};
    use tutti_units::SampleRate;

    /// A two-tap response `h` of two taps no `f32` holds is `y[n] = h0 x[n - L] + h1 x[n - L - 1]`
    /// for the latency `L` of the block, to `2^-40` (the FFT's rounding at `f64`, far under the
    /// `2^-25` an `f32` anywhere in the engine or the gain would leave).
    #[test]
    fn the_convolver_is_its_difference_equation_of_a_wide_signal() {
        const BLOCK: usize = 64;
        let h = [1.0 / 3.0, -1.0 / 7.0];
        let x = wide_signal(512);
        let node = test_nodes::mono_node::<f64, _>(Mono::new(&h), Samples::new(BLOCK));
        let latency = node.latency_samples().get();
        assert_eq!(latency, BLOCK);
        let out = render_f64(node, SampleRate::SR_48K, &[&x], x.len()).remove(0);
        let at = |n: usize, k: usize| n.checked_sub(latency + k).map_or(0.0, |m| x[m]);
        let want: Vec<f64> = (0..x.len())
            .map(|n| h[0] * at(n, 0) + h[1] * at(n, 1))
            .collect();
        assert_close(&out, &want, 2.0f64.powi(-40));
    }

    /// A one-tap response `h` at the wet gain `1/3` (which no `f32` holds) is `h x[n - L] / 3`,
    /// to `2^-40`: the gain's ramp and product run in `f64`.
    #[test]
    fn the_wet_gain_of_a_third_scales_a_wide_signal_in_f64() {
        const BLOCK: usize = 64;
        let h = [1.0 / 7.0];
        let x = wide_signal(4 * BLOCK);
        let node = test_nodes::mono_node::<f64, _>(Mono::new(&h), Samples::new(BLOCK));
        node.gain().store(Amplitude::new(1.0 / 3.0));
        let out = render_f64(node, SampleRate::SR_48K, &[&x], x.len()).remove(0);
        let want: Vec<f64> = (0..x.len())
            .map(|n| n.checked_sub(BLOCK).map_or(0.0, |m| h[0] * x[m] / 3.0))
            .collect();
        assert_close(&out, &want, 2.0f64.powi(-40));
    }

    /// A wet gain cell written infinite reads as the last finite gain, not as a saturated
    /// finite one: the block after the write is the earlier gain (`1/3`) times the signal.
    #[test]
    fn an_infinite_gain_cell_holds_the_last_finite_gain() {
        use tutti_graph::contract::Direct;
        const BLOCK: usize = 64;
        let h = [1.0 / 7.0];
        let x = wide_signal(BLOCK);
        let node = test_nodes::mono_node::<f64, _>(Mono::new(&h), Samples::new(BLOCK));
        let cell = node.gain();
        cell.store(Amplitude::new(1.0 / 3.0));
        let mut d = Direct::<_, f64>::with_format(node, SampleRate::SR_48K, BLOCK);
        d.inputs_mut()
            .plane_mut(0)
            .samples_mut()
            .copy_from_slice(&x);
        d.block();
        cell.store(Amplitude::new(f64::INFINITY));
        d.block();
        // The latency is one block, so the second block holds the first block's input.
        let want: Vec<f64> = x.iter().map(|v| h[0] * v / 3.0).collect();
        assert_close(d.output().plane(0).samples(), &want, 2.0f64.powi(-40));
    }
}
