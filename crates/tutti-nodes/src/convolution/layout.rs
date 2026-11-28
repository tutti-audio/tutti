//! [`IrLayout`] and [`MatrixLayout`]: which impulse response a
//! [`ConvolverNode`](super::ConvolverNode) applies to which path from an input to an output.

use tutti_buffers::{AmbisonicFormat, ChannelTopology};

/// How a convolver's impulse responses lay over the channels of its bus, one response per
/// channel or per bus.
///
/// The layout states what its responses mean; it is never guessed from a width. The
/// [`ConvolverConfig`](super::ConvolverConfig)'s bus is the channels in and out. A layout
/// with a response for every (input, output) path is a [`MatrixLayout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IrLayout {
    /// One response per channel of the bus: channel `c` is convolved with response `c`
    /// and nothing else, with no coupling between channels (dual mono at a stereo bus).
    /// One response alone is shared by every channel. A one-channel node is this layout.
    PerChannel,
    /// Every input folded to mono, then convolved with one response per output channel
    /// (one response alone is shared by every output). The inputs are the bus's own
    /// channels, folded as its placement says.
    Folded,
}

/// How the impulse responses of a matrix convolver lay over its paths: one response for each
/// (input, output) path, input-major, in a [`MatrixResponses`](tutti_dsp::MatrixResponses)
/// the caller owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixLayout {
    /// A stereo pair with a response for each of the four paths, `[LL, LR, RL, RR]`
    /// (`XY` is input `X` to output `Y`): `out_L = in_L * LL + in_R * RL` and
    /// `out_R = in_L * LR + in_R * RR`. The bus is the pair, in and out, so it is two
    /// channels wide.
    TrueStereo,
    /// An ambisonic set of this format in, the bus out (speakers, or another ambisonic
    /// set), with a response for each of the `input width * output width` paths,
    /// input-major: the path from input channel `j` to output channel `i` is response
    /// `j * outputs + i`.
    Ambisonic(AmbisonicFormat),
}

impl MatrixLayout {
    /// The bus of the channels in, for a node whose channels out are `output`.
    #[must_use]
    pub fn input(self, output: ChannelTopology) -> ChannelTopology {
        match self {
            Self::TrueStereo => output,
            Self::Ambisonic(format) => ChannelTopology::Ambisonic(format),
        }
    }
}
