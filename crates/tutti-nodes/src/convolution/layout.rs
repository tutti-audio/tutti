//! [`IrLayout`]: which impulse response a [`ConvolverNode`](super::ConvolverNode) applies to
//! which path from an input to an output.

use tutti_buffers::{AmbisonicFormat, Bed, ChannelLayout, ChannelTopology};

/// How a convolver's impulse responses lay over its channels.
///
/// The layout states what its responses mean; it is never guessed from a width, and a
/// node checks it against both its ports when it is built
/// ([`ConvolverNode::from_layout`](super::ConvolverNode::from_layout)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IrLayout {
    /// One response per channel of the topology: channel `c` is convolved with response
    /// `c` and nothing else, with no coupling between channels (dual mono at a stereo
    /// topology). A one-channel node is this layout.
    PerChannel(ChannelTopology),
    /// A stereo pair with a response for each of the four paths, `[LL, LR, RL, RR]`
    /// (`XY` is input `X` to output `Y`): `out_L = in_L * LL + in_R * RL` and
    /// `out_R = in_L * LR + in_R * RR`.
    TrueStereo,
    /// An ambisonic set in and a topology out (speakers, or another ambisonic set), with a
    /// response for each of the `input width * output width` paths, input-major: the path
    /// from input channel `j` to output channel `i` is response `j * outputs + i`.
    Ambisonic {
        /// The format of the channels in.
        input: AmbisonicFormat,
        /// The channels out.
        output: ChannelTopology,
    },
}

impl IrLayout {
    /// The topology of the channels in.
    #[must_use]
    pub fn input(&self) -> ChannelTopology {
        match self {
            Self::PerChannel(topology) => topology.clone(),
            Self::TrueStereo => Bed::Stereo.topology(),
            Self::Ambisonic { input, .. } => ChannelTopology::from(*input),
        }
    }

    /// The topology of the channels out.
    #[must_use]
    pub fn output(&self) -> ChannelTopology {
        match self {
            Self::PerChannel(topology) => topology.clone(),
            Self::TrueStereo => Bed::Stereo.topology(),
            Self::Ambisonic { output, .. } => output.clone(),
        }
    }

    /// The channels in.
    #[must_use]
    pub fn inputs(&self) -> ChannelLayout {
        self.input().width()
    }

    /// The channels out.
    #[must_use]
    pub fn outputs(&self) -> ChannelLayout {
        self.output().width()
    }

    /// The impulse responses the layout takes: the width for per-channel, otherwise one per
    /// (input, output) path.
    #[must_use]
    pub fn ir_count(&self) -> usize {
        let (i, o) = (
            usize::from(self.inputs().count()),
            usize::from(self.outputs().count()),
        );
        match self {
            Self::PerChannel(_) => i,
            Self::TrueStereo | Self::Ambisonic { .. } => i * o,
        }
    }
}
