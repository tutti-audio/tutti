//! What an export can fail with.
//!
//! One `#[non_exhaustive]` [`RenderError`](enum@RenderError) for the whole crate — every stage (plan,
//! render, resample, dither, encode) reports through it, so a caller matches one
//! type whichever entry point it used.

use std::io;

use tutti_buffers::{ChannelLayout, FoldError};

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
    /// A [`SinkTarget`](crate::SinkTarget) whose sink is not as wide as the audio the output
    /// reads. A sink takes the output's width unconverted, so it is refused, never folded.
    #[error("the sink is {sink:?} wide and the audio it is fed {feed:?}")]
    SinkWidth {
        /// The sink's width.
        sink: ChannelLayout,
        /// The width of the audio the output reads.
        feed: ChannelLayout,
    },
    /// Another output writes the same path.
    #[error("output {other} writes the same path")]
    SamePath {
        /// The other output.
        other: usize,
    },
}

/// Why a [`Done`] has no result for a key.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DoneError {
    /// Another [`Outputs`] made the key.
    #[error("the key belongs to another Outputs")]
    Foreign,
    /// The result was moved out by [`Done::take_file`] or [`Done::take_buffers`].
    #[error("the result was already taken")]
    Taken,
}

/// Anything that can go wrong rendering or writing an export.
///
/// `#[non_exhaustive]`: the codecs behind the `encode-*` features each bring their
/// own failure modes, so a caller must keep a wildcard arm.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum RenderError {
    /// The sink failed: the output file could not be created, written (a full
    /// disk) or moved into place.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A config field the encoder cannot act on. Caught before any frame is
    /// rendered.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(#[from] ConfigError),

    /// The encoder refused the spec or a block: a spec the format cannot be
    /// written for, a WAV file past its size limit, or a codec failure. The
    /// file is removed. (The sink failing is [`Io`](Self::Io).)
    #[error(transparent)]
    Encode(tutti_codec::EncodeError),

    /// A buffer that is not whole frames of its width.
    #[error(transparent)]
    Buffer(#[from] tutti_buffers::BufferError),

    /// The resampler could not be constructed for this rate pair, or rejected a
    /// chunk.
    #[error("Resampling error: {0}")]
    Resample(#[from] tutti_dsp::ResampleError),

    /// A width the render cannot use, which means zero and nothing else.
    ///
    /// The frame width is a runtime `ChannelLayout`, so any positive count
    /// exports — including the 3- and 5-wide main outputs a fixed enumeration of
    /// widths could not express.
    #[error("Unsupported channel count: 0 (a render needs at least one channel)")]
    UnsupportedChannels,

    /// A topology of another width than the rendered audio: the one
    /// [`Normalize::gain_for_wave`](crate::Normalize::gain_for_wave)
    /// was given, or the one [`Setup::main_output`](crate::Setup::main_output)
    /// places the file from.
    #[error("the topology is {topology:?} wide and the rendered audio {rendered:?}")]
    TopologyWidth {
        /// The topology's width.
        topology: tutti_buffers::ChannelLayout,
        /// The rendered audio's width.
        rendered: tutti_buffers::ChannelLayout,
    },

    /// A loudness measurement was asked for and the audio cannot be
    /// normalized by it (`reason` says why): the meter rejects it, or the
    /// reading it gives is not a measurement of the programme.
    ///
    /// An error rather than a skipped or invented gain, because a normalized
    /// export that quietly writes un-normalized audio (or applies +47 dB to a
    /// hiss at the gate's floor) reports success and leaves no trace: nothing
    /// in [`Written`](crate::Written) records that the gain the caller asked
    /// for was never applied, or what it was. A file export leaves the target
    /// as it was.
    #[error("Cannot normalize: {channels:?} at {sample_rate:?} is unmeasurable: {reason}")]
    Unmeasurable {
        /// The output whose audio it is, by its index in the list given to
        /// [`render`](crate::render()); `None` for planes measured on their own
        /// ([`Normalize::gain_for_wave`](crate::Normalize::gain_for_wave)),
        /// which belong to no output.
        output: Option<usize>,
        /// The channel layout of the audio.
        channels: ChannelLayout,
        /// Its sample rate.
        sample_rate: tutti_units::SampleRate,
        /// Why the reading is not usable.
        reason: Unmeasured,
    },

    /// The graph could not be forked for the render because the node at
    /// `key` cannot be: it handed the editor no fork source (a mic monitor, a
    /// node inserted `Unforkable`), so a copy would share the live node. From the session's fork of a live graph;
    /// nothing was rendered.
    ///
    /// A variant of its own rather than inside [`Fork`](Self::Fork) because it
    /// is the one a host acts on — "remove or freeze this node" — and the key
    /// is what it acts on.
    #[error(
        "Cannot export: node {key:?} cannot be forked for an offline render (a mic monitor, or a node built unforkable)"
    )]
    NotForkable {
        /// The node, as the live graph keys it.
        key: tutti_graph::NodeKey,
    },

    /// The graph could not be forked for the render for any reason other
    /// than [`NotForkable`](Self::NotForkable): the target node is missing or
    /// has no outputs, or the forked graph did not commit.
    #[error("Cannot fork the graph for export: {0}")]
    Fork(tutti_graph::ForkError),

    /// The graph dropped events during the render (a port past its capacity,
    /// a full delay): the audio of a dropped note-on or note-off is wrong, and
    /// a note whose note-off was dropped may sound to the end. Raised under
    /// [`EventLossPolicy::Fail`](crate::EventLossPolicy::Fail), the default;
    /// a file output is not put in place. The render stops at the first block
    /// that lost an event, so `count` is the executor's `lost_event_count`
    /// from when the render began (or, for a graph that had a commit
    /// installed since its last render, from that render's end) to the end of
    /// that block: events that never arrived, not a note-off that was
    /// refused and sent again.
    #[error("Export failed: the graph dropped {count} events during the render")]
    EventsDropped {
        /// How many.
        count: u64,
    },

    /// The render would hold its planes in memory (a [`BufferTarget`](crate::BufferTarget)) and
    /// the allocation for `frames` frames of `channels` channels failed or
    /// overflowed. Nothing was rendered. A file export streams and never asks
    /// for this much.
    #[error("Cannot hold the render in memory: {frames:?} frames of {channels:?} channels")]
    RenderTooLarge {
        /// Frames per plane the render needed.
        frames: tutti_units::Samples,
        /// The planes it needed them for.
        channels: ChannelLayout,
    },

    /// An output cannot be rendered as it was given ([`TargetError`] says why);
    /// `output` is its index in the list given to [`render`](crate::render()).
    /// Nothing was rendered.
    #[error("output {output} cannot be rendered: {why}")]
    Target {
        /// The output's index.
        output: usize,
        /// What is wrong with it.
        why: TargetError,
    },

    /// A caller's sink ([`SinkTarget`](crate::SinkTarget)) failed on a write or on closing,
    /// and `source` is its own error, type-erased. No file of the render is put in place;
    /// what the sink already received stays with it.
    #[error("output {output}'s sink failed: {source}")]
    Sink {
        /// The output's index.
        output: usize,
        /// The sink's error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A render was given no outputs.
    #[error("a render needs at least one output")]
    NoOutputs,

    /// A tap's ring lost frames: the render did not drain it as fast as the
    /// graph filled it, and what was written would have a hole. With rings of
    /// `2 x max_block` frames and one drain after each block it cannot happen;
    /// this is the guard, not a mode.
    #[error("output {output}'s tap lost {lost:?} frames: its ring was too small")]
    TapOverrun {
        /// The output's index.
        output: usize,
        /// The frames lost.
        lost: tutti_units::Samples,
    },

    /// A tap had fewer frames than the block rendered: it was never committed
    /// to the graph being rendered, or its source is not in it.
    #[error("output {output}'s tap held less than the block rendered; was it committed?")]
    TapStarved {
        /// The output's index.
        output: usize,
    },

    /// A rename failed after earlier files were put in place: the files that
    /// landed (`landed`) are kept, nothing is rolled back, and no output that
    /// did not land is left behind. The failure of the first rename is the
    /// plain error: nothing had landed then.
    #[error(
        "{} file(s) were put in place before a rename failed: {cause}",
        .landed.len()
    )]
    NotCommitted {
        /// The files that landed, in the order they were given.
        landed: Vec<std::path::PathBuf>,
        /// The rename's failure.
        cause: tutti_codec::EncodeError,
    },

    /// A result was read from the finished render by a key it has none for
    /// ([`DoneError`](crate::DoneError)): for a caller that reads results on its own caller's behalf.
    #[error("Export result unavailable: {0}")]
    Key(#[from] crate::DoneError),

    /// A forked unit failed while the render ran: a hosted plugin's
    /// server crashed, or stopped answering (`kind`). Past that point the
    /// render holds silence where the unit's output belongs, so the export is
    /// reported as failed rather than written as if it had succeeded. The
    /// editor's `fork_health` is checked after every graph render
    /// (see "When a forked unit fails" in tutti-graph's `fork` docs), and for a
    /// file export before the file is put in place: the file at the path keeps
    /// what it held.
    #[error("Export failed: forked node {key:?} {kind:?} during the render: {cause}")]
    ForkFailed {
        /// The node, as the live graph keys it.
        key: tutti_graph::NodeKey,
        /// Crashed or timed out.
        kind: tutti_graph::ForkFaultKind,
        /// The unit's own account (for a plugin, a
        /// `tutti_plugin_ipc::PluginRenderFault`).
        cause: tutti_graph::ForkCause,
    },
}

/// Why audio cannot be normalized ([`RenderError::Unmeasurable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Unmeasured {
    /// The meter does not take this width or rate: EBU R128 accepts 1 to 64
    /// channels at 16 Hz to 2.8 MHz.
    Format,
    /// [`Normalize::Lufs`](crate::Normalize::Lufs) cannot weigh the channels:
    /// loudness is weighted by each speaker's angles (BS.1770-5 Table 4),
    /// which an ambisonic set or speakers that are not a known bed do not have.
    /// A file output is refused before the render
    /// ([`TargetError::UnplacedLoudness`](crate::TargetError));
    /// this is what [`Normalize::gain_for_wave`](crate::Normalize::gain_for_wave)
    /// reports for a topology it was given.
    Placement,
    /// [`Normalize::Lufs`](crate::Normalize::Lufs): no part of the audio
    /// passed the integrated loudness gate. It is silent, quieter than the
    /// absolute gate (-70 LUFS), or shorter than the 400 ms block the gate is
    /// judged in; any gain chosen from that reading would be made up.
    BelowGate,
    /// [`Normalize::Peak`](crate::Normalize::Peak): the true peak is at the
    /// floor ([`Db::FLOOR`](tutti_units::Db::FLOOR)), which is silence.
    Silent,
}

impl std::fmt::Display for Unmeasured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Format => "the meter does not take this width or rate",
            Self::Placement => "the channels' speaker placement is unknown or ambisonic",
            Self::BelowGate => {
                "nothing passed the loudness gate (silent, under -70 LUFS, or under 400 ms)"
            }
            Self::Silent => "the true peak is at the floor (silence)",
        })
    }
}

/// A config the export cannot act on ([`RenderError::InvalidConfig`]).
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The graph's editor does not feed its executor.
    #[error("the graph's editor does not feed its executor; pass the pair built together")]
    UnpairedGraph,
    /// The graph is prepared at another rate than the render.
    #[error(
        "the graph is prepared at {prepared} Hz and the render is at {render} Hz; \
         prepare it with `RenderGraph::prepare`"
    )]
    RateMismatch {
        /// The rate the graph was prepared at, in Hz.
        prepared: f64,
        /// The rate the render asks for, in Hz.
        render: f64,
    },
    /// The graph's channels have no place in the file's: the topology the file
    /// states cannot take them, or is not as wide as the file.
    #[error("the graph's channels cannot be placed into the file's: {0}")]
    Placement(tutti_buffers::FoldError),
}

impl From<tutti_codec::EncodeError> for RenderError {
    fn from(error: tutti_codec::EncodeError) -> Self {
        match error {
            tutti_codec::EncodeError::Io(e) => Self::Io(e),
            tutti_codec::EncodeError::Buffer(e) => Self::Buffer(e),
            tutti_codec::EncodeError::Resample(e) => Self::Resample(e),
            other => Self::Encode(other),
        }
    }
}

/// `std::result::Result` with this crate's [`RenderError`](enum@RenderError) as the error
/// type.
pub type Result<T> = std::result::Result<T, RenderError>;
