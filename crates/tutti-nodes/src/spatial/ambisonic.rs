//! Ambisonic nodes in the AmbiX convention (ACN order, SN3D or N3D): [`AmbisonicEncoderNode`]
//! places a mono source, [`AmbisonicRotatorNode`] turns a sound field and
//! [`AmbisonicDecoderNode`] feeds the speakers of a bed. The algorithms are
//! `tutti_dsp`'s ([`tutti_dsp::Harmonics`], [`Rotation`], [`AmbisonicDecoder`]).

use core::marker::PhantomData;

use thiserror::Error;
use tutti_buffers::{
    AmbisonicFormat, AmbisonicNormalization, AmbisonicOrder, AmbisonicOrdering, Bed, BusTopology,
    ChannelTopology, Float, InputOutput, MaxBlock, Mono,
};
use tutti_dsp::{
    AmbisonicDecoder, AmbisonicEncoder, AmbisonicRotator, BlockMismatch, COEFF_INTERVAL,
    HarmonicNormalization, Rotation, SpeakerGeometryError, SpeakerLayout,
};
use tutti_graph::{Buses, Cost, Shape};
use tutti_units::{ArcDegrees, Azimuth, Direction, Elevation, SampleRate, Tail};

use super::vbap::speaker_of_channel;
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// The bus of the channels of `order` in AmbiX's ordering and the scaling `normalization`.
fn bus(order: AmbisonicOrder, normalization: HarmonicNormalization) -> BusTopology {
    BusTopology::ambisonic(AmbisonicFormat::new(
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
/// The bus is `BusTopology::ambisonic` of an [`AmbisonicFormat`] in ACN order and SN3D or N3D
/// scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbisonicEncoderConfig {
    output: BusTopology,
    direction: Direction,
}

impl AmbisonicEncoderConfig {
    /// An encoder to `output`, with the source straight ahead.
    #[must_use]
    pub fn new(output: impl Into<BusTopology>) -> Self {
        Self {
            output: output.into(),
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
/// [`KernelNode`], made from an [`AmbisonicEncoderConfig`] (`AmbisonicEncoderNode::try_from`).
pub type AmbisonicEncoderNode<F> = KernelNode<AmbisonicEncoderKernel<F>>;

impl<F: Float> TryFrom<AmbisonicEncoderConfig> for AmbisonicEncoderNode<F> {
    type Error = AmbisonicError;

    /// # Errors
    ///
    /// [`AmbisonicError::NotAmbisonic`] for a bus that is not an ambisonic one and
    /// [`AmbisonicError::Unsupported`] for a format that is not ACN in SN3D or N3D.
    fn try_from(config: AmbisonicEncoderConfig) -> Result<Self, AmbisonicError> {
        let (order, normalization) = format_of(config.output)?;
        Ok(Self::with_kernel(
            AmbisonicEncoderKernel {
                encoder: AmbisonicEncoder::new(order, normalization, config.direction),
                order,
                normalization,
                landed: false,
                float: PhantomData,
            },
            DirectionSet,
            config.direction,
        ))
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
    /// Whether a retarget has landed since the last prepare or reset: the first lands whole.
    landed: bool,
    float: PhantomData<fn() -> F>,
}

impl<F: Float> Kernel for AmbisonicEncoderKernel<F> {
    type Float = F;
    type Set = DirectionSet;

    const GRID: usize = COEFF_INTERVAL;

    /// A mono bus in, the order's channels out, `Moderate`: a gain per channel and frame.
    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::main(BusTopology::MONO),
            Buses::main(bus(self.order, self.normalization)),
        )
        .with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {
        self.landed = false;
    }

    fn reset(&mut self) {
        self.landed = false;
    }

    fn retarget(&mut self, controls: &Direction) {
        if self.landed {
            self.encoder.retarget(*controls);
        } else {
            self.encoder.jump_to(*controls);
            self.landed = true;
        }
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
    bus: BusTopology,
    front: Direction,
    roll: ArcDegrees,
}

impl AmbisonicRotatorConfig {
    /// A rotator of the channels of `bus` that leaves the field as it is.
    #[must_use]
    pub fn new(bus: impl Into<BusTopology>) -> Self {
        Self {
            bus: bus.into(),
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
/// (`AmbisonicRotatorNode::try_from`).
pub type AmbisonicRotatorNode<F> = KernelNode<AmbisonicRotatorKernel<F>>;

impl<F: Float> TryFrom<AmbisonicRotatorConfig> for AmbisonicRotatorNode<F> {
    type Error = AmbisonicError;

    /// # Errors
    ///
    /// As the encoder: [`AmbisonicError::NotAmbisonic`] and [`AmbisonicError::Unsupported`].
    fn try_from(config: AmbisonicRotatorConfig) -> Result<Self, AmbisonicError> {
        let (order, normalization) = format_of(config.bus)?;
        Ok(Self::with_kernel(
            AmbisonicRotatorKernel {
                rotator: AmbisonicRotator::new(Rotation::new(order, config.front, config.roll)),
                order,
                normalization,
                roll: config.roll,
                landed: false,
                float: PhantomData,
            },
            DirectionSet,
            config.front,
        ))
    }
}

/// The rotator as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct AmbisonicRotatorKernel<F: Float> {
    rotator: AmbisonicRotator,
    order: AmbisonicOrder,
    normalization: HarmonicNormalization,
    roll: ArcDegrees,
    landed: bool,
    float: PhantomData<fn() -> F>,
}

impl<F: Float> Kernel for AmbisonicRotatorKernel<F> {
    type Float = F;
    type Set = DirectionSet;

    const GRID: usize = COEFF_INTERVAL;

    /// The order's channels in and out, `Moderate`: a block-diagonal product per frame.
    fn shape(&self) -> Shape {
        Shape::through(bus(self.order, self.normalization)).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {
        self.landed = false;
    }

    fn reset(&mut self) {
        self.landed = false;
    }

    fn retarget(&mut self, controls: &Direction) {
        let rotation = Rotation::new(self.order, *controls, self.roll);
        if self.landed {
            self.rotator.retarget(rotation);
        } else {
            self.rotator.jump_to(rotation);
            self.landed = true;
        }
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

/// The error returned when an ambisonic node cannot be built.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum AmbisonicError {
    /// The bus of the channels holds no ambisonic format.
    #[error("the bus is not an ambisonic one")]
    NotAmbisonic,
    /// The format is not AmbiX's channel order (ACN) in SN3D or N3D scaling, which the nodes
    /// run in; `Fold` converts the others.
    #[error("the ambisonic format {0:?} is not ACN in SN3D or N3D")]
    Unsupported(AmbisonicFormat),
    /// The bus places no bed: a decoder feeds a bed's speakers.
    #[error("the bus is not a bed, so it has no speakers to decode to")]
    NotABed,
    /// The bed's speakers cannot be decoded to: `Bed::Mono` has one and VBAP needs two.
    #[error("the bed's speakers cannot be decoded to: {0}")]
    Geometry(#[from] SpeakerGeometryError),
}

/// The order and the scaling of an ambisonic bus the nodes run in.
fn format_of(bus: BusTopology) -> Result<(AmbisonicOrder, HarmonicNormalization), AmbisonicError> {
    let format = bus.ambisonic_format().ok_or(AmbisonicError::NotAmbisonic)?;
    let normalization = match (format.ordering(), format.normalization()) {
        (AmbisonicOrdering::Acn, AmbisonicNormalization::Sn3d) => HarmonicNormalization::Sn3d,
        (AmbisonicOrdering::Acn, AmbisonicNormalization::N3d) => HarmonicNormalization::N3d,
        _ => return Err(AmbisonicError::Unsupported(format)),
    };
    Ok((format.order(), normalization))
}

/// What an [`AmbisonicDecoderNode`] is made from: the bus of speakers, the ambisonic bus it
/// reads and the decoding rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmbisonicDecoderConfig {
    input: BusTopology,
    decoding: Decoding,
    output: BusTopology,
}

impl AmbisonicDecoderConfig {
    /// A decoder to the speakers of `output`, reading first order in AmbiX and decoding by
    /// [`Decoding::AllRad`].
    #[must_use]
    pub fn new(output: impl Into<BusTopology>) -> Self {
        Self {
            input: bus(AmbisonicOrder::of(1), HarmonicNormalization::Sn3d),
            decoding: Decoding::default(),
            output: output.into(),
        }
    }

    /// The ambisonic bus it reads.
    #[must_use]
    pub fn input(self, input: impl Into<BusTopology>) -> Self {
        Self {
            input: input.into(),
            ..self
        }
    }

    /// The decoding rule.
    #[must_use]
    pub const fn decoding(self, decoding: Decoding) -> Self {
        Self { decoding, ..self }
    }
}

/// An ambisonic signal decoded to the speakers of a [`Bed`]: ambisonic channels in, the
/// bed's channels out, in the bed's order, each placed speaker at its `Bed::direction` and
/// the LFE silent.
///
/// A decoder has no live control. No latency and no tail. A [`KernelNode`], made from an
/// [`AmbisonicDecoderConfig`] (`AmbisonicDecoderNode::try_from`).
pub type AmbisonicDecoderNode<F> = KernelNode<AmbisonicDecoderKernel<F>>;

impl<F: Float> TryFrom<AmbisonicDecoderConfig> for AmbisonicDecoderNode<F> {
    type Error = AmbisonicError;

    /// # Errors
    ///
    /// [`AmbisonicError::NotAmbisonic`] and [`AmbisonicError::Unsupported`] for an input that
    /// is not ACN in SN3D or N3D, [`AmbisonicError::NotABed`] for a bus that places no bed, and
    /// [`AmbisonicError::Geometry`] for a bed whose speakers cannot be decoded to
    /// ([`Bed::Mono`]: the speakers are the layout VBAP pans over).
    fn try_from(config: AmbisonicDecoderConfig) -> Result<Self, AmbisonicError> {
        let (order, normalization) = format_of(config.input)?;
        let bed = Bed::from_topology(&ChannelTopology::from(config.output))
            .ok_or(AmbisonicError::NotABed)?;
        let layout = SpeakerLayout::try_from(bed)?;
        let decoder = match config.decoding {
            Decoding::Sampling => AmbisonicDecoder::sampling(order, normalization, &layout),
            Decoding::AllRad => AmbisonicDecoder::allrad(order, normalization, &layout),
        };
        Ok(Self::with_kernel(
            AmbisonicDecoderKernel {
                decoder,
                input: config.input,
                output: config.output,
                speaker_of_channel: speaker_of_channel(bed),
                float: PhantomData,
            },
            AmbisonicDecoderSet,
            (),
        ))
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

    fn defaults(&self) {}

    fn flatten(&self, _: &(), _: &mut [f64]) {}

    fn assemble(&self, _: &[f64]) {}
}

/// The decoder as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct AmbisonicDecoderKernel<F: Float> {
    decoder: AmbisonicDecoder,
    input: BusTopology,
    output: BusTopology,
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
        Shape::buses(Buses::main(self.input), Buses::main(self.output)).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}

    fn reset(&mut self) {}

    fn retarget(&mut self, _: &()) {}

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
