#![doc = include_str!("../README.md")]
//! **The OFFLINE edge**: render a Tutti graph to a file, or to buffers, faster (or
//! slower) than real time. The `tutti` facade's `Bounce` drives it
//! (feature `export`).
//!
//! ## What is here
//!
//! The graph is *pulled* to a known frame count that a `Job` fixes up front, once,
//! and every `Output` of the render is fed from that one pass. `render` takes the
//! `Job`, a list of `Output`s and a `Control`:
//!
//! - An `Output` is a `Feed` (the graph's outputs, or a plan tap, `Feed::Tap`, read
//!   after every block and placed by a `Placement`) and a target: a file
//!   (`FileTarget`: a `tutti_codec::Encoding`, a `ToFile` and the normalize) or planes in
//!   memory (`BufferTarget`, returned as a `tutti_buffers::Wave`) or a caller's sink (`SinkTarget`).
//!   `Outputs` holds them by kind: `Outputs::file` and `Outputs::buffers` each return the key
//!   (`FileKey`, `BufferKey`) its result is read by, from the `Done` that `render` returns
//!   (`Done::file`, `Done::buffers`); `Outputs::sink` returns none, its result is the sink.
//! - A file output that does not normalize **streams**: each block goes through
//!   its resample, dither and encoder as it arrives and no PCM is held.
//! - A file output that normalizes keeps what the graph rendered for it (in memory
//!   up to 512 MiB across the render, then in a temporary file beside its
//!   target), measures it at the file's rate, and writes it again with the gain.
//!   The graph is never rendered twice. An output can take another's gain
//!   (`Gain::Of`), so a set of stems keeps the main output's balance.
//! - Every file is written to a temporary beside its path and put in place at the
//!   end, after every output has been finalized and the fork and the event loss
//!   have been checked. A render that fails or is cancelled leaves every path as it
//!   was. The renames are the one step that is not undone: if one fails after others
//!   succeeded the error is `RenderError::NotCommitted`, which names the files in place.
//! - Planes a caller already holds are written with `tutti_codec::write`, which takes the
//!   same `Encoding` (a buffer output is a `Wave`, which is what it writes).
//!
//! `Control` is how a host watches and stops the render: a `CancelToken` that is
//! checked (before every block, between stages, during the measurement and the
//! write, before each output is finalized and before the renames) and a callback
//! that is handed a `Progress`, a `Stage` and one fraction of the whole render.
//! `Control::none()` is a render nobody watches or stops. A cancelled render is
//! `Outcome::Cancelled`, with every temporary removed and every target as it was.
//! Encoding is WAV, FLAC or AIFF, each
//! behind its own `tutti-codec` feature (`encode-wav`, `encode-flac`, `encode-aiff`).
//! **None is on by default**, so a consumer names the formats it writes. The
//! umbrellas (`tutti`, `tutti-session`) forward them under the same names; their
//! `export` feature implies `encode-wav` only.
//!
//! This crate re-exports no other crate's type: a `Format` or `Pcm` is
//! `tutti_codec`'s, a `ChannelLayout` is `tutti_buffers`', and the clocks (`RenderClock`,
//! `FrozenClock`) are `tutti_timeline`'s. Each figure it needs of one of them it asks
//! that crate for: the frames a file can hold of the encoder, the length of a resampled
//! stream of the resampler.
//!
//! What a render is and where it goes are values of this crate (`Job`, the
//! targets); a project's bounce, whose figures are derived from a session, is
//! `tutti-session`'s `Bounce`, and this crate is the pipeline it drives.
//!
//! ## What this crate does not own
//!
//! - **Live capture.** Its peer is the **LIVE** edge — a microphone, a
//!   tap, a session take (`tutti-session`) pushing blocks into an `EncoderSink` as they arrive. The distinction is
//!   structural rather than stylistic: a live capture has no total frame count (it
//!   ends when someone stops it), so it cannot be expressed as an export, and an
//!   export needs a total (to size a plan, trim latency, cap output), so it cannot
//!   be expressed as a live pump. Reach here to bounce a mix; reach for a session take
//!   to record one.
//! - **Loudness measurement.** EBU R128 lives in `tutti-measure`, which is where
//!   the engine's `Config`/`State`/step analysis vocabulary is;
//!   a normalizing output measures with it. The edge is acyclic —
//!   `tutti-measure` does not depend on this crate.
//! - **Threads.** `render` is synchronous and runs on the calling
//!   thread. A host that wants a render off the main thread runs it on its own
//!   task pool.
//! - **A buffering strategy.** Every format streams, because every codec library
//!   used here supports incremental encoding. There is no buffered-versus-streaming
//!   mode to pick.
//!
//! ## Example — bounce a graph
//!
//! A `Job` is a rate and three frame counts, and a target names a file or planes.
//! The clock is not optional:
//! `FrozenClock` is how a caller *says* "this graph has no time-dependent nodes",
//! so forgetting a transport is a compile error rather than a silently silent
//! render.
//!
//! The graph is `tutti_graph`'s, built with its `GraphBuilder` and
//! prepared at the render's rate (`RenderGraph::prepare`): a graph prepared at
//! another rate is refused rather than re-rated. A graph is made one way, `RenderGraph::assemble(editor, executor, Setup)`,
//! which checks that the pair was built together; a host exporting its live graph
//! forks it and assembles the fork (`tutti-session` does both).
//!
//! ```rust
//! use tutti_timeline::FrozenClock;
//! use tutti_units::{Hz, SampleRate, Samples, Seconds};
//! use tutti_export::{render, BufferTarget, Control, FileTarget, Job, Output, Outputs, RenderGraph, Setup, TailPolicy};
//! use tutti_buffers::ChannelLayout;
//! use tutti_codec::{Encoding, FlacLevel, Format, IntDepth, IntPcm, Pcm, ToFile};
//! use tutti_rt::SplitMix64;
//! use tutti_graph::GraphBuilder;
//! use tutti_nodes::testing::Osc;
//!
//! let rate = SampleRate::SR_48K;
//! // A render borrows its graph; build one per render.
//! let tone = || {
//!     let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
//!     let osc = g.add(Osc::sine(Hz::new(440.0)));
//!     g.pipe_output(osc);
//!     let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
//!     RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
//! };
//!
//! let job = Job::new(rate, Seconds::new(0.5).to_samples(rate), Samples::new(0), TailPolicy::Frames(Samples::ZERO))
//!     .expect("a half second");
//!
//! // To buffers: a `Wave`, planar, one channel per plane, and `len()` is FRAMES per
//! // channel rather than the total sample count.
//! let mut outputs = Outputs::new();
//! let planes = outputs.buffers(Output::main_output(BufferTarget::new(ChannelLayout::STEREO)));
//! let done = render(&mut tone(), &job, &FrozenClock, outputs, &mut Control::none())
//!     .expect("a 0.5 s stereo render")
//!     .into_finished()
//!     .expect("nothing cancels it");
//! let wave = done.buffers(&planes).expect("a key of these outputs");
//! assert_eq!(wave.channels(), 2);
//! assert_eq!(wave.len().get(), 24_000);
//!
//! // Or straight to files, which streams — no PCM is held whole. One pass can
//! // write several, each in its own format.
//! let dir = tempfile::tempdir().expect("temp dir");
//! let mut outputs = Outputs::new();
//! let files = [
//!     outputs.file(Output::main_output(FileTarget::new(ToFile::new(
//!         dir.path().join("main_output.flac"),
//!         Encoding::new(Format::Flac { pcm: IntPcm::new(IntDepth::Int24), level: FlacLevel::Standard }),
//!     )))),
//!     outputs.file(Output::main_output(FileTarget::new(ToFile::new(
//!         dir.path().join("main_output.wav"),
//!         Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))))
//!             .dither_seed(SplitMix64::mix(1)),
//!     )))),
//! ];
//! let done = render(&mut tone(), &job, &FrozenClock, outputs, &mut Control::none())
//!     .expect("flac and wav encode")
//!     .into_finished()
//!     .expect("nothing cancels it");
//! for key in &files {
//!     let written = done.file(key).expect("a key of these outputs");
//!     assert!(written.bytes > 0, "a finalized export reports its size on disk");
//! }
//! ```
//!
//! ## Normalization is part of an output, not of the render
//!
//! Choosing a gain means measuring the whole signal first, which is two passes. So
//! an output that asks for it (`FileTarget::normalize`) does not stream: its
//! audio waits in a spill while the graph renders, is measured at the file's rate
//! (`Normalize::Peak` or `Normalize::Lufs`, which an integer format's headroom
//! lowers) and is written with the gain, which `Written::gain` reports beside the
//! reading it came from, `Written::measured`.
//!
//! For a gain that is logged, gated, or derived some other way, compose the steps
//! directly: render into a `BufferTarget`, take `Normalize::gain_for_wave`,
//! apply it with `tutti_buffers::Wave::apply_gain`, and write with `tutti_codec::write`.
//!
//! ## Features
//!
//! None. The formats are `tutti-codec`'s `encode-wav`, `encode-flac`
//! and `encode-aiff` (a consumer turns them on beside this
//! crate), and a `Format` variant exists only with its feature.
//!
//! Resampling (`tutti_dsp::StreamingResampler`, an FFT resampler on
//! `tutti_dsp::RealFft`) is an algorithm in `tutti-dsp`; `tutti_codec::Chain` calls it, and a
//! normalizing output's measurement runs the same `tutti_codec::Converter`. Resampling is
//! unconditional: it is not a codec, so it is not gated.
//!
//! A render is written from any `tutti_dsp::FftFloat` (`Wave<F>`, `f32` by default). A float
//! file takes it as it is; an integer file is quantized by the encoder, which adds the
//! dither noise its `tutti_codec::IntPcm` names, converts by the rule in
//! `tutti_buffers::Sample` and counts the values that clipped (`Written::clipped_samples`).
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{ConfigError, DoneError, RenderError, Result, TargetError, Unmeasured};

mod control;
pub use control::{CancelToken, Control, Outcome, Progress, Stage};

mod job;
pub use job::{EventLossPolicy, Job, JobError, MAX_FRAMES, TailPolicy};

mod output;
pub use output::{BufferKey, BufferTarget, Feed, FileKey, FileTarget, Gain, Output, Placement};

mod sink_target;
pub use sink_target::SinkTarget;

mod outputs;
pub use outputs::{Done, Outputs};

mod normalize;
pub use normalize::Normalize;

mod spill;

mod graph;
pub use graph::{GRAPH_MAX_BLOCK, RenderGraph, RtLogSink, Setup};

pub(crate) mod render;

use std::path::PathBuf;
use tutti_dsp::FftFloat;
use tutti_measure::Loudness;
use tutti_timeline::RenderClock;
use tutti_units::{Db, Samples};

/// A file an export wrote, from a file [`Output`] of [`render()`]
/// ([`Done::file`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    /// Where it was written: the `path` the entry point was given.
    pub path: PathBuf,
    /// Size on disk after finalization, in bytes. `0` if the file could not
    /// be stat'd.
    pub bytes: u64,
    /// Frames the file holds, at the file's rate: what the encoder was given,
    /// after the latency trim, the length and any resample (the stated length
    /// of the resample, `tutti_dsp::resampled_frames`).
    pub frames: Samples,
    /// Events the graph lost during the render that produced the file;
    /// nonzero only under [`EventLossPolicy::Allow`] (under `Fail` the render
    /// is an error instead).
    pub dropped_events: u64,
    /// Values the conversion to an integer format clipped (after any gain,
    /// resample and dither): those whose rounded value lies past the integer
    /// format's `MAX` or `MIN`, held there by the rule in
    /// [`Sample`](tutti_buffers::Sample); an infinity is counted. A NaN is
    /// written as 0 and is not counted. `0` for a format that does not quantize (`Pcm::Float(FloatWidth::F32)`,
    /// `Pcm::Float(FloatWidth::F64)`). A
    /// normalizing export keeps its target and ceiling one LSB plus the dither
    /// amplitude below full scale for an integer format, so this stays `0`
    /// unless the programme itself was past full scale.
    pub clipped_samples: u64,
    /// The gain a normalizing output applied ([`FileTarget::normalize`]);
    /// `None` for the others, so `None` means the file is as rendered. Chosen
    /// against the target or ceiling after an integer format's headroom was
    /// taken off it, so for [`Normalize::Peak`] it is
    /// `target - measured.true_peak` of that lowered target. An output that
    /// took another's gain ([`Gain::Of`]) reports that gain, the
    /// same figure the output it followed reports.
    pub gain: Option<Db>,
    /// What a normalizing output read from the audio before it applied
    /// [`gain`](Self::gain): the programme at the file's rate (after any
    /// resample, which moves the true peak), measured once. `None` where
    /// `gain` is, and for an output that took another's gain: it measured
    /// nothing, and the reading is of the output it followed.
    ///
    /// `Some` only when a placement-weighted loudness was measured: for a
    /// `Peak` or `Lufs` output whose channels have a placement (stated, or
    /// the default layout for a width that has a bed). A `Peak` output with none (an
    /// unplaced width such as 3 channels) measured the true peak alone, and
    /// reports `None` rather than a loudness no placement stands behind;
    /// [`gain`](Self::gain) is still its peak normalization.
    pub measured: Option<Loudness>,
    /// Whether the file's channels were placed by the output
    /// ([`Encoding::channels`](tutti_codec::Encoding::channels) named speakers other than the default:
    /// [`Stated`](tutti_buffers::Placement::Stated))
    /// or taken to be the engine's order for the width
    /// ([`ChannelTopology::default_for`](tutti_buffers::ChannelTopology::default_for):
    /// [`Default`](tutti_buffers::Placement::Default)). A `Lufs` normalize
    /// weighted the file's loudness by that placement.
    pub placement: tutti_buffers::Placement,
    /// The spill file a normalizing output kept its rendered audio in, when
    /// removing it failed: it is left beside [`path`](Self::path) for the
    /// caller to delete. `None` when there was none or it is gone.
    pub spill_left: Option<PathBuf>,
}

impl Written {
    /// The file `landed` put at `path`, its channels placed as `placement`: as rendered (no
    /// gain, no reading, no spill left) and with no events lost. Its size is read from disk,
    /// `0` if the file cannot be stat'd.
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        landed: tutti_codec::Landed,
        placement: tutti_buffers::Placement,
    ) -> Self {
        let path = path.into();
        Self {
            bytes: std::fs::metadata(&path).map_or(0, |m| m.len()),
            path,
            frames: landed.frames(),
            dropped_events: 0,
            clipped_samples: landed.clipped(),
            gain: None,
            measured: None,
            placement,
            spill_left: None,
        }
    }
}

/// Renders `graph` once into every one of `outputs`, and writes or returns
/// what each becomes ([`Done`], read by the key each output was added under).
///
/// One pass: the graph is stepped once per block however many outputs there
/// are, and each output reads its [`Feed`] (the graph's outputs, or a plan tap)
/// folds it onto its target's layout, and writes it. An output that does
/// not normalize is streamed through its resample, dither and encoder as the
/// blocks arrive and holds no PCM; one that does keeps its audio (in memory up
/// to 512 MiB across the render, past it in a temporary file beside its
/// target, removed afterwards), is measured at its file's rate and written
/// again with the gain, and the graph is never rendered twice. An output that
/// takes another's gain ([`Gain::Of`]) is written with it, so a
/// set of stems keeps the main output's balance.
///
/// The render is the `job`'s: its rate (the graph must be prepared at it:
/// [`RenderGraph::prepare`]), the audible span, the
/// latency trimmed from the head of every output, the tail kept past the end
/// and what to do about a dropped event. `clock` is advanced once per block,
/// after the graph processes ([`render_graph`](tutti_timeline::render_graph)); pass
/// [`FrozenClock`](tutti_timeline::FrozenClock) for a graph with no time-dependent nodes. `graph` is
/// borrowed, so a graph holding a unit whose teardown is main-thread-only (an
/// in-process VST2 plugin's fork) and rendered on a worker is brought back and
/// dropped on the main thread.
///
/// Every file is written to a temporary beside its path and moved into place
/// at the end, after every output's encoder has been finalized and the fork
/// and the event loss have been checked: a render that fails, or is
/// cancelled, leaves every path as it was and no temporary. The renames are
/// the one step that is not undone: if one fails after others succeeded the
/// error is [`RenderError::NotCommitted`] and the files already in place are kept.
/// The finished file whose rename failed is kept beside its path, named by
/// the error ([`tutti_codec::EncodeError::Commit`]): moving or deleting it is the
/// caller's.
///
/// `control` is how a host watches and stops the render: it is told the
/// [`Stage`] and fraction as the render goes (`Rendering`, then with a
/// normalizing output `Measuring` and `Writing`, then `Finalizing`), and a
/// [`CancelToken`] it holds is checked at every point this render has:
/// before each block, between the stages, in each chunk
/// of the measurement and the write, before each output is finalized and once
/// more before the first rename. A cancelled render is [`Outcome::Cancelled`],
/// not an error. Pass [`Control::none`] for a render nobody watches.
///
/// # Errors
///
/// [`RenderError::NoOutputs`] for an empty list; [`RenderError::UnsupportedChannels`] for
/// a target of no channels; [`RenderError::Target`] for an output that cannot be
/// rendered as given ([`TargetError`]: a tap whose lanes are not as wide as
/// its placement or cannot be placed on the target, a gain taken from an
/// output that does not measure or from another [`Outputs`], two outputs with
/// one path);
/// [`RenderError::InvalidConfig`] if the graph was prepared at another rate or its
/// editor does not feed its executor, or the graph's channels cannot be placed
/// on a target's ([`ConfigError::Placement`]); an encoder's refusal of a format
/// and width ([`RenderError::Encode`], before anything is rendered);
/// [`RenderError::RenderTooLarge`] for planes that cannot be held;
/// [`RenderError::ForkFailed`] if a forked node (a hosted plugin) failed during the
/// render and [`RenderError::EventsDropped`] if the graph lost an event and the
/// job's policy is [`EventLossPolicy::Fail`] (the default), both checked
/// before anything is put in place;
/// [`RenderError::TapOverrun`] and [`RenderError::TapStarved`] from a tap;
/// [`RenderError::Unmeasurable`] for a normalizing output whose audio has no reading
/// to take a gain from (silent, under 400 ms or under the loudness gate for
/// [`Normalize::Lufs`], at the true-peak floor for [`Normalize::Peak`], or a
/// width or rate the meter does not take); [`RenderError::Resample`];
/// [`RenderError::Io`] and [`RenderError::NotCommitted`].
pub fn render<F: FftFloat>(
    graph: &mut RenderGraph<F>,
    job: &Job,
    clock: &dyn RenderClock,
    outputs: Outputs<F>,
    control: &mut Control<'_>,
) -> Result<Outcome<Done<F>>> {
    render::outputs::render_outputs(
        graph,
        job,
        clock,
        outputs,
        control,
        spill::SPILL_MEMORY_BYTES,
    )
}
