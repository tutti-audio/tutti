//! What a render writes, and where each output reads from.
//!
//! One render pass feeds a list of [`Output`]s. An output is a [`Feed`] (the
//! audio it reads: the graph's outputs, or a plan tap) and a [`Target`] (what
//! it becomes: a file, or planes in memory). Every type has private fields, a
//! constructor and getters, and none has a default for a rate, a format or a
//! duration.

use std::path::{Path, PathBuf};

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Float, FoldError};
use tutti_codec::Format;
use tutti_dsp::DitherShape;
use tutti_graph::TapReader;

use crate::Resample;
use crate::normalize::Normalize;
use crate::{Rendered, Written};

/// Where the gain of a normalizing output comes from ([`NormalizeSpec`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GainSource {
    /// This output is measured, and its own reading chooses the gain.
    Own,
    /// The output at this index (in the list given to [`render`](crate::render()))
    /// is measured and this one takes its gain, so the two keep their
    /// balance (stems that still sum to the master). The index must name
    /// another output that normalizes by [`Own`](Self::Own).
    FromOutput(usize),
}

/// How a file output is normalized: a policy, and whose reading it follows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizeSpec {
    mode: Normalize,
    gain: GainSource,
}

impl NormalizeSpec {
    /// Measures this output and normalizes it by `mode`.
    #[must_use]
    pub fn own(mode: Normalize) -> Self {
        Self {
            mode,
            gain: GainSource::Own,
        }
    }

    /// Applies the gain measured on the output at `index`.
    ///
    /// `mode` is not consulted for the gain (the measured output's is); it is
    /// the policy this output would have been given on its own, and is kept so
    /// the spec states what the file was asked to be.
    #[must_use]
    pub fn from_output(mode: Normalize, index: usize) -> Self {
        Self {
            mode,
            gain: GainSource::FromOutput(index),
        }
    }

    /// The normalize policy.
    #[must_use]
    pub(crate) fn mode(&self) -> Normalize {
        self.mode
    }

    /// Whose reading the gain comes from.
    #[must_use]
    pub(crate) fn gain(&self) -> GainSource {
        self.gain
    }
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

    pub(crate) fn topology(&self) -> Option<&ChannelTopology> {
        match self {
            Self::Count(_) => None,
            Self::Placed(topology) => Some(topology),
        }
    }
}

/// A file one output is written to: where, in what format and width, and how
/// it is converted on the way (rate, dither, normalize).
///
/// A format and width the encoder cannot take, and a topology the fold cannot
/// place the channels into, are refused when the render starts (the file's rate is not known
/// until then), before any frame is rendered and before the path is touched.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTarget {
    path: PathBuf,
    format: Format,
    pub(crate) width: Width,
    pub(crate) resample: Option<Resample>,
    pub(crate) dither: Option<DitherShape>,
    pub(crate) normalize: Option<NormalizeSpec>,
    create_new: bool,
}

impl FileTarget {
    /// A file at `path` of `format`, `channels` wide, at the render's rate,
    /// with triangular dither (it applies only to an integer width) and no
    /// normalize.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, format: Format, channels: ChannelLayout) -> Self {
        Self {
            path: path.into(),
            format,
            width: Width::Count(channels),
            resample: None,
            dither: Some(DitherShape::Triangular),
            normalize: None,
            create_new: false,
        }
    }

    /// Which speaker each file channel feeds: for the WAV speaker mask
    /// and the fold onto the file. It replaces the
    /// width [`new`](Self::new) was given: the file is as wide as the
    /// topology, and without one it has the engine's order for the width
    /// ([`Written::placement`] reports which).
    #[must_use]
    pub fn topology(mut self, topology: ChannelTopology) -> Self {
        self.width = Width::Placed(topology);
        self
    }

    /// Writes at another rate than the render's.
    #[must_use]
    pub fn resample(mut self, resample: Resample) -> Self {
        self.resample = Some(resample);
        self
    }

    /// The noise added when an integer width is quantized: triangular unless
    /// this or [`no_dither`](Self::no_dither) says otherwise. A format with no
    /// integer width (`Pcm::Float32`) ignores it.
    #[must_use]
    pub fn dither(mut self, shape: DitherShape) -> Self {
        self.dither = Some(shape);
        self
    }

    /// No dither when an integer width is quantized: the rounding error is
    /// left as it falls. A format with no integer width ignores it.
    #[must_use]
    pub fn no_dither(mut self) -> Self {
        self.dither = None;
        self
    }

    /// Normalizes this output ([`NormalizeSpec`]); without it the output is
    /// written as rendered.
    ///
    /// [`Normalize::Lufs`] weights loudness by where each channel's speaker
    /// stands, so it needs a placement: the topology stated by
    /// [`topology`](Self::topology), or, for a file that states none,
    /// [`ChannelTopology::assumed`] for its width, which
    /// [`Written::placement`] reports as `Assumed`. A width with no assumed
    /// placement (3, 5, 7 or 9 channels, say) is refused when the render
    /// starts ([`TargetError::UnplacedLoudness`]), before any frame is
    /// rendered.
    #[must_use]
    pub fn normalize(mut self, normalize: NormalizeSpec) -> Self {
        self.normalize = Some(normalize);
        self
    }

    /// Refuses a path that already exists instead of replacing it. The file is
    /// still written beside the path; the refusal is the commit's, which fails
    /// with a [`tutti_codec::Error::Commit`] of kind `AlreadyExists`, leaves
    /// what is at the path alone and keeps the complete file at the error's
    /// `kept` path for the caller.
    #[must_use]
    pub fn create_new(mut self) -> Self {
        self.create_new = true;
        self
    }

    /// Whether an existing path is refused ([`create_new`](Self::create_new)).
    #[must_use]
    pub(crate) fn refuses_existing(&self) -> bool {
        self.create_new
    }

    /// Where the file lands.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The container and its settings.
    #[must_use]
    pub(crate) fn format(&self) -> Format {
        self.format
    }

    /// The file's width.
    #[must_use]
    pub(crate) fn channels(&self) -> ChannelLayout {
        self.width.layout()
    }

    /// Whether the file's speakers are stated ([`topology`](Self::topology))
    /// or assumed from its width ([`ChannelTopology::assumed`]).
    #[must_use]
    pub(crate) fn placement(&self) -> tutti_buffers::Placement {
        match self.width {
            Width::Placed(_) => tutti_buffers::Placement::Stated,
            Width::Count(_) => tutti_buffers::Placement::Assumed,
        }
    }

    /// The speakers the file's channels feed: the stated topology, or
    /// [`ChannelTopology::assumed`] for its width ([`placement`](Self::placement)
    /// says which).
    pub(crate) fn effective_topology(&self) -> ChannelTopology {
        match &self.width {
            Width::Placed(topology) => topology.clone(),
            Width::Count(layout) => ChannelTopology::assumed(*layout),
        }
    }
}

/// Planes in memory: an output rendered at the render's rate, not resampled
/// and not dithered (they are not an output a listener hears), returned as a
/// [`Rendered`].
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

    /// The planes' width.
    #[must_use]
    pub fn channels(&self) -> ChannelLayout {
        self.width.layout()
    }
}

/// What an output becomes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Target {
    /// A file.
    File(FileTarget),
    /// Planes in memory.
    Buffers(BufferTarget),
}

impl From<FileTarget> for Target {
    fn from(target: FileTarget) -> Self {
        Self::File(target)
    }
}

impl From<BufferTarget> for Target {
    fn from(target: BufferTarget) -> Self {
        Self::Buffers(target)
    }
}

/// What the channels of a tap are: the speakers (or unassigned feeds) its lanes
/// carry, in lane order, so the fold onto the target's layout can place them.
///
/// Built by whoever opened the tap (the session, for a stem); the pipeline
/// checks it against the tap's width and the target's layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    topology: ChannelTopology,
}

impl Placement {
    /// Lanes that are the channels of `topology`.
    #[must_use]
    pub fn new(topology: ChannelTopology) -> Self {
        Self { topology }
    }

    /// What the lanes are.
    #[must_use]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }
}

/// The audio an output reads.
#[derive(Debug)]
#[non_exhaustive]
pub enum Feed<F: Float = f32> {
    /// The graph's outputs (the declared root, or the engine's order for the
    /// width), folded onto the target's layout.
    Master,
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

/// One thing a render writes: the [`Feed`] it reads and the [`Target`] it
/// becomes.
#[derive(Debug)]
pub struct Output<F: Float = f32> {
    feed: Feed<F>,
    target: Target,
}

impl<F: Float> Output<F> {
    /// The graph's outputs, into `target`.
    #[must_use]
    pub fn master(target: impl Into<Target>) -> Self {
        Self {
            feed: Feed::Master,
            target: target.into(),
        }
    }

    /// What `reader` taps, placed as `lanes`, into `target`.
    #[must_use]
    pub fn tap(reader: TapReader<F>, lanes: Placement, target: impl Into<Target>) -> Self {
        Self {
            feed: Feed::Tap { reader, lanes },
            target: target.into(),
        }
    }

    /// What this output reads.
    #[must_use]
    pub fn feed(&self) -> &Feed<F> {
        &self.feed
    }

    /// What this output becomes.
    #[must_use]
    pub fn target(&self) -> &Target {
        &self.target
    }

    pub(crate) fn into_parts(self) -> (Feed<F>, Target) {
        (self.feed, self.target)
    }
}

/// What one [`Output`] produced, in the order the outputs were given.
#[derive(Debug)]
#[non_exhaustive]
pub enum Produced<F: Float = f32> {
    /// From a [`Target::File`].
    File(Written),
    /// From a [`Target::Buffers`].
    Buffers(Rendered<F>),
}

impl<F: Float> Produced<F> {
    /// The file, or `None` for planes.
    ///
    /// ```
    /// # use tutti_export::{Produced, Rendered};
    /// # use tutti_units::SampleRate;
    /// let all = vec![Produced::Buffers(
    ///     Rendered::new(vec![vec![0.0; 4]], SampleRate::SR_48K, 0)?,
    /// )];
    /// // Borrowed, so an output in a list is read in place.
    /// assert!(all[0].file().is_none());
    /// assert_eq!(all[0].buffers().map(Rendered::channels), Some(1));
    /// # Ok::<(), tutti_export::RenderedError>(())
    /// ```
    #[must_use]
    pub fn file(&self) -> Option<&Written> {
        match self {
            Self::File(written) => Some(written),
            Self::Buffers(_) => None,
        }
    }

    /// The planes, or `None` for a file.
    #[must_use]
    pub fn buffers(&self) -> Option<&Rendered<F>> {
        match self {
            Self::Buffers(rendered) => Some(rendered),
            Self::File(_) => None,
        }
    }

    /// The file, owned, or `None` for planes.
    #[must_use]
    pub fn into_file(self) -> Option<Written> {
        match self {
            Self::File(written) => Some(written),
            Self::Buffers(_) => None,
        }
    }

    /// The planes, owned, or `None` for a file.
    #[must_use]
    pub fn into_buffers(self) -> Option<Rendered<F>> {
        match self {
            Self::Buffers(rendered) => Some(rendered),
            Self::File(_) => None,
        }
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
    /// A [`GainSource::FromOutput`] that names this output, an output past
    /// the list, or one that does not measure itself.
    #[error(
        "the gain is to come from output {index}, which is not another output that measures itself"
    )]
    GainSource {
        /// The index it names.
        index: usize,
    },
    /// [`write_buffers`](crate::write_buffers) was given a target that asks to
    /// normalize: planes are written as they are, so the gain is applied first
    /// ([`Rendered::apply_gain`](crate::Rendered::apply_gain)).
    #[error("planes are written as they are: apply the gain first, and ask no normalize")]
    NormalizeOfPlanes,
    /// A [`Normalize::Lufs`] output whose channels name no speakers: loudness
    /// is weighted by each speaker's placement, and a file with no stated
    /// topology is taken to be [`ChannelTopology::assumed`] for its width, which
    /// has a placement at 1, 2, 4, 6, 8 and 12 channels only. State a topology
    /// ([`FileTarget::topology`]) that is a known bed, or normalize by
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
