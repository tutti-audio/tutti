//! [`Destination`]: where a bounce's output goes, and what the finished bounce hands back for
//! it.
//!
//! A destination is a request: the codec's [`ToFile`](tutti_codec::ToFile) pairs an
//! [`Encoding`](tutti_codec::Encoding) (format, channels, rate, dither) with a path, and
//! [`ToMemory`] asks for planes. The bounce's main output and each of its stems take one, and
//! the type says what comes back ([`Destination::Output`]): a [`Written`] for a file and a
//! [`Rendered`] for planes, with no run-time match on the kind.

use std::fmt::Debug;

use tutti_buffers::{ChannelLayout, ChannelSet, Float};
use tutti_export::{BufferTarget, Rendered, Written};

/// Where a bounce's output goes: a [`tutti_codec::ToFile`] or a [`ToMemory`] (sealed). It
/// names what the finished bounce holds for that output, so the caller who chose a file is
/// handed a [`Written`] and one who chose memory a [`Rendered`].
///
/// A file is written beside its path and put in place when the bounce finishes, so a
/// cancelled or failed bounce leaves what was at the path as it was, and nothing beside it.
/// The encoding's channels are the width of the file, and without them the output's own (a
/// main output's, or the width of the tapped channels of a stem). The dither seed of an
/// encoding that states none is mixed from the output's index, so files that are summed later
/// do not share one noise sequence; a seed stated with
/// [`Encoding::dither_seed`](tutti_codec::Encoding::dither_seed) is used as given.
///
/// ```no_run
/// # use tutti_session::Bounce;
/// # use tutti_codec::{Encoding, Format, IntDepth, IntPcm, Pcm, ToFile};
/// # use tutti_buffers::ChannelLayout;
/// let wav = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24)));
/// let _ = Bounce::<tutti_graph::NodeKey>::new().main_output(
///     ToFile::new("out/mix.wav", Encoding::new(wav).width(ChannelLayout::STEREO)).replace(),
/// );
/// ```
pub trait Destination: sealed::Sealed {
    /// What the output is once rendered, at the graph's float.
    type Output<F: Float>: Debug + Send;
}

mod sealed {
    /// Which types are destinations; what one does inside the render is the bounce's.
    pub trait Sealed: Send + 'static {}
}

impl sealed::Sealed for tutti_codec::ToFile {}

impl Destination for tutti_codec::ToFile {
    type Output<F: Float> = Written;
}

/// Planes in memory: [`Bounced::main_output`](crate::Bounced::main_output) is their
/// [`Rendered`], one plane per channel at the render's rate. Nothing is encoded, resampled,
/// dithered or normalized, and a bounce into memory has no way to ask for any of them.
///
/// A bounce knows its span, so what it holds is bounded by its frame limit
/// ([`tutti_export::MAX_FRAMES`]): the cost is the frames, times the channels, times the
/// size of the graph's float, and a span too long for that is refused before anything is
/// rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToMemory {
    channels: Option<ChannelSet>,
}

impl ToMemory {
    /// Planes as wide as the output they hold.
    pub fn new() -> Self {
        Self::default()
    }

    /// The planes' channels: their width and the speakers they are, folded onto from the
    /// output's. A narrowing the fold cannot place is refused when the render starts, never
    /// truncated.
    pub fn channels(mut self, channels: ChannelSet) -> Self {
        self.channels = Some(channels);
        self
    }

    /// The planes of an output `width` channels wide unless channels were stated.
    pub(crate) fn planes(self, width: ChannelLayout) -> BufferTarget {
        match self.channels {
            None => BufferTarget::new(width),
            Some(set) => {
                let planes = BufferTarget::new(set.width());
                match set.topology() {
                    Some(topology) => planes.topology(*topology),
                    None => planes,
                }
            }
        }
    }
}

impl sealed::Sealed for ToMemory {}

impl Destination for ToMemory {
    type Output<F: Float> = Rendered<F>;
}
