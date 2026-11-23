//! [`IrLayout`]: which impulse response a [`ConvolverNode`](super::ConvolverNode) applies to
//! which path from an input to an output.

use tutti_buffers::{AmbisonicFormat, BusTopology, ChannelLayout, ChannelTopology};

/// How a convolver's impulse responses lay over its channels.
///
/// The layout states what its responses mean; it is never guessed from a width. The
/// [`ConvolverConfig`](super::ConvolverConfig)'s bus is the channels out, and the layout says
/// what comes in and how the responses join the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrLayout {
    /// One response per channel of the bus: channel `c` is convolved with response `c`
    /// and nothing else, with no coupling between channels (dual mono at a stereo bus).
    /// One response alone is shared by every channel. A one-channel node is this layout.
    PerChannel,
    /// Every input folded to mono, then convolved with one response per output channel
    /// (one response alone is shared by every output). The inputs are the bus's own
    /// channels, folded as its placement says.
    Folded,
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

impl IrLayout {
    /// The bus of the channels in, for a node whose channels out are `output`.
    #[must_use]
    pub fn input(self, output: BusTopology) -> BusTopology {
        match self {
            Self::PerChannel | Self::Folded | Self::TrueStereo => output,
            Self::Ambisonic(format) => ChannelTopology::Ambisonic(format),
        }
    }

    /// The impulse responses the layout takes for `output` channels out: the width for
    /// the per-channel layouts (a shared one is also taken), otherwise one per
    /// (input, output) path.
    #[must_use]
    pub fn ir_count(self, output: ChannelLayout) -> usize {
        let outputs = usize::from(output.count());
        match self {
            Self::PerChannel | Self::Folded => outputs,
            Self::TrueStereo => outputs * outputs,
            Self::Ambisonic(format) => usize::from(format.width().count()) * outputs,
        }
    }
}
