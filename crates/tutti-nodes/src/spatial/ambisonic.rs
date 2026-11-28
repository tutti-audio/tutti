//! Ambisonic nodes in the AmbiX convention (ACN order, SN3D or N3D): [`AmbisonicEncoderNode`]
//! places a mono source, [`AmbisonicRotatorNode`] turns a sound field and
//! [`AmbisonicDecoderNode`] feeds the speakers of a bed. The algorithms are
//! `tutti_dsp`'s ([`tutti_dsp::Harmonics`], [`Rotation`], [`AmbisonicDecoder`]).

use core::marker::PhantomData;

use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, AmbisonicOrdering, ChannelTopology, Float, InputOutput,
    MaxBlock, Mono,
};
use tutti_dsp::{
    AmbisonicDecoder, AmbisonicEncoder, AmbisonicRotator, BedLayout, BlockMismatch, COEFF_INTERVAL,
    HarmonicNormalization, Rotation,
};
use tutti_graph::{Buses, Cost, Shape};
use tutti_units::{ArcDegrees, Azimuth, Direction, Elevation, SampleRate, Tail};

use super::vbap::speaker_of_channel;
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// The bus of the channels of `order` in AmbiX's ordering and the scaling `normalization`.
fn bus(order: AmbisonicOrder, normalization: HarmonicNormalization) -> ChannelTopology {
    ChannelTopology::Ambisonic(AmbisonicFormat::new(
        order,
        AmbisonicOrdering::Acn,
        normalization.into(),
    ))
}

/// The controls of a placed source or a turned field: where the source is, or where the
/// front of the field goes.
const FRONT: Direction = Direction::new(Azimuth::FRONT, Elevation::LEVEL);

/// What an [`AmbisonicEncoderNode`] is made from: the ambisonic bus it writes and where the
/// source starts.
///
/// The bus is `ChannelTopology::ambisonic` of an [`AmbisonicFormat`] in ACN order and SN3D or N3D
/// scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbisonicEncoderConfig {
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    direction: Direction,
}

impl AmbisonicEncoderConfig {
    /// An encoder to the ACN bus of `order` scaled by `normalization`, with the source
    /// straight ahead.
    #[must_use]
    pub const fn new(order: AmbisonicOrder, normalization: HarmonicNormalization) -> Self {
        Self {
            order,
            normalization,
            direction: FRONT,
        }
    }

    /// The direction the source starts at.
    #[must_use]
    pub const fn direction(self, direction: Direction) -> Self {
        Self { direction, ..self }
    }
}

/// A mono source placed in an ambisonic sound field: channel `c` is the source times the
/// real spherical harmonic `c` of its direction ([`Harmonics`](tutti_dsp::Harmonics)).
///
/// The direction is two live cells, `UnitParam::Azimuth` (0 ahead, positive to the left)
/// and `UnitParam::Elevation`. A cell that moves lands at once and the gains glide to it
/// over one grid interval ([`COEFF_INTERVAL`] frames). No latency and no tail. A
/// [`KernelNode`], made from an [`AmbisonicEncoderConfig`] (`AmbisonicEncoderNode::from`).
pub type AmbisonicEncoderNode<F> = KernelNode<AmbisonicEncoderKernel<F>>;

kernel_config!(AmbisonicEncoderConfig => AmbisonicEncoderNode<F> where [F: Float],
    controls = direction,
    set(_): DirectionSet = DirectionSet);

impl<F: Float> From<AmbisonicEncoderConfig> for AmbisonicEncoderNode<F> {
    fn from(config: AmbisonicEncoderConfig) -> Self {
        let AmbisonicEncoderConfig {
            order,
            normalization,
            direction,
        } = config;
        Self::with_kernel(
            AmbisonicEncoderKernel {
                encoder: AmbisonicEncoder::new(order, normalization, direction),
                order,
                normalization,
                float: PhantomData,
            },
            config.kernel_set(),
            direction,
        )
    }
}

/// The direction's two controls: a bearing and a height.
#[derive(Clone, Copy, Debug)]
pub struct DirectionSet;

control_set! {
    DirectionSet(_s): Direction = FRONT;
    live: (all);
    Azimuth => Azimuth = azimuth { whole },
    Elevation => Elevation = elevation { whole };

    fn defaults(&self) -> Direction {
        FRONT
    }
}

/// The encoder as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct AmbisonicEncoderKernel<F: Float> {
    encoder: AmbisonicEncoder,
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    float: PhantomData<fn() -> F>,
}

impl<F: Float> Kernel for AmbisonicEncoderKernel<F> {
    type Float = F;
    type Set = DirectionSet;

    const GRID: usize = COEFF_INTERVAL;

    /// A mono bus in, the order's channels out, `Moderate`: a gain per channel and frame.
    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::main(ChannelTopology::MONO),
            Buses::main(bus(self.order, self.normalization)),
        )
        .cost(Cost::Moderate)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}

    fn reset(&mut self) {}

    fn retarget(&mut self, controls: &Direction) {
        self.encoder.retarget(*controls);
    }

    fn jump_to_target(&mut self) {
        self.encoder.jump_to_target();
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let (input, output) = block.split();
        let input: Mono<'_, F> = input.plane(0);
        self.encoder.process(input, output);
        Ok(())
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

/// What an [`AmbisonicRotatorNode`] is made from: the ambisonic bus it turns, where the
/// front starts and the roll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbisonicRotatorConfig {
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    front: Direction,
    roll: ArcDegrees,
}

impl AmbisonicRotatorConfig {
    /// A rotator of the ACN bus of `order` scaled by `normalization` that leaves the field
    /// as it is.
    #[must_use]
    pub const fn new(order: AmbisonicOrder, normalization: HarmonicNormalization) -> Self {
        Self {
            order,
            normalization,
            front: FRONT,
            roll: ArcDegrees::new(0.0),
        }
    }

    /// Where the front starts: a source straight ahead is carried there.
    #[must_use]
    pub const fn front(self, front: Direction) -> Self {
        Self { front, ..self }
    }

    /// The turn of the field about the front, fixed for the life of the node.
    #[must_use]
    pub const fn roll(self, roll: ArcDegrees) -> Self {
        Self { roll, ..self }
    }
}

/// An ambisonic sound field turned about the listener ([`Rotation`]): a source straight
/// ahead is carried to the direction of the two live cells (`UnitParam::Azimuth` and
/// `UnitParam::Elevation`), after the configured roll about the front.
///
/// A cell that moves lands at once and the matrix glides to it over one grid interval. No
/// latency and no tail. A [`KernelNode`], made from an [`AmbisonicRotatorConfig`]
/// (`AmbisonicRotatorNode::from`).
pub type AmbisonicRotatorNode<F> = KernelNode<AmbisonicRotatorKernel<F>>;

kernel_config!(AmbisonicRotatorConfig => AmbisonicRotatorNode<F> where [F: Float],
    controls = front,
    set(_): DirectionSet = DirectionSet);

impl<F: Float> From<AmbisonicRotatorConfig> for AmbisonicRotatorNode<F> {
    fn from(config: AmbisonicRotatorConfig) -> Self {
        let AmbisonicRotatorConfig {
            order,
            normalization,
            front,
            roll,
        } = config;
        Self::with_kernel(
            AmbisonicRotatorKernel {
                rotator: AmbisonicRotator::new(Rotation::new(order, front, roll)),
                order,
                normalization,
                roll,
                float: PhantomData,
            },
            config.kernel_set(),
            front,
        )
    }
}

/// The rotator as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct AmbisonicRotatorKernel<F: Float> {
    rotator: AmbisonicRotator,
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    roll: ArcDegrees,
    float: PhantomData<fn() -> F>,
}

impl<F: Float> Kernel for AmbisonicRotatorKernel<F> {
    type Float = F;
    type Set = DirectionSet;

    const GRID: usize = COEFF_INTERVAL;

    /// The order's channels in and out, `Moderate`: a block-diagonal product per frame.
    fn shape(&self) -> Shape {
        Shape::through(bus(self.order, self.normalization)).cost(Cost::Moderate)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}

    fn reset(&mut self) {}

    fn retarget(&mut self, controls: &Direction) {
        self.rotator
            .retarget(Rotation::new(self.order, *controls, self.roll));
    }

    fn jump_to_target(&mut self) {
        self.rotator.jump_to_target();
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let (input, output) = block.split();
        self.rotator.process(input, output);
        Ok(())
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

/// How an [`AmbisonicDecoderNode`] turns the sound field into speaker feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Decoding {
    /// The harmonics sampled at the speakers: for a regular array
    /// ([`AmbisonicDecoder::sampling`]).
    Sampling,
    /// All-round ambisonic decoding over the array's VBAP, for any array
    /// ([`AmbisonicDecoder::allrad`]).
    #[default]
    AllRad,
}

/// What an [`AmbisonicDecoderNode`] is made from: the ACN bus of an order and scaling it
/// reads, the bed whose speakers it feeds and the decoding rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmbisonicDecoderConfig {
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    output: BedLayout,
    decoding: Decoding,
}

impl AmbisonicDecoderConfig {
    /// A decoder to the speakers of `output`, reading first order in AmbiX (ACN, SN3D) and
    /// decoding by [`Decoding::AllRad`].
    #[must_use]
    pub const fn new(output: BedLayout) -> Self {
        Self {
            order: AmbisonicOrder::of(1),
            normalization: HarmonicNormalization::Sn3d,
            output,
            decoding: Decoding::AllRad,
        }
    }

    /// The ACN bus it reads: `order`, scaled by `normalization`.
    #[must_use]
    pub fn input(self, order: AmbisonicOrder, normalization: HarmonicNormalization) -> Self {
        Self {
            order,
            normalization,
            ..self
        }
    }

    /// The decoding rule.
    #[must_use]
    pub fn decoding(self, decoding: Decoding) -> Self {
        Self { decoding, ..self }
    }
}

/// An ambisonic signal decoded to the speakers of a [`Bed`](tutti_buffers::Bed): ambisonic channels in, the
/// bed's channels out, in the bed's order, each placed speaker at its `Bed::direction` and
/// the LFE silent.
///
/// A decoder has no live control. No latency and no tail. A [`KernelNode`], made from an
/// [`AmbisonicDecoderConfig`] (`AmbisonicDecoderNode::from`).
pub type AmbisonicDecoderNode<F> = KernelNode<AmbisonicDecoderKernel<F>>;

kernel_config!(AmbisonicDecoderConfig => AmbisonicDecoderNode<F> where [F: Float],
    set(_): AmbisonicDecoderSet = AmbisonicDecoderSet,
    fixed: ());

impl<F: Float> From<AmbisonicDecoderConfig> for AmbisonicDecoderNode<F> {
    fn from(config: AmbisonicDecoderConfig) -> Self {
        let AmbisonicDecoderConfig {
            order,
            normalization,
            output,
            decoding,
        } = config;
        let layout = output.layout();
        let decoder = match decoding {
            Decoding::Sampling => AmbisonicDecoder::sampling(order, normalization, layout),
            Decoding::AllRad => AmbisonicDecoder::allrad(order, normalization, layout),
        };
        Self::with_kernel(
            AmbisonicDecoderKernel {
                decoder,
                input: bus(order, normalization),
                output: output.bed().topology(),
                speaker_of_channel: speaker_of_channel(output.bed()),
                float: PhantomData,
            },
            AmbisonicDecoderSet,
            (),
        )
    }
}

/// A node with no control.
#[derive(Clone, Copy, Debug)]
pub struct AmbisonicDecoderSet;

impl crate::ControlSet for AmbisonicDecoderSet {
    type Controls = ();

    fn addrs(&self) -> &'static [tutti_units::UnitAddr] {
        &[]
    }

    fn units(&self) -> &'static [tutti_graph::DeclaredUnit] {
        &[]
    }

    fn defaults(&self) {}

    fn flatten(&self, _: &(), _: &mut [f64]) {}

    fn assemble(&self, _: &[f64]) {}
}

/// The decoder as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct AmbisonicDecoderKernel<F: Float> {
    decoder: AmbisonicDecoder,
    input: ChannelTopology,
    output: ChannelTopology,
    /// Which speaker feeds each output channel; `None` for the LFE.
    speaker_of_channel: Vec<Option<usize>>,
    float: PhantomData<fn() -> F>,
}

impl<F: Float> Kernel for AmbisonicDecoderKernel<F> {
    type Float = F;
    type Set = AmbisonicDecoderSet;

    const GRID: usize = COEFF_INTERVAL;

    /// The order's channels in, the bed's out, `Moderate`: a gain per channel and speaker.
    fn shape(&self) -> Shape {
        Shape::buses(Buses::main(self.input), Buses::main(self.output)).cost(Cost::Moderate)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}

    fn reset(&mut self) {}

    fn retarget(&mut self, _: &()) {}

    fn jump_to_target(&mut self) {}

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let (input, mut output) = block.split();
        for (channel, speaker) in self.speaker_of_channel.iter().enumerate() {
            let mut out = output.plane_mut(channel);
            match speaker {
                Some(s) => self.decoder.process(*s, input, out),
                None => out.samples_mut().fill(F::ZERO),
            }
        }
        Ok(())
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests;
