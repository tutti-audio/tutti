//! What a render writes, and where each output reads from.
//!
//! One render pass feeds a list of [`Output`]s. An output is a [`Feed`] (the
//! audio it reads: the graph's outputs, or a plan tap) and a target (what
//! it becomes: a [`FileTarget`], or a [`BufferTarget`] of planes in memory). Every type has private fields, a
//! constructor and getters, and none has a default for a rate, a format or a
//! duration.

use std::path::Path;

use tutti_buffers::{ChannelLayout, ChannelSet, ChannelTopology};
use tutti_buffers::{Float, FoldError};
use tutti_codec::ToFile;
use tutti_graph::TapReader;

use crate::FileKey;
use crate::normalize::Normalize;

/// How a file output is normalized, and whose reading sets its gain.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Gain {
    /// This output is measured, and its own reading chooses the gain by the policy.
    Own(Normalize),
    /// The file output `FileKey` names is measured and this one takes its gain, so the two
    /// keep their balance (stems that still sum to the main output). The key must name
    /// another output of the same [`Outputs`](crate::Outputs) that normalizes by
    /// [`Own`](Self::Own).
    Of(FileKey),
}

/// The width of a target, stated once: a bare count of channels, or the
/// speakers they feed (whose length is the width). A target cannot hold both,
/// so a topology of another width than the target is not writable.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Width {
    Count(ChannelLayout),
    Placed(ChannelTopology),
}

impl Width {
    fn layout(&self) -> ChannelLayout {
        match self {
            Self::Count(layout) => *layout,
            Self::Placed(topology) => topology.width(),
        }
    }

    /// Whether the speakers are stated or the default layout for the width.
    pub(crate) fn placement(&self) -> tutti_buffers::Placement {
        match self {
            Self::Placed(_) => tutti_buffers::Placement::Stated,
            Self::Count(_) => tutti_buffers::Placement::Default,
        }
    }

    /// The channels the target's width stands for: the stated topology, or
    /// [`ChannelSet::default_for`] the count.
    pub(crate) fn channel_set(&self) -> ChannelSet {
        match self {
            Self::Count(layout) => ChannelSet::default_for(*layout),
            Self::Placed(topology) => ChannelSet::from(*topology),
        }
    }
}

/// A file one output is written to: how it is encoded ([`Encoding`](tutti_codec::Encoding): format, channels,
/// resample, dither seed) and where it lands (both in its [`ToFile`]), and how it is normalized. An integer
/// format's dither and noise shaping are those of its `tutti_codec::IntPcm`.
///
/// Outputs of one render are summed later (a bounce's stems and its master), so each takes its
/// own dither seed ([`Encoding::dither_seed`](tutti_codec::Encoding::dither_seed), `SplitMix64::mix` of its index): two files with
/// one noise sequence would add it coherently.
///
/// A format and width the encoder cannot take, and a topology the fold cannot
/// place the channels into, are refused when the render starts (the file's rate is not known
/// until then), before any frame is rendered and before the path is touched.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTarget {
    to: ToFile,
    pub(crate) normalize: Option<Gain>,
}

impl FileTarget {
    /// The file `to` names, with no normalize. An encoding that states no channels has the
    /// feed's.
    #[must_use]
    pub fn new(to: ToFile) -> Self {
        Self {
            to,
            normalize: None,
        }
    }

    /// Normalizes this output ([`Gain`]); without it the output is
    /// written as rendered.
    ///
    /// [`Normalize::Lufs`] weights loudness by where each channel's speaker
    /// stands, so it needs a placement: the topology of the file's channels
    /// ([`Encoding::channels`](tutti_codec::Encoding::channels), else the feed's). A width with no placement
    /// (3, 5, 7 or 9 channels, say) is refused when the render
    /// starts ([`TargetError::UnplacedLoudness`]), before any frame is
    /// rendered.
    #[must_use]
    pub fn normalize(mut self, normalize: Gain) -> Self {
        self.normalize = Some(normalize);
        self
    }

    /// Where the file lands.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.to.path()
    }

    pub(crate) fn to_file(&self) -> &ToFile {
        &self.to
    }
}

/// Planes in memory: an output rendered at the render's rate, not resampled
/// and not dithered (they are not an output a listener hears), returned as a
/// [`Rendered`](crate::Rendered).
#[derive(Debug, Clone, PartialEq)]
pub struct BufferTarget {
    pub(crate) width: Width,
}

impl BufferTarget {
    /// Planes `channels` wide.
    #[must_use]
    pub fn new(channels: ChannelLayout) -> Self {
        Self {
            width: Width::Count(channels),
        }
    }

    /// Which speakers the planes are, for the fold onto them. It replaces the
    /// width [`new`](Self::new) was given: there is one plane per speaker.
    #[must_use]
    pub fn topology(mut self, topology: ChannelTopology) -> Self {
        self.width = Width::Placed(topology);
        self
    }

    /// The channels the planes stand for.
    pub(crate) fn channel_set(&self) -> ChannelSet {
        self.width.channel_set()
    }

    /// Whether the speakers are stated or the default layout for the width.
    pub(crate) fn placement(&self) -> tutti_buffers::Placement {
        self.width.placement()
    }

    /// The planes' width.
    #[must_use]
    pub fn channels(&self) -> ChannelLayout {
        self.width.layout()
    }
}

/// What the channels of a tap are: the speakers (or unassigned feeds) its lanes
/// carry, in lane order, so the fold onto the target's layout can place them.
///
/// Built by whoever opened the tap (the session, for a stem); the pipeline
/// checks it against the tap's width and the target's layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement(ChannelSet);

impl Placement {
    /// Lanes that are the channels of `topology`.
    #[must_use]
    pub fn new(topology: ChannelTopology) -> Self {
        Self(ChannelSet::from(topology))
    }

    /// `layout` lanes in the engine's order for the width
    /// ([`ChannelSet::default_for`]); past
    /// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) they have no topology and are
    /// unassigned channels.
    #[must_use]
    pub fn of_width(layout: ChannelLayout) -> Self {
        Self(ChannelSet::default_for(layout))
    }

    /// The lanes' channels: the topology, or the engine's order for the width.
    pub(crate) fn channel_set(&self) -> ChannelSet {
        self.0
    }

    /// How many lanes.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.0.width()
    }

    /// What the lanes are, when the width has a topology.
    #[must_use]
    pub fn topology(&self) -> Option<&ChannelTopology> {
        self.0.topology()
    }
}

/// The audio an output reads.
#[derive(Debug)]
#[non_exhaustive]
pub enum Feed<F: Float = f32> {
    /// The graph's outputs (the declared main output, or the engine's order for the
    /// width), folded onto the target's layout.
    MainOutput,
    /// A plan tap ([`Editor::open_tap`](tutti_graph::Editor::open_tap)), read
    /// after every block: its lanes are `lanes`, folded onto the target's
    /// layout.
    ///
    /// Open the tap with [`TapAlign::ToOutput`](tutti_graph::TapAlign::ToOutput)
    /// so it is on the outputs' time (one latency trim serves every output),
    /// give each lane `2 x max_block` frames of ring, and commit before the
    /// render. A lane count that is not `lanes`' width is
    /// [`TargetError::Lanes`]; frames the ring lost are
    /// [`RenderError::TapOverrun`](crate::RenderError::TapOverrun).
    Tap {
        /// The tap's reader: the render is its one consumer.
        reader: TapReader<F>,
        /// What its lanes are.
        lanes: Placement,
    },
}

/// One thing a render writes: the [`Feed`] it reads and the target it
/// becomes, a [`FileTarget`] or a [`BufferTarget`]. It is added to
/// [`Outputs`](crate::Outputs) by its kind, which hands back the key its result
/// is read by.
#[derive(Debug)]
pub struct Output<T, F: Float = f32> {
    feed: Feed<F>,
    target: T,
}

impl<T, F: Float> Output<T, F> {
    /// The graph's outputs, into `target`.
    #[must_use]
    pub fn main_output(target: T) -> Self {
        Self {
            feed: Feed::MainOutput,
            target,
        }
    }

    /// What `reader` taps, placed as `lanes`, into `target`.
    #[must_use]
    pub fn tap(reader: TapReader<F>, lanes: Placement, target: T) -> Self {
        Self {
            feed: Feed::Tap { reader, lanes },
            target,
        }
    }

    /// What this output reads.
    #[must_use]
    pub fn feed(&self) -> &Feed<F> {
        &self.feed
    }

    /// What this output becomes.
    #[must_use]
    pub fn target(&self) -> &T {
        &self.target
    }

    pub(crate) fn into_parts(self) -> (Feed<F>, T) {
        (self.feed, self.target)
    }
}

/// Why an [`Output`] cannot be rendered ([`RenderError::Target`](crate::RenderError::Target)).
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TargetError {
    /// The channels (the graph's, or a tap's lanes) have no place in the
    /// target's layout: the topology it states cannot take them.
    #[error("the channels cannot be placed into the target's: {0}")]
    Placement(FoldError),
    /// The tap has a lane count other than its [`Placement`]'s width.
    #[error("the tap carries {lanes:?} but its placement names {placement:?}")]
    Lanes {
        /// The tap's width.
        lanes: ChannelLayout,
        /// The placement's width.
        placement: ChannelLayout,
    },
    /// A [`Gain::Of`] that names an output that does not measure itself.
    #[error(
        "the gain is to come from output {index}, which is not another output that measures itself"
    )]
    GainSource {
        /// The position of the output it names.
        index: usize,
    },
    /// A [`Gain::Of`] key that another [`Outputs`](crate::Outputs) made.
    #[error("the gain is to come from a key of another Outputs")]
    ForeignGain,
    /// A [`Normalize::Lufs`] output whose channels name no speakers: loudness
    /// is weighted by each speaker's placement, and a file with no stated
    /// topology is taken to be [`ChannelTopology::default_for`] for its width, which
    /// has a placement at 1, 2, 4, 6, 8 and 12 channels only. State a topology
    /// ([`Encoding::channels`](tutti_codec::Encoding::channels)) that is a known bed, or normalize by
    /// [`Normalize::Peak`].
    #[error(
        "a Lufs output of {channels:?} has no speaker placement to weigh loudness by: \
         state a known topology with `topology`, or normalize by peak"
    )]
    UnplacedLoudness {
        /// The output's width.
        channels: ChannelLayout,
    },
    /// Another output writes the same path.
    #[error("output {other} writes the same path")]
    SamePath {
        /// The other output.
        other: usize,
    },
}
