//! The session's bounce (feature `export`): the
//! request ([`Bounce`]), the job ([`BounceJob`]: gathered on the control
//! thread, built and rendered by a blocking task of the session's executor), and
//! its return ([`BounceRendered`]), which [`Session::apply`] takes back: it
//! drops what the render leaves on the main thread, releases the bounce's token
//! and gives the [`Bounced`].
//!
//! A [`Bounce`] is data. Every figure it does not state is derived from the
//! session when the bounce starts and stored once: the range from
//! the transport's loop, the clock from the project's tempo map, the render
//! rate from the session's, the latency and the tail from the compiled graph,
//! the main output's width from the main output. Each has one explicit method.
//!
//! **One bounce at a time**: [`Session::bounce`] mints a bounce token, a
//! clone of an `Arc<()>` the session holds, which rides in the job, in the
//! future awaiting it ([`Bouncing`]) and then in what it returns
//! ([`BounceRendered`]). A bounce is in flight while any token lives, and
//! another is refused ([`BounceError::InFlight`]). With `plugin`,
//! [`Phase::RenderMode`](crate::Phase::RenderMode) announces Offline from that
//! level: `bounce` runs the phase before it returns (so a plugin hears Offline
//! before the job can render a block), and the next phase run after the token
//! drops (the next [`update`](Session::update)) announces Realtime. `apply`
//! does not announce: a host that starts the next bounce before its next
//! update never tells its plugins Realtime and then Offline again between two
//! bounces. A job, a future or a return dropped unfinished (a cancelled task, a
//! panic on the worker) drops its token, so the next update restores Realtime.
//!
//! **Cancelling and watching.** A job carries a [`BounceHandle`]
//! ([`BounceJob::handle`]), cloned to whoever watches:
//! [`cancel`](BounceHandle::cancel) stops the render at the next point the
//! pipeline checks (between the session's steps, every block, between its
//! stages, and before each file is finalized and put in place; or before it
//! starts), and [`progress`](BounceHandle::progress) reads its stage and
//! fraction, [`Stage::Preparing`](tutti_export::Stage::Preparing) included (the session's steps, in the band
//! the pipeline leaves it). Dropping the [`Bouncing`] before it resolves
//! cancels it. A cancelled job returns [`BounceError::Cancelled`]
//! from [`Session::apply`], drops its token when the render has
//! stopped, and leaves a file target's path as it was.
//!
//! **A token that never drops** (a job `mem::forget`ed, or kept unawaited
//! forever) keeps the bounce in flight: the plugins stay Offline and every
//! later bounce is refused. [`Phase::Collect`](crate::Phase::Collect) warns,
//! once, when one has been in flight for an hour of its `now`.
//!
//! **Leftovers drop on the main thread.** The rendered graph, what a failed
//! fork build had made, an edit that never ran: each is wrapped so that
//! wherever it is dropped, it is sent back to the session instead, and
//! dropped by `apply` or the next
//! [`Phase::Collect`](crate::Phase::Collect). A forked in-process VST2
//! plugin closes its instance when dropped, which VST2 confines to the main
//! thread. Something sent once the session is gone drops where it is.
//!
//! **The main output's destination is a type.** [`Bounce::main_output`] turns a bounce with
//! no main output ([`NoMainOutput`]) into one with a [`Destination`] ([`ToFile`] or
//! [`ToMemory`]), and only one with a main output starts: a bounce with no main output is not
//! writable. The destination rides through [`BounceJob`] and [`BounceRendered`] to
//! [`Bounced`], whose main output is a [`Written`](tutti_export::Written) for a file and a [`Rendered`](tutti_export::Rendered) for planes,
//! with no `Option` to unwrap. A cancelled bounce is [`BounceError::Cancelled`].

use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::{Context, Poll};
use std::time::Duration;
use tutti_platform::{Instant, WeakExecutor};

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_export::{
    BufferKey, CancelToken, Control, Done, DoneError, EventLossPolicy, FileKey, GRAPH_MAX_BLOCK,
    Gain, Job, JobError, Normalize, Outcome, Output, Outputs, Placement, Progress, TailPolicy,
};
use tutti_graph::{CommitError, Editor, MissingSource, NodeKey, OutputPort, TapId, TapOptions};
use tutti_rt::Mutex;
use tutti_tempo::TempoMap;
use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport, RenderClock};
use tutti_units::{Beat, Frame, SampleRate, Samples, Seconds};

use crate::apply::{Cut, Sealed as ApplySealed};
use crate::format::Sealed;
use crate::unblocked::{Unanswered, Unblocked};
use crate::{Session, SessionFloat};
use tutti_buffers::Float;
use tutti_dsp::FftFloat;

use crate::destination::sealed::{Landing, MainShape, StemOutput, StemShape};
use crate::fork::PendingExport;
use crate::{Destination, ToFile, ToMemory};

/// The span of the project a bounce renders, in the unit the caller thinks in.
///
/// Beats go to frames through the project's tempo map at the render's rate
/// (doc 020, section 6: the first frame at or after each beat, so a step or a
/// ramp is followed); the span's length is the difference of the two frames.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Range {
    /// The transport's loop region, when one is set and armed
    /// ([`BounceConfigError::NoRange`] otherwise).
    Loop,
    /// From beat 0 to this beat: the end of a project whose clips end there,
    /// for a host that knows it.
    Until(Beat),
    /// From one beat to another.
    Beats(Beat, Beat),
    /// From one time to another, on the project's tempo map.
    Seconds(Seconds, Seconds),
    /// From one frame to another, counted from the project's start at the
    /// render's rate.
    Frames(Frame, Frame),
}

/// `Beat::new(0.0)..Beat::new(8.0)` is [`Range::Beats`]: the span from one beat to the
/// other, end exclusive as a Rust range is.
impl From<std::ops::Range<Beat>> for Range {
    fn from(r: std::ops::Range<Beat>) -> Self {
        Self::Beats(r.start, r.end)
    }
}

/// `Seconds::new(0.0)..Seconds::new(2.0)` is [`Range::Seconds`].
impl From<std::ops::Range<Seconds>> for Range {
    fn from(r: std::ops::Range<Seconds>) -> Self {
        Self::Seconds(r.start, r.end)
    }
}

/// `Frame::new(0)..Frame::new(48_000)` is [`Range::Frames`].
impl From<std::ops::Range<Frame>> for Range {
    fn from(r: std::ops::Range<Frame>) -> Self {
        Self::Frames(r.start, r.end)
    }
}

/// Which leading frames a bounce drops: the latency the graph's plan
/// compensates every output by (a look-ahead limiter's, a hosted plugin's) is
/// silence ahead of the programme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BounceLatency {
    /// The compiled plan's total latency, asked of the graph that renders (after
    /// the edit). The default.
    #[default]
    Graph,
    /// Nothing: the file starts with the latency's silence, and keeps the
    /// programme's last frames ([`BounceTail`] adds the latency that is not trimmed).
    Keep,
    /// This many frames.
    Frames(Samples),
}

/// What a bounce renders past the programme's end: a reverb's decay, a delay's
/// repeats, a plugin's declared tail.
///
/// A tail excludes latency, so whatever of the graph's latency the bounce does
/// not trim ([`BounceLatency`]) is rendered on top of the tail whichever this is,
/// or the programme's last frames are cut.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum BounceTail {
    /// The tail the graph's nodes report when the programme has ended, at most
    /// what they declare (`GraphTail::resolve`'s rule against `cap`: a graph
    /// that never decays declares `cap`; otherwise what its nodes declared, at
    /// most `cap`). A node that narrows its tail as its settings change
    /// ([`Io::report_tail`](tutti_graph::Io::report_tail)) spends what is left
    /// of its report when nothing upstream still owes, and its declared
    /// ceiling after an upstream that does; one that never reports spends what
    /// it declares, and one at rest spends nothing. The graph is asked at the
    /// programme's end and after every block of the tail, and the tail is the
    /// smallest bound any ask gave (what is spent plus what is owed): never
    /// longer than the first ask's, shorter when an instrument goes idle and
    /// what follows it owes its own report. The figures and progress count the
    /// bound until the programme ends and the true total after. The default, with a cap of
    /// [`BounceTail::CAP`].
    Graph {
        /// The most it renders.
        cap: Seconds,
    },
    /// No tail.
    None,
    /// This many frames.
    Frames(Samples),
}

impl BounceTail {
    /// The default cap: ten seconds.
    pub const CAP: Seconds = Seconds::new(10.0);
}

impl Default for BounceTail {
    fn default() -> Self {
        Self::Graph { cap: Self::CAP }
    }
}

/// A bounce that has not chosen its main output yet: [`Bounce::new`]'s.
/// It does not start; [`Bounce::main_output`] chooses a [`Destination`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMainOutput;

/// A bounce: what to render and where it goes, with every figure it does not
/// name derived from the session when it starts. Start one with
/// [`Session::bounce`].
///
/// ```no_run
/// # use tutti_session::{Bounce, Range, Session, ToFile};
/// # use tutti_codec::{Encoding, Format, IntDepth, IntPcm, Pcm};
/// # use tutti_graph::NodeKey;
/// # use tutti_units::Beat;
/// # async fn go(session: &mut Session, drums: NodeKey) -> Result<(), tutti_session::BounceError> {
/// let rendering = session.bounce(
///     Bounce::new()
///         .range(Range::Beats(Beat::new(0.0), Beat::new(16.0)))
///         .main_output(ToFile::new(
///             Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24)))),
///             tutti_codec::ToFile::new("out/main_output.wav"),
///         ))
///         .stem(drums, "out/drums.wav"),
/// )?;
/// let rendered = rendering.await;
/// let done = session.apply(rendered)?;
/// # let _ = done;
/// # Ok(())
/// # }
/// ```
///
/// `N` names a stem's node: a [`NodeKey`] for the session, or a host's own
/// handle that [`map`](Self::map) resolves to one before the bounce
/// starts.
#[must_use = "a bounce renders nothing until the session starts it"]
pub struct Bounce<N = NodeKey, D = NoMainOutput, F: Float = f32> {
    main_output: D,
    stems: Vec<StemRequest<N, F>>,
    range: Option<Range>,
    clock: Option<ExportClock>,
    sample_rate: Option<SampleRate>,
    latency: BounceLatency,
    tail: BounceTail,
    main_output_topology: Option<ChannelTopology>,
    serial: bool,
    on_event_loss: EventLossPolicy,
    normalize: Option<Normalize>,
    edit: Option<PrepareEdit<F>>,
}

impl<N, F: Float> Default for Bounce<N, NoMainOutput, F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N, F: Float> Bounce<N, NoMainOutput, F> {
    /// A bounce of the project's mix, with nothing chosen: a main output
    /// ([`main_output`](Self::main_output), without which it does not start) and a range
    /// when no loop is set. Name nodes as stems with [`stem`](Self::stem) or
    /// [`stem_with`](Self::stem_with); `N` is what names one (a [`NodeKey`], or a host's own
    /// handle for it: [`map`](Self::map) resolves them).
    pub fn new() -> Self {
        Self {
            main_output: NoMainOutput,
            stems: Vec::new(),
            range: None,
            clock: None,
            sample_rate: None,
            latency: BounceLatency::default(),
            tail: BounceTail::default(),
            main_output_topology: None,
            serial: false,
            on_event_loss: EventLossPolicy::default(),
            normalize: None,
            edit: None,
        }
    }

    /// The main output into `destination`: a [`ToFile`], written beside its path and renamed
    /// over it when the bounce finishes (so a cancelled or failed bounce leaves what was at
    /// the path as it was), or a [`ToMemory`]. [`Bounced::main_output`] is the destination's
    /// [`Output`](Destination::Output). A file main output is the one that can be
    /// [`normalize`](Bounce::normalize)d, which only a file honours:
    ///
    /// ```compile_fail,E0599
    /// # use tutti_session::{Bounce, ToMemory};
    /// # use tutti_export::Normalize;
    /// # use tutti_units::Db;
    /// let _ = Bounce::<tutti_graph::NodeKey>::new()
    ///     .main_output(ToMemory::new())
    ///     .normalize(Normalize::Peak { target: Db::new(-1.0) });
    /// ```
    pub fn main_output<D: Destination>(self, destination: D) -> Bounce<N, D, F> {
        let Self {
            main_output: _,
            stems,
            range,
            clock,
            sample_rate,
            latency,
            tail,
            main_output_topology,
            serial,
            on_event_loss,
            normalize,
            edit,
        } = self;
        Bounce {
            main_output: destination,
            stems,
            range,
            clock,
            sample_rate,
            latency,
            tail,
            main_output_topology,
            serial,
            on_event_loss,
            normalize,
            edit,
        }
    }
}

impl<N, D, F: Float> Bounce<N, D, F> {
    /// Also writes the stem `stem` describes, to the destination it names
    /// ([`Stem::to`]); a [`Stem::new`] one is held in memory.
    pub fn stem_with<S: Destination>(mut self, stem: Stem<N, S>) -> Self {
        self.stems.push(StemRequest::of(stem));
        self
    }

    /// The same bounce with each stem's node named by what `f` makes of it:
    /// a host that names nodes by its own handle resolves them to
    /// [`NodeKey`]s once, when the bounce starts. The first `Err` is returned
    /// and nothing is kept.
    ///
    /// # Errors
    ///
    /// What `f` returns for the first node it refuses.
    pub fn map<N2, E>(self, mut f: impl FnMut(N) -> Result<N2, E>) -> Result<Bounce<N2, D, F>, E> {
        let Self {
            main_output,
            stems,
            range,
            clock,
            sample_rate,
            latency,
            tail,
            main_output_topology,
            serial,
            on_event_loss,
            normalize,
            edit,
        } = self;
        let stems = stems
            .into_iter()
            .map(|stem| stem.map(&mut f))
            .collect::<Result<Vec<_>, E>>()?;
        Ok(Bounce {
            main_output,
            stems,
            range,
            clock,
            sample_rate,
            latency,
            tail,
            main_output_topology,
            serial,
            on_event_loss,
            normalize,
            edit,
        })
    }

    /// The span to render ([`Range`]). Without it the transport's loop is
    /// rendered, and a project with no loop set is
    /// [`BounceConfigError::NoRange`]: nothing is guessed. A host that knows
    /// where its clips end names it with [`Range::Until`].
    pub fn range(mut self, range: impl Into<Range>) -> Self {
        self.range = Some(range.into());
        self
    }

    /// The render's time. Without it, an offline timeline over the project's
    /// tempo map, started at the range's first beat with the loop off, so a
    /// transport-driven node reads what playback gives it. See [`ExportClock`].
    pub fn clock(mut self, clock: ExportClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The rate the graph is forked, prepared and rendered at; the session's
    /// without it.
    pub fn sample_rate(mut self, rate: impl Into<SampleRate>) -> Self {
        self.sample_rate = Some(rate.into());
        self
    }

    /// What to trim from the head; see [`BounceLatency`].
    pub fn latency(mut self, latency: BounceLatency) -> Self {
        self.latency = latency;
        self
    }

    /// What to render past the end; see [`BounceTail`].
    pub fn tail(mut self, tail: BounceTail) -> Self {
        self.tail = tail;
        self
    }

    /// What the graph's output channels are, in place of the session's declared
    /// main output topology. It is as wide as the graph's main output, or the
    /// render refuses it
    /// ([`RenderError::TopologyWidth`](tutti_export::RenderError::TopologyWidth)).
    pub fn main_output_topology(mut self, main_output: ChannelTopology) -> Self {
        self.main_output_topology = Some(main_output);
        self
    }

    /// Renders on one thread, without the session's worker pool.
    pub fn serial(mut self) -> Self {
        self.serial = true;
        self
    }

    /// What the bounce does when the graph dropped an event: fails, without
    /// this.
    pub fn on_event_loss(mut self, policy: EventLossPolicy) -> Self {
        self.on_event_loss = policy;
        self
    }

    /// Edits the forked graph before it renders: see [`PrepareEdit`].
    pub fn edit(mut self, edit: PrepareEdit<F>) -> Self {
        self.edit = Some(edit);
        self
    }
}

/// What only a file honours: a normalize, and the stems that share the main output's format.
impl<N, F: Float> Bounce<N, ToFile, F> {
    /// Also writes `node`'s own outputs into a file at `path` in the main output's
    /// format, in the same pass as the main output: see [`Stem`]
    /// ([`stem_with`](Self::stem_with) for one that places its channels, resamples or
    /// states its own encoding).
    ///
    /// A stem is the node's output as it leaves the node, aligned with the
    /// main output (what a bus or the main output applies downstream is not in it), at
    /// the node's own width.
    pub fn stem(self, node: N, path: impl Into<PathBuf>) -> Self {
        let to = self.main_output.beside(path);
        self.stem_with(Stem::new(node).to(to))
    }

    /// Normalizes the main output file to `normalize`: the render is kept, measured
    /// and written with the gain, never rendered twice.
    pub fn normalize(mut self, normalize: Normalize) -> Self {
        self.normalize = Some(normalize);
        self
    }
}

impl<N: std::fmt::Debug, D: std::fmt::Debug, F: Float> std::fmt::Debug for Bounce<N, D, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bounce")
            .field("main_output", &self.main_output)
            .field("stems", &self.stems)
            .field("range", &self.range)
            .field("clock", &self.clock)
            .field("sample_rate", &self.sample_rate)
            .field("latency", &self.latency)
            .field("tail", &self.tail)
            .field("main_output_topology", &self.main_output_topology)
            .field("serial", &self.serial)
            .field("on_event_loss", &self.on_event_loss)
            .field("normalize", &self.normalize)
            .field("edit", &self.edit.is_some())
            .finish()
    }
}

/// One node's own output, written beside the main output in the same pass: `N` names
/// the node ([`NodeKey`] by default), and `D` is where it goes ([`ToMemory`] by default,
/// or the [`Destination`] [`to`](Stem::to) names).
///
/// A stem is what the node produces, read where it leaves the node and
/// delayed to the time the main output leaves the plan (so every stem and the
/// main output are sample-aligned and trimmed alike); what the buses and the
/// main output apply downstream of the node is not in it. It is the node's whole
/// output, in port order, at the node's own width, unless it says
/// otherwise:
///
/// - [`channels`](Self::channels) taps some of the node's output channels,
///   in the order given;
/// - [`topology`](Self::topology) states what the tapped channels are (the
///   engine's order for the width without it).
///
/// The width of the stem's file or planes is its destination's: an [`Encoding`] that states
/// channels, or [`ToMemory::channels`]. The tapped channels are folded onto it from their
/// topology, and a fold that cannot place them refuses the bounce when it renders (nothing
/// is truncated).
///
/// Only a file stem has a gain ([`gain`](Stem::gain)): a stem held in memory is
/// not measured, so it cannot be asked for one.
///
/// ```compile_fail,E0599
/// # use tutti_session::{Stem, StemGain};
/// # use tutti_graph::NodeKey;
/// # fn go(node: NodeKey) {
/// let _ = Stem::new(node).gain(StemGain::Shared);
/// # }
/// ```
///
/// The main output's declaration is never applied to a stem: it describes the
/// graph's output bus, not a node's output.
#[derive(Debug, Clone, PartialEq)]
pub struct Stem<N = NodeKey, D = ToMemory> {
    node: N,
    channels: Option<Vec<u16>>,
    topology: Option<ChannelTopology>,
    gain: StemGain,
    destination: D,
}

impl<N> Stem<N, ToMemory> {
    /// `node`'s output into memory ([`Bounced::stems`] is
    /// [`StemKey::Buffers`]), one plane per channel, at the render's rate:
    /// nothing is encoded, resampled, dithered or normalized, as for [`ToMemory`].
    /// [`to`](Self::to) names another destination.
    pub fn new(node: N) -> Self {
        Self {
            node,
            channels: None,
            topology: None,
            gain: StemGain::None,
            destination: ToMemory::new(),
        }
    }

    /// This stem into `destination`: a [`ToFile`] is written beside its path and renamed
    /// over it when the bounce finishes, as the main output is, and its gain is
    /// [`StemGain::None`] until [`gain`](Stem::gain); a [`ToMemory`] states the planes'
    /// channels.
    #[must_use]
    pub fn to<D: Destination>(self, destination: D) -> Stem<N, D> {
        let Self {
            node,
            channels,
            topology,
            gain,
            destination: _,
        } = self;
        Stem {
            node,
            channels,
            topology,
            gain,
            destination,
        }
    }
}

impl<N> Stem<N, ToFile> {
    /// How this stem is normalized: as rendered without it.
    #[must_use]
    pub fn gain(mut self, gain: StemGain) -> Self {
        self.gain = gain;
        self
    }
}

impl<N, D> Stem<N, D> {
    /// Taps only these output channels of the node, in this order, in place
    /// of all of them. A channel the node has not is
    /// [`BounceConfigError::StemChannelMissing`].
    #[must_use]
    pub fn channels(mut self, channels: impl IntoIterator<Item = u16>) -> Self {
        self.channels = Some(channels.into_iter().collect());
        self
    }

    /// What the tapped channels are, in lane order. Its width must be the
    /// number tapped (a stem whose width differs is refused when it renders).
    #[must_use]
    pub fn topology(mut self, topology: ChannelTopology) -> Self {
        self.topology = Some(topology);
        self
    }

    /// The node this stem names.
    #[must_use]
    pub fn node(&self) -> &N {
        &self.node
    }

    /// The same stem with its node named by what `f` makes of it.
    ///
    /// # Errors
    ///
    /// What `f` returns.
    pub fn map<M, E>(self, f: impl FnOnce(N) -> Result<M, E>) -> Result<Stem<M, D>, E> {
        let Self {
            node,
            channels,
            topology,
            gain,
            destination,
        } = self;
        Ok(Stem {
            node: f(node)?,
            channels,
            topology,
            gain,
            destination,
        })
    }
}

/// A [`Stem`] of any destination, as the bounce holds it: its destination is built into an
/// output when the bounce starts and knows the stem's width.
struct StemRequest<N, F: Float> {
    node: N,
    channels: Option<Vec<u16>>,
    topology: Option<ChannelTopology>,
    gain: StemGain,
    output: Box<dyn FnOnce(StemShape) -> Box<dyn StemOutput<F>> + Send>,
}

impl<N, F: Float> StemRequest<N, F> {
    fn of<D: Destination>(stem: Stem<N, D>) -> Self {
        let Stem {
            node,
            channels,
            topology,
            gain,
            destination,
        } = stem;
        Self {
            node,
            channels,
            topology,
            gain,
            output: Box::new(move |shape| destination.stem(shape)),
        }
    }

    fn map<M, E>(self, f: impl FnOnce(N) -> Result<M, E>) -> Result<StemRequest<M, F>, E> {
        let Self {
            node,
            channels,
            topology,
            gain,
            output,
        } = self;
        Ok(StemRequest {
            node: f(node)?,
            channels,
            topology,
            gain,
            output,
        })
    }
}

impl<N: std::fmt::Debug, F: Float> std::fmt::Debug for StemRequest<N, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StemRequest")
            .field("node", &self.node)
            .field("channels", &self.channels)
            .field("topology", &self.topology)
            .field("gain", &self.gain)
            .finish_non_exhaustive()
    }
}

/// How a file stem is normalized ([`Stem::gain`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum StemGain {
    /// As rendered: what a DAW that adds its own gain wants.
    #[default]
    None,
    /// The stem takes the gain the main output was measured to need
    /// ([`Bounce::normalize`], required), so stems that sum to the main output
    /// still do. Right only while the main output is the plain sum of the stems;
    /// with processing on the main output bus, the sum is the caller's to keep.
    Shared,
    /// The stem is measured and normalized on its own. Breaks the balance
    /// between stems, by design: independent stem libraries.
    Each(Normalize),
}

/// Where a stem's result is read from: the key it was rendered under, by kind.
///
/// The kind is the stem's own choice ([`Stem::new`] for planes, a path for a
/// file), so a bounce's stems are a list of both.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StemKey {
    /// A file stem: read its [`Written`](tutti_export::Written) with [`Done::file`].
    File(FileKey),
    /// A stem in memory: read its [`Rendered`](tutti_export::Rendered) with [`Done::buffers`].
    Buffers(BufferKey),
}

/// What a finished bounce produced: the main output's result, which is what the
/// bounce's main output destination says it is ([`Written`](tutti_export::Written) for a [`ToFile`],
/// [`Rendered`](tutti_export::Rendered) for a [`ToMemory`]), and the stems' results, read from
/// [`stem_results`](Self::stem_results) by the [`StemKey`] each stem has, in the
/// order the bounce named them.
#[derive(Debug)]
pub struct Bounced<D: Destination, F: Float = f32> {
    main_output: D::Output<F>,
    /// The render's results, the main output's moved out.
    done: Done<F>,
    stems: Vec<StemKey>,
}

impl<D: Destination, F: Float> Bounced<D, F> {
    /// The main output's result.
    #[must_use]
    pub fn main_output(&self) -> &D::Output<F> {
        &self.main_output
    }

    /// The key of each stem, in the order the bounce named them.
    ///
    /// ```no_run
    /// # use tutti_session::{Bounce, Session, Stem, StemKey, ToMemory};
    /// # use tutti_graph::NodeKey;
    /// # async fn go(session: &mut Session, drums: NodeKey) -> Result<(), Box<dyn std::error::Error>> {
    /// let rendered = session
    ///     .bounce(
    ///         Bounce::new()
    ///             .range(tutti_session::Range::Until(tutti_units::Beat::new(4.0)))
    ///             .main_output(ToMemory::new())
    ///             .stem_with(Stem::new(drums)),
    ///     )?
    ///     .await;
    /// let done = session.apply(rendered)?;
    /// // Neither call consumes `done`.
    /// let StemKey::Buffers(key) = &done.stems()[0] else { return Ok(()) };
    /// let planes = done.stem_results().buffers(key)?.channels();
    /// # let _ = planes;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn stems(&self) -> &[StemKey] {
        &self.stems
    }

    /// The stems' results, read by [`stems`](Self::stems)' keys.
    #[must_use]
    pub fn stem_results(&self) -> &Done<F> {
        &self.done
    }

    /// The main output's result, owned.
    #[must_use]
    pub fn into_main_output(self) -> D::Output<F> {
        self.main_output
    }

    /// The main output's result, the stems' results and their keys, owned.
    #[must_use]
    pub fn into_parts(self) -> (D::Output<F>, Done<F>, Vec<StemKey>) {
        (self.main_output, self.done, self.stems)
    }

    /// The pipeline's results, with the main output's taken out by its key.
    fn new(
        mut done: Done<F>,
        main: &<D as crate::destination::sealed::Sealed>::Key,
        stems: Vec<StemKey>,
    ) -> Result<Self, DoneError> {
        Ok(Self {
            main_output: D::take(&mut done, main)?,
            done,
            stems,
        })
    }
}

/// The time an export renders in: the clock the renderer advances and the
/// timeline every transport-aware node reads, as **one** value, so a render
/// cannot be handed one and its nodes another.
///
/// - [`frozen`](Self::frozen): no musical time (an effect tail, a synth
///   patch, a test tone). The renderer's transport is stopped at beat 0.
/// - [`timeline`](Self::timeline): an `OfflineTimeline` (never the live
///   transport), seeded at the tempo and beat to render from, at the render's
///   rate.
#[derive(Clone)]
pub struct ExportClock(Clock);

#[derive(Clone)]
enum Clock {
    Frozen,
    Timeline {
        clock: Arc<dyn RenderClock>,
        timeline: OfflineTransport,
    },
}

impl ExportClock {
    /// No musical time; see the type docs.
    pub fn frozen() -> Self {
        Self(Clock::Frozen)
    }

    /// Renders against `timeline`: the renderer advances it, and every
    /// node reads its block's transport from it through `Env`.
    pub fn timeline(timeline: Arc<OfflineTimeline>) -> Self {
        Self(Clock::Timeline {
            clock: timeline.clone() as Arc<dyn RenderClock>,
            timeline: OfflineTransport::new(timeline),
        })
    }

    /// What the renderer advances.
    fn render_clock(&self) -> Arc<dyn RenderClock> {
        match &self.0 {
            Clock::Frozen => Arc::new(tutti_timeline::FrozenClock),
            Clock::Timeline { clock, .. } => Arc::clone(clock),
        }
    }

    /// The render's timeline, as [`PreparedGraph::ctx`] hands it.
    fn offline(&self) -> OfflineTransport {
        match &self.0 {
            Clock::Frozen => OfflineTransport::new(Arc::new(Stopped(Arc::new(
                TempoMap::constant(tutti_units::Bpm::default()).expect("120 BPM is a map"),
            )))),
            Clock::Timeline { timeline, .. } => timeline.clone(),
        }
    }
}

impl std::fmt::Debug for ExportClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Clock::Frozen => f.write_str("ExportClock::Frozen"),
            Clock::Timeline { .. } => f.write_str("ExportClock::Timeline(..)"),
        }
    }
}

/// [`ExportClock::frozen`]'s timeline: stopped at beat 0, what
/// `FrozenClock` hands the renderer. A stopped transport's tempo is never
/// read; its map is the engine's default, 120 BPM.
struct Stopped(Arc<TempoMap>);

/// The frozen render's timeline: only the render reads it.
impl tutti_timeline::OfflineClock for Stopped {}

impl tutti_timeline::Timeline for Stopped {
    fn beat(&self) -> tutti_units::Beat {
        tutti_units::Beat::new(0.0)
    }
    fn tempo_map(&self) -> Arc<TempoMap> {
        Arc::clone(&self.0)
    }
    fn is_rolling(&self) -> bool {
        false
    }
    fn segment_generation(&self) -> u64 {
        // Never moves, so never jumps.
        0
    }
}

/// What a bounce refuses that no render would: a request that names no
/// span, a graph with nothing to render, and an edit that
/// left the fork unusable. The pipeline's own refusals are
/// [`tutti_export::RenderError`] ([`BounceError::Export`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum BounceConfigError {
    /// The graph has no outputs to render.
    #[error("the graph has no outputs to render")]
    GraphHasNoOutputs,
    /// A stem names a node that is not in the graph.
    #[error("the stem's node {node:?} is not in the graph")]
    StemNotInGraph {
        /// The node.
        node: NodeKey,
    },
    /// A stem names a node no output reaches: the fork holds exactly what the
    /// outputs hear, so it renders nothing there.
    #[error("the stem's node {node:?} is not heard by any output")]
    StemNotAudible {
        /// The node.
        node: NodeKey,
    },
    /// A stem taps an output channel its node has not (a node with no audio
    /// outputs has none to tap, `channel` 0), or taps none.
    #[error("the stem's node {node:?} has no output channel {channel}")]
    StemChannelMissing {
        /// The node.
        node: NodeKey,
        /// The channel asked for.
        channel: u16,
    },
    /// Two targets name one path.
    #[error("two targets write {0:?}")]
    DuplicatePath(PathBuf),
    /// [`StemGain::Shared`] takes the main output's gain, and the bounce does not
    /// normalize its main output ([`Bounce::normalize`]).
    #[error("the stems take the main output's gain, and the main output is not normalized")]
    SharedGainWithoutMainOutput,
    /// There is no span to render: no loop is set and the bounce names neither
    /// a range ([`Bounce::range`]). Nothing is guessed.
    #[error("no range: the transport has no loop and the bounce names no range")]
    NoRange,
    /// The range keeps no frame: it ends where it begins or before, or is
    /// shorter than a frame at the render's rate.
    #[error("the range is empty at the render's rate")]
    EmptyRange,
    /// The range is not a span of the project: a NaN, an infinity or a
    /// position before the start.
    #[error("the range is not a span of the project")]
    InvalidRange,
    /// [`Job::new`] refused the render: a rate that is not finite and
    /// positive, or more frames than [`tutti_export::MAX_FRAMES`].
    #[error("the render cannot be a job: {0}")]
    Job(JobError),
    /// A prepare hook left the forked graph in a state that cannot commit.
    #[error("the prepare hook left the forked graph uncommittable: {0:?}")]
    PrepareHookUncommittable(CommitError),
    /// A prepare hook replaced the fork's editor with another graph's: it no
    /// longer feeds the fork's executor.
    #[error("the prepare hook replaced the forked graph's editor")]
    PrepareHookUnpaired,
}

/// Why a bounce could not start, render or be applied.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BounceError {
    /// Another bounce is in flight: its job, the future awaiting it, or what
    /// that returned is still alive. Apply or drop it first.
    #[error("another bounce is in flight")]
    InFlight,
    /// The bounce asks for something no render could give.
    #[error("bounce: {0}")]
    Config(#[from] BounceConfigError),
    /// A node the fork needs cannot be forked (it has no fork source), named by
    /// its key.
    #[error("bounce: {node:?} cannot be forked")]
    NotForkable {
        /// The node.
        node: NodeKey,
    },
    /// The fork was refused: a deferred fork source failed (naming its node),
    /// or the graph's shape did not allow one.
    #[error("bounce: the graph could not be forked: {0}")]
    Fork(tutti_graph::ForkError),
    /// The bounce was cancelled ([`BounceHandle::cancel`]): nothing was
    /// produced, and a file target is as it was.
    #[error("the bounce was cancelled")]
    Cancelled,
    /// The pipeline's: an I/O or encoder error, a fork that failed mid-render
    /// (naming its node's key), a lost event.
    #[error("bounce: {0}")]
    Export(tutti_export::RenderError),
    /// The render did not run to an answer: the session was dropped before the
    /// render started, or the executor dropped it before it ran.
    #[error("the session shut down before the bounce rendered")]
    ShutDown,
    /// The render panicked: nothing was produced. The panic's message.
    #[error("the bounce panicked: {message}")]
    Panicked {
        /// The panic's message.
        message: String,
    },
    /// The render was made for another session, which cannot take it here.
    #[error("the bounce was rendered for another session")]
    Stale,
}

impl From<tutti_export::RenderError> for BounceError {
    fn from(e: tutti_export::RenderError) -> Self {
        match e {
            tutti_export::RenderError::NotForkable { key } => Self::NotForkable { node: key },
            tutti_export::RenderError::Fork(e) => Self::Fork(e),
            other => Self::Export(other),
        }
    }
}

/// The graph an bounce is about to render, handed to its
/// [`PrepareEdit`] on the job's thread once the fork is built.
///
/// `editor` is the fork's: edit it (insert nodes, wire them with
/// [`connect_edge`](Editor::connect_edge) and the calls beside it); the job
/// commits whatever the edit leaves, and a commit the fork refuses fails the
/// bounce with the reason. A fork's units are on its executor by now, so a
/// unit already in it cannot be refilled: insert a new one. The fork's
/// executor, pool and main output are the job's; the edit does not see them.
#[derive(Debug)]
pub struct PreparedGraph<'a, F: Float = f32> {
    /// The fork's editor.
    pub editor: &'a mut Editor<F>,
    /// The render's timeline, which the renderer advances. A node in the
    /// graph reads it through its block's `Env`; this is for the edit
    /// itself (to place what it inserts, say).
    pub ctx: &'a OfflineTransport,
}

impl<F: Float> PreparedGraph<'_, F> {
    /// A key no node in any graph holds, for a node the edit inserts
    /// (`editor.insert(prepared.fresh_key(), ..)`), minted from
    /// [`NodeKey::fresh`]'s process-wide counter as the live graph's are.
    pub fn fresh_key(&self) -> NodeKey {
        NodeKey::fresh()
    }
}

/// An edit of an bounce's forked graph, made once the fork is built,
/// before the render.
///
/// **It runs on the job's thread**, and so do the `prepare` of every node
/// it inserts and the drop of whatever it captured once it has run. It must
/// not insert, prepare or capture a unit confined to the main thread (an
/// in-process VST2 plugin); such a node goes into the live graph, where the
/// fork's gather builds it on the main thread. An edit that never runs (the
/// build failed, the job was dropped) is dropped on the main thread.
pub type PrepareEdit<F = f32> = Box<dyn for<'a> FnOnce(PreparedGraph<'a, F>) + Send>;

/// How long a bounce may be in flight, on the `now` the session's
/// [`Phase::Collect`](crate::Phase::Collect) is given, before it warns that
/// its token may have leaked: an hour. Generous: an offline render of a
/// long project, with plugin forks that launch servers, takes minutes.
pub(crate) const BOUNCE_STALL_WARNING: Duration = Duration::from_secs(60 * 60);

/// The session's bounce state: the token every bounce holds a clone of,
/// the channel leftovers come back on, and the stall watch.
#[derive(Debug)]
pub(crate) struct BounceState {
    token: Arc<()>,
    send: Sender<Box<dyn Send>>,
    recv: Receiver<Box<dyn Send>>,
    /// The `now` the collect first saw the current bounce in flight.
    since: Option<Instant>,
    /// Whether the current bounce's stall was warned about.
    stall_warned: bool,
    /// How many stalls were warned about: what a test reads.
    #[cfg(test)]
    stall_warnings: usize,
}

impl Default for BounceState {
    fn default() -> Self {
        let (send, recv) = channel();
        Self {
            token: Arc::new(()),
            send,
            recv,
            since: None,
            stall_warned: false,
            #[cfg(test)]
            stall_warnings: 0,
        }
    }
}

impl BounceState {
    /// Whether an bounce token lives.
    pub(crate) fn in_flight(&self) -> bool {
        Arc::strong_count(&self.token) > 1
    }

    /// Watches for a leaked token at `now`: warns once when a bounce has been
    /// in flight (as the collects saw it) for an hour. Its clock restarts when
    /// nothing is in flight, and when a bounce starts
    /// ([`restart_stall_watch`](Self::restart_stall_watch)).
    pub(crate) fn watch_stall(&mut self, now: Instant) {
        if !self.in_flight() {
            self.since = None;
            self.stall_warned = false;
            return;
        }
        let since = *self.since.get_or_insert(now);
        if !self.stall_warned && now.saturating_duration_since(since) >= BOUNCE_STALL_WARNING {
            self.stall_warned = true;
            #[cfg(test)]
            {
                self.stall_warnings += 1;
            }
            tracing::warn!(
                "an bounce has been in flight for over {} minutes: if its job or what it \
                 returned leaked (forgotten, or kept unrun), hosted plugins stay offline \
                 and every later bounce is refused (BounceInFlight)",
                BOUNCE_STALL_WARNING.as_secs() / 60
            );
        }
    }

    /// Restarts the stall clock for a bounce starting now. A host that
    /// hands one bounce's slot to the next within a frame
    /// ([`Session::apply`], then [`Session::bounce`])
    /// never lets a collect see nothing in flight, so without this a queue
    /// of bounces each well inside the bound would be warned about.
    fn restart_stall_watch(&mut self) {
        self.since = None;
        self.stall_warned = false;
    }

    /// Drops, on this thread, every leftover sent back so far.
    pub(crate) fn drop_leftovers(&self) {
        while let Ok(value) = self.recv.try_recv() {
            drop(value);
        }
    }

    /// The bounce's share of the collect: what an export left behind drops
    /// here, on the main thread, and a bounce in flight too long is warned
    /// about once.
    pub(crate) fn collect(&mut self, now: Instant) {
        self.drop_leftovers();
        self.watch_stall(now);
    }
}

/// `T`, sent back to the session wherever it is dropped, unless
/// [`take`](Self::take)n first.
struct ToMain<T: Send + 'static> {
    value: Option<T>,
    to: Sender<Box<dyn Send>>,
}

impl<T: Send + 'static> ToMain<T> {
    fn new(value: T, to: Sender<Box<dyn Send>>) -> Self {
        Self {
            value: Some(value),
            to,
        }
    }

    /// The value, no longer sent anywhere: its new owner drops it.
    fn take(mut self) -> T {
        self.value.take().expect("taken once")
    }

    /// The same value, its type forgotten, still sent back to be dropped on
    /// the main thread.
    fn erase(self) -> ToMain<Box<dyn Send>> {
        let to = self.to.clone();
        ToMain::new(Box::new(self.take()), to)
    }
}

impl<T: Send + 'static> std::ops::Deref for ToMain<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().expect("not taken")
    }
}

impl<T: Send + 'static> std::ops::DerefMut for ToMain<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("not taken")
    }
}

impl<T: Send + 'static> Drop for ToMain<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            // A session already gone hands the value back, and it drops
            // here.
            let _ = self.to.send(Box::new(value));
        }
    }
}

/// A handle on a running bounce: stops it, and reads how far it is. Cheap
/// to clone, `Send` and `Sync`; any host reads the same reading, from any
/// thread, without waiting on the render.
///
/// It wraps the render's [`CancelToken`] and the latest [`Progress`]. The
/// render writes the reading when it delivers one (each of the session's
/// steps, every 4096 frames of a stage that counts them, and every stage
/// change) on its own thread and a host reads it on its own; neither is an
/// audio thread, so a lock is enough.
#[derive(Debug, Clone, Default)]
pub struct BounceHandle {
    cancel: CancelToken,
    progress: Arc<Mutex<Option<Progress>>>,
}

impl BounceHandle {
    /// Asks the render to stop. It does at the next point the session or the
    /// pipeline checks (between the session's steps, every block, between
    /// stages, during the measurement and the conversion, before each file is
    /// finalized and before it is put in place), or
    /// before it starts for a job that has not run. It reports
    /// [`BounceError::Cancelled`] and drops the bounce's token; every temporary
    /// file is removed and the target path is untouched. A cancel after the
    /// file is in place is too late and the render is `Finished`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// How far the render is: the latest reading delivered, or `None` before
    /// the first (the job has not begun to run). Its
    /// [`stage`](Progress::stage) says what the render is doing and its
    /// [`fraction`](Progress::fraction) how far through the whole of it, the
    /// pipeline's own weights.
    #[must_use]
    pub fn progress(&self) -> Option<Progress> {
        *self.progress.lock()
    }

    fn stop_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The render's callback: records the reading.
    fn record(&self, p: &Progress) {
        *self.progress.lock() = Some(*p);
    }
}

/// The figures a bounce derives from the session at its start, and the two it
/// derives from the graph that renders (the latency and the tail), which are
/// asked of it after the edit.
#[derive(Debug, Clone, Copy)]
struct Figures {
    rate: SampleRate,
    /// The audible span, in frames at `rate`.
    length: Samples,
    latency: BounceLatency,
    tail: BounceTail,
    on_event_loss: EventLossPolicy,
    /// What the buffers' target would be too large to hold, for the error
    /// of a span past [`tutti_export::MAX_FRAMES`]: its width.
    held_in_memory: Option<ChannelLayout>,
}

impl Figures {
    /// The [`Job`], from what `graph` reports now.
    fn job<F: Float>(&self, graph: &tutti_export::RenderGraph<F>) -> Result<Job, BounceError> {
        let reported = graph.reported_latency();
        let latency = match self.latency {
            BounceLatency::Graph => reported,
            BounceLatency::Keep => Samples::new(0),
            BounceLatency::Frames(n) => n,
        };
        // A tail excludes latency: the programme leaves the graph its
        // reported latency late, so the part of that latency the render does
        // not trim is rendered and kept as well, or the programme's end is cut.
        let kept = reported.remaining_after(latency);
        let tail = match self.tail {
            BounceTail::None => TailPolicy::Frames(kept),
            BounceTail::Frames(n) => TailPolicy::Frames(n + kept),
            // The most the graph's nodes can ring for, under `cap`: the render
            // spends what they owe when the programme enters the graph, at
            // most this. `resolve`, not `samples().unwrap_or(cap)`: a node
            // that never said (`Tail::Unknown`) is bounded by what the others
            // declared, not by the cap, which would append silence. The
            // untrimmed latency is `kept_latency`: pulled once, after the programme
            // and before the tail.
            BounceTail::Graph { cap } => TailPolicy::Reported {
                ceiling: graph.declared_tail().resolve(cap.to_samples(self.rate)),
                kept_latency: kept,
            },
        };
        self.provisional(latency, tail)
    }

    /// The job of these counts, refused as the bounce reports it.
    fn provisional(&self, latency: Samples, tail: TailPolicy) -> Result<Job, BounceError> {
        match Job::new(self.rate, self.length, latency, tail) {
            Ok(job) => Ok(job.event_loss(self.on_event_loss)),
            // Planes that cannot be held are the pipeline's error, as when
            // the allocation fails.
            Err(JobError::TooLong { total: Some(total) }) if self.held_in_memory.is_some() => {
                Err(tutti_export::RenderError::RenderTooLarge {
                    frames: total,
                    channels: self.held_in_memory.expect("checked"),
                }
                .into())
            }
            Err(e) => Err(BounceConfigError::Job(e).into()),
        }
    }
}

/// How many steps the session reports as [`Stage::Preparing`](tutti_export::Stage::Preparing): the gather
/// (done when the job is made), the fork's build, the edit, the commit and
/// assembly.
const PREPARING_STEPS: u32 = 4;

/// A bounce, gathered on the control thread: `Send`, and a [`Future`] through
/// [`IntoFuture`]. Awaiting it ([`Bouncing`]) builds the fork (a hosted
/// plugin's fork launches a `plugin-server` and loads its state), runs the edit
/// and renders, as a blocking task of the session's executor, and yields a
/// [`BounceRendered`] to hand to [`Session::apply`]. There is no blocking form.
///
/// Holds the bounce's token: dropping it unawaited ends the bounce (the next
/// update announces Realtime), and what it holds goes back to the session to
/// drop. It holds no strong handle to the session's executor.
///
/// **Do not leak it.** A job (or its [`BounceRendered`]) that is
/// `mem::forget`ed, or kept unawaited forever, keeps its token: the bounce
/// stays in flight, hosted plugins stay Offline, and every later
/// [`Session::bounce`] is refused with [`BounceError::InFlight`].
/// Nothing else reports it; the session warns once after an hour.
#[must_use = "a bounce renders nothing until it is awaited"]
pub struct BounceJob<D: Destination, F: FftFloat = f32> {
    cut: Cut,
    executor: WeakExecutor,
    pending: ToMain<PendingExport<F>>,
    edit: Option<ToMain<PrepareEdit<F>>>,
    ctx: OfflineTransport,
    clock: Arc<dyn RenderClock>,
    main_output: D::Target,
    /// What each stem taps and where it goes, in order.
    stems: Vec<StemTap<F>>,
    figures: Figures,
    drops: Sender<Box<dyn Send>>,
    token: Arc<()>,
    handle: BounceHandle,
    kind: PhantomData<fn() -> D>,
}

impl<D: Destination, F: FftFloat> std::fmt::Debug for BounceJob<D, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BounceJob")
            .field("ctx", &self.ctx)
            .field("stems", &self.stems.len())
            .field("figures", &self.figures)
            .field("drops", &self.drops)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl<D: Destination, F: FftFloat> BounceJob<D, F> {
    /// The handle that cancels this job and reads its progress.
    #[must_use]
    pub fn handle(&self) -> BounceHandle {
        self.handle.clone()
    }

    /// Builds the fork, runs the edit, commits it, and renders, on this thread:
    /// the one the render task runs on. It blocks for the whole render, or
    /// until [`BounceHandle::cancel`] stops it.
    fn render(self) -> BounceRendered<D, F> {
        let Self {
            cut,
            executor: _,
            pending,
            edit,
            ctx,
            clock,
            main_output,
            stems,
            figures,
            drops,
            token,
            handle,
            kind: _,
        } = self;
        let token = Held { cut, _token: token };
        let record = &mut |p: &Progress| handle.record(p);
        let mut control = Control::new(&handle.cancel).on_progress(record);
        // The gather was the first step, made when the job was.
        // The band the render leaves for the session's steps depends on the
        // files alone: planes in memory never normalize.
        let files = || {
            std::iter::once(D::landing(&main_output))
                .chain(stems.iter().map(|stem| stem.output.landing()))
                .filter_map(|landing| match landing {
                    Landing::File(file) => Some(file),
                    Landing::Buffers(_) => None,
                })
        };
        control.preparing(files(), 1, PREPARING_STEPS);
        if handle.stop_requested() {
            // Cancelled before it began: nothing is built, and what the job
            // holds goes back to the main thread.
            drop((pending, edit));
            return BounceRendered::cancelled(None, token);
        }
        let mut pair = match pending.take().build() {
            Ok(pair) => ToMain::new(pair, drops.clone()),
            Err((e, remains)) => {
                drop(ToMain::new(remains, drops));
                return BounceRendered::failed(e.into(), None, token);
            }
        };
        control.preparing(files(), 2, PREPARING_STEPS);
        if handle.stop_requested() {
            // Cancelled while the fork was building (a plugin's server
            // launches here, half a second or more).
            drop(edit);
            return BounceRendered::cancelled(Some(pair.erase()), token);
        }
        if let Some(edit) = edit {
            (edit.take())(PreparedGraph {
                editor: pair.editor_mut(),
                ctx: &ctx,
            });
        }
        // One tap per stem, on the editor the commit below installs.
        let readers = match open_stem_taps(pair.editor_mut(), &stems) {
            Ok(readers) => readers,
            Err(e) => {
                return BounceRendered::failed(e.into(), Some(pair.erase()), token);
            }
        };
        control.preparing(files(), 3, PREPARING_STEPS);
        // A fork's editor holds whatever the edit left, uncommitted: send
        // it, and apply it here so the render's first block runs it.
        if let Err(e) = pair.commit() {
            return BounceRendered::failed(
                BounceConfigError::PrepareHookUncommittable(e).into(),
                Some(pair.erase()),
                token,
            );
        }
        // The edit held `&mut Editor`, which can be swapped for another
        // graph's: seal only a pair that is still one, and keep an unpaired
        // one to drop on the main thread, where a pinned unit belongs.
        if !pair.is_paired() {
            return BounceRendered::failed(
                BounceConfigError::PrepareHookUnpaired.into(),
                Some(pair.erase()),
                token,
            );
        }
        let mut graph = match pair.take().assemble() {
            Ok(graph) => ToMain::new(graph, drops),
            // Not reached: the pair was just checked.
            Err(e) => return BounceRendered::failed(e.into(), None, token),
        };
        control.preparing(files(), 4, PREPARING_STEPS);
        if handle.stop_requested() {
            return BounceRendered::cancelled(Some(graph.erase()), token);
        }
        // The graph's own figures, asked of the graph that renders: after the
        // edit, which may have changed it.
        let job = match figures.job(&graph) {
            Ok(job) => job,
            Err(e) => return BounceRendered::failed(e, Some(graph.erase()), token),
        };
        let mut outputs = Outputs::new();
        let main_key = D::add(&mut outputs, Output::main_output(main_output));
        let mut stem_keys = Vec::with_capacity(stems.len());
        let main_file = D::file_key(&main_key);
        for (reader, stem) in readers.into_iter().zip(stems) {
            match stem
                .output
                .add(&mut outputs, reader, stem.placement, main_file)
            {
                Ok(key) => stem_keys.push(key),
                Err(e) => return BounceRendered::failed(e.into(), Some(graph.erase()), token),
            }
        }
        let result = tutti_export::render(&mut graph, &job, clock.as_ref(), outputs, &mut control)
            .map_err(BounceError::from)
            .and_then(|outcome| match outcome {
                Outcome::Finished(done) => Bounced::new(done, &main_key, stem_keys)
                    .map_err(|e| BounceError::from(tutti_export::RenderError::from(e))),
                Outcome::Cancelled => Err(BounceError::Cancelled),
            });
        BounceRendered {
            result,
            held: token,
            graph: Some(graph.erase()),
        }
    }
}

impl<D: Destination, F: FftFloat> IntoFuture for BounceJob<D, F> {
    type Output = BounceRendered<D, F>;
    type IntoFuture = Bouncing<D, F>;

    fn into_future(self) -> Bouncing<D, F> {
        let held = Held {
            cut: self.cut,
            _token: Arc::clone(&self.token),
        };
        Bouncing {
            handle: self.handle.clone(),
            held: Some(held),
            render: Unblocked::new(
                self.executor.clone(),
                Box::new(self),
                |job| Ok(job.render()),
            ),
        }
    }
}

/// What a bounce holds of the session until it is applied: the session it was
/// cut from, and the bounce's token.
#[derive(Debug)]
struct Held {
    cut: Cut,
    _token: Arc<()>,
}

impl Unanswered for BounceError {}

impl crate::unblocked::sealed::Sealed for BounceError {
    fn shut_down() -> Self {
        Self::ShutDown
    }

    fn panicked(message: String) -> Self {
        Self::Panicked { message }
    }
}

/// A bounce being awaited ([`BounceJob`]): resolves to the [`BounceRendered`]
/// once the render has ended.
///
/// The first poll starts the render as a blocking task of the session's executor
/// (the session's weak handle, upgraded for the spawn only), on a thread that
/// drives no queue (the fork's plugin servers wait on such a thread), and awaits
/// the receiver of a oneshot. **The render task fills the sender**: it builds
/// the fork, renders, and sends the result when the render ends, which wakes the
/// future. Nothing else drives it and no caller waits on code the app does not
/// run: a poll never blocks, and `Session::update` is not needed.
///
/// Dropped before it resolves, the future cancels the render
/// ([`BounceHandle::cancel`]): the render stops at its next check, finds its
/// answer unwanted, and its token and leftovers go back as for any dropped
/// return. A render that has not begun never starts.
///
/// The output carries the failure by name when the render could not answer
/// ([`BounceError::ShutDown`] for a session that is gone, a thread the OS
/// refused or an executor that dropped the render before it ran,
/// [`BounceError::Panicked`] for a render that panicked); apply it to read it.
///
/// # Panics
///
/// When polled after it resolved.
#[must_use = "a bounce renders nothing unless it is awaited"]
pub struct Bouncing<D: Destination, F: FftFloat = f32> {
    render: Unblocked<Box<BounceJob<D, F>>, BounceRendered<D, F>, BounceError>,
    /// What the answer carries when the render gives none; taken on resolving.
    held: Option<Held>,
    handle: BounceHandle,
}

impl<D: Destination, F: FftFloat> std::fmt::Debug for Bouncing<D, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bouncing").finish_non_exhaustive()
    }
}

impl<D: Destination, F: FftFloat> Future for Bouncing<D, F> {
    type Output = BounceRendered<D, F>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let answered = std::task::ready!(Pin::new(&mut this.render).poll(cx));
        let held = this.held.take().expect("held until it resolves");
        Poll::Ready(answered.unwrap_or_else(|e| BounceRendered::failed(e, None, held)))
    }
}

impl<D: Destination, F: FftFloat> Drop for Bouncing<D, F> {
    fn drop(&mut self) {
        if self.held.is_some() {
            self.handle.cancel();
        }
    }
}

/// What one stem taps, and what its lanes are: the part of a [`Stem`] the
/// fork's editor needs to open its tap, and the output it becomes.
#[derive(Debug)]
struct StemTap<F: Float> {
    node: NodeKey,
    /// The node's output ports, one lane each.
    ports: Vec<u16>,
    placement: Placement,
    output: Box<dyn StemOutput<F>>,
}

/// Opens one plan tap per stem on the fork's editor, aligned to the outputs'
/// time, each lane with the ring the pipeline's drain needs (`2 x` the
/// fork's block), and returns their readers in order. The commit that follows
/// installs them.
///
/// The ids are above any the edit hook opened. A node or channel the edit
/// removed since the start's check is refused here, by the same names.
fn open_stem_taps<F: Float>(
    editor: &mut Editor<F>,
    stems: &[StemTap<F>],
) -> Result<Vec<tutti_graph::TapReader<F>>, BounceConfigError> {
    // The fork's topology is isolated and never merges back, so its ids are
    // numbered above the ones it holds instead of minted from a counter.
    let mut next = editor
        .spec()
        .topology
        .taps
        .keys()
        .next_back()
        .map_or(1, |id| id.get() + 1);
    let capacity = GRAPH_MAX_BLOCK.samples() * 2;
    stems
        .iter()
        .map(|stem| {
            let sources = stem
                .ports
                .iter()
                .map(|&port| tutti_graph::PortSource::Node(OutputPort::new(stem.node, port)))
                .collect();
            let id = TapId::of(next);
            next += 1;
            editor
                .open_tap(id, sources, TapOptions::new(capacity))
                .map_err(|missing| match missing {
                    MissingSource::Port { node, port, .. } => {
                        BounceConfigError::StemChannelMissing {
                            node,
                            channel: port,
                        }
                    }
                    MissingSource::Node { node } => BounceConfigError::StemNotInGraph { node },
                    // A stem's lanes are node ports, nothing else.
                    other => unreachable!("a stem taps node ports, not {other:?}"),
                })
        })
        .collect()
}

/// What an awaited [`BounceJob`] resolves to: the result, the rendered graph
/// (dropped on the main thread by [`Session::apply`]), and the bounce's token.
/// `Send`. [`Session::apply`] takes it back and gives the [`Bounced`] or the
/// error the render ended in.
///
/// Applying releases the bounce's token but announces nothing: hosted plugins
/// stay Offline until the next [`update`](Session::update)'s
/// [`Phase::RenderMode`](crate::Phase::RenderMode) tells them Realtime. An app
/// that plays live after a bounce updates right after applying.
#[must_use = "a bounce is finished only when the session applies its render"]
pub struct BounceRendered<D: Destination, F: Float = f32> {
    result: Result<Bounced<D, F>, BounceError>,
    /// Declared before `graph`: dropping this releases the token, then the graph
    /// goes back to the session, so a session that finds the graph back finds the
    /// bounce no longer in flight.
    held: Held,
    /// The fork, sealed or not: whatever it holds is dropped on the main thread.
    graph: Option<ToMain<Box<dyn Send>>>,
}

impl<D: Destination, F: Float> BounceRendered<D, F> {
    fn cancelled(graph: Option<ToMain<Box<dyn Send>>>, held: Held) -> Self {
        Self::failed(BounceError::Cancelled, graph, held)
    }

    fn failed(e: BounceError, graph: Option<ToMain<Box<dyn Send>>>, held: Held) -> Self {
        Self {
            result: Err(e),
            held,
            graph,
        }
    }
}

impl<D: Destination, F: SessionFloat> ApplySealed<F> for BounceRendered<D, F> {
    type Applied = Bounced<D, F>;
    type Error = BounceError;

    fn cut(&self) -> &Cut {
        &self.held.cut
    }

    fn stale() -> BounceError {
        BounceError::Stale
    }

    /// Drops the rendered graph (and every leftover sent back so far) here, which
    /// closes an in-process plugin in it on the main thread, and releases the
    /// bounce's token. It does not announce the render mode: the next
    /// [`Phase::RenderMode`](crate::Phase::RenderMode) tells the plugins Realtime
    /// once no bounce is in flight.
    fn apply_to<K: crate::AppKey, C: crate::Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<Bounced<D, F>, BounceError> {
        let Self {
            result,
            held,
            graph,
        } = self;
        if let Some(graph) = graph {
            drop(graph.take());
        }
        session.bounce.drop_leftovers();
        drop(held);
        result
    }
}

impl<D: Destination, F: Float> std::fmt::Debug for BounceRendered<D, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BounceRendered")
            .field(
                "result",
                &match &self.result {
                    Ok(b) => format!("Ok({} stems)", b.stems().len()),
                    Err(BounceError::Cancelled) => "Cancelled".to_string(),
                    Err(_) => "Err".to_string(),
                },
            )
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

/// The range a bounce renders: the one it names, else the transport's loop;
/// never a guess.
fn chosen(
    range: Option<Range>,
    looped: Option<tutti_music::LoopRange>,
) -> Result<Range, BounceConfigError> {
    range
        .or_else(|| looped.map(|_| Range::Loop))
        .ok_or(BounceConfigError::NoRange)
}

/// The span `range` is on `map`: the beat the render starts at and its length
/// in frames at `rate`.
///
/// Beats go through the map's `frame_at_or_after`, as the walker places
/// them (doc 020, section 6), so a step or a ramp inside the span is the
/// length it has in playback.
fn span(
    range: Range,
    loop_range: Option<tutti_music::LoopRange>,
    map: &TempoMap,
    rate: SampleRate,
) -> Result<(Beat, Samples), BounceConfigError> {
    let beats = |a: Beat, b: Beat| {
        if !a.get().is_finite() || !b.get().is_finite() || a.get() < 0.0 || b.get() < 0.0 {
            return Err(BounceConfigError::InvalidRange);
        }
        let (from, to) = (
            map.view(rate).frame_at_or_after(a),
            map.view(rate).frame_at_or_after(b),
        );
        Ok((a, to.get().saturating_sub(from.get())))
    };
    let (start, frames) = match range {
        Range::Loop => {
            let looped = loop_range.ok_or(BounceConfigError::NoRange)?;
            beats(looped.start(), looped.end())?
        }
        Range::Until(end) => beats(Beat::new(0.0), end)?,
        Range::Beats(a, b) => beats(a, b)?,
        Range::Seconds(a, b) => {
            if !a.get().is_finite() || !b.get().is_finite() || a.get() < 0.0 || b.get() < 0.0 {
                return Err(BounceConfigError::InvalidRange);
            }
            let long = (b - a).max(Seconds::new(0.0)).to_samples(rate);
            (map.beat_at_seconds(a), long.get() as u64)
        }
        Range::Frames(a, b) => (
            map.view(rate).beat_at_frame(a),
            b.get().saturating_sub(a.get()),
        ),
    };
    match usize::try_from(frames) {
        Ok(0) => Err(BounceConfigError::EmptyRange),
        Ok(n) => Ok((start, Samples::new(n))),
        Err(_) => Err(BounceConfigError::Job(JobError::TooLong { total: None })),
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C>
where
    C::Float: FftFloat,
{
    /// Starts a bounce: checks what it names, derives what it does not,
    /// gathers its fork of the live graph here (what it reads of the live graph
    /// and its nodes, now; an in-process VST2 plugin's second instance is built
    /// here too), mints the bounce's token, and (with `plugin`) announces
    /// Offline to every hosted plugin before it returns. Await the job (any
    /// thread: the render runs on a thread of its own), then
    /// [`apply`](Self::apply) the [`BounceRendered`] it resolves to.
    ///
    /// **What a fork keeps of the live inputs.** A live audio input (an inlet,
    /// `add_input`) cannot be copied: its frames come
    /// from a thread of its own, so a bounce whose outputs hear one is refused,
    /// naming its node ([`BounceError::NotForkable`]). Live keys, hardware MIDI
    /// and the clock out fork silent: the render is driven from its clock, which
    /// has no events waiting. A `VoicePool` forks
    /// only while its handle has queued nothing, and is refused otherwise
    /// ([`BounceError::Fork`], naming its node).
    ///
    /// **No `update` is needed first.** The fork is the graph's spec as it
    /// stands at this call, so an insert, a wire or a mode set since the last
    /// [`update`](Self::update) is in it, committed or not (the live executor
    /// is not touched). What `update` derives from the session's own
    /// declarations (modulation lanes, a clip's node) reaches the graph in its
    /// phases, so a bounce after declaring those wants an `update` first.
    ///
    /// The fork renders what the graph is driven to play from the bounce's
    /// clock, from silence: every node isolated and reset, reading that
    /// clock through its `Env`; controls as they are now; a hosted plugin
    /// forked by state transfer (a fresh instance in a new `plugin-server`,
    /// loaded with the live one's state).
    ///
    /// # Errors
    ///
    /// [`BounceError::InFlight`] while another bounce's token lives;
    /// [`BounceError::Config`] for a request that names no
    /// range, whose range keeps no frame, or for a graph with no outputs (all
    /// before anything is forked); [`BounceError::NotForkable`] and
    /// [`BounceError::Fork`] for a node the fork cannot copy.
    pub fn bounce<D: Destination>(
        &mut self,
        bounce: Bounce<NodeKey, D, C::Float>,
    ) -> Result<BounceJob<D, C::Float>, BounceError> {
        if self.bounce.in_flight() {
            return Err(BounceError::InFlight);
        }
        // The fork renders the project as set: the staged tempo, meter and
        // loop are published first.
        self.values.publish_pending();
        self.graph.apply_controls();
        let Bounce {
            main_output,
            stems,
            range,
            clock,
            sample_rate,
            latency,
            tail,
            main_output_topology,
            serial,
            on_event_loss,
            normalize,
            edit,
        } = bounce;
        if self.graph.outputs() == 0 {
            return Err(BounceConfigError::GraphHasNoOutputs.into());
        }
        // The width the main output has without being told: the graph's.
        let outputs = self.graph.outputs();
        let rate = sample_rate.unwrap_or_else(|| self.transport.sample_rate());

        // The span: the range named, else the transport's loop, else the end
        // the bounce names; never a guess.
        let map = self.transport.bases.tempo.get();
        // The loop as set: a staged or just sent region counts, not only the
        // one the engine has applied.
        let looped = self
            .values
            .get(self.transport.handle.looping())
            .unwrap_or_else(|_| self.transport.motion().loop_region())
            .active();
        let range = chosen(range, looped)?;
        let (start, length) = span(range, looped, &map, rate)?;

        let target = main_output.target(MainShape {
            channels: ChannelLayout::clamped(outputs),
            normalize: normalize.map(Gain::Own),
        });
        // The stems: what each taps and where it goes, refused by name before
        // anything is forked. The main output is output 0, which `Shared` reads.
        let mut taps = Vec::with_capacity(stems.len());
        for (i, stem) in stems.into_iter().enumerate() {
            let StemRequest {
                node,
                channels: tapped,
                topology,
                gain,
                output,
            } = stem;
            if !self.graph.contains(node) {
                return Err(BounceConfigError::StemNotInGraph { node }.into());
            }
            if !self.graph.reaches_outputs(node) {
                return Err(BounceConfigError::StemNotAudible { node }.into());
            }
            let width = self
                .graph
                .node_outputs(node)
                .map_err(|_| BounceConfigError::StemNotInGraph { node })?;
            let ports = tapped.unwrap_or_else(|| (0..width as u16).collect());
            if ports.is_empty() {
                return Err(BounceConfigError::StemChannelMissing { node, channel: 0 }.into());
            }
            if let Some(&channel) = ports.iter().find(|&&c| usize::from(c) >= width) {
                return Err(BounceConfigError::StemChannelMissing { node, channel }.into());
            }
            let lanes = ChannelLayout::from_count(ports.len().try_into().unwrap_or(u16::MAX));
            let placement = match topology {
                Some(topology) => Placement::new(topology),
                None => Placement::of_width(lanes),
            };
            if gain == StemGain::Shared && normalize.is_none() {
                return Err(BounceConfigError::SharedGainWithoutMainOutput.into());
            }
            taps.push(StemTap {
                node,
                ports,
                placement,
                // The main output is output 0.
                output: output(StemShape {
                    lanes,
                    index: i + 1,
                    gain,
                }),
            });
        }
        // Two files at one path would each rename over the other.
        let all = || {
            std::iter::once(D::landing(&target))
                .chain(taps.iter().map(|stem| stem.output.landing()))
        };
        for (i, one) in all().enumerate() {
            if let Landing::File(file) = one
                && all()
                    .take(i)
                    .any(|t| matches!(t, Landing::File(f) if f.path() == file.path()))
            {
                return Err(BounceConfigError::DuplicatePath(file.path().to_path_buf()).into());
            }
        }
        // What a span past the frame limit would be too large to hold: the
        // first target kept in memory.
        let held_in_memory = all().find_map(|t| match t {
            Landing::Buffers(b) => Some(b.channels()),
            Landing::File(_) => None,
        });
        let figures = Figures {
            rate,
            length,
            latency,
            tail,
            on_event_loss,
            held_in_memory,
        };
        // What the counts the bounce names make of the job, refused now rather
        // than after the fork is built; the graph's own figures are added
        // once it is.
        figures.provisional(
            match latency {
                BounceLatency::Frames(n) => n,
                BounceLatency::Graph | BounceLatency::Keep => Samples::new(0),
            },
            match tail {
                BounceTail::Frames(n) => TailPolicy::Frames(n),
                BounceTail::Graph { .. } | BounceTail::None => TailPolicy::Frames(Samples::ZERO),
            },
        )?;

        // The render's time: the bounce's clock, or the project's tempo map
        // from the range's first beat with the loop off, so a transport-driven
        // node reads what playback gives it.
        let clock = clock.unwrap_or_else(|| {
            ExportClock::timeline(Arc::new(
                OfflineTimeline::new(&OfflineTimelineConfig {
                    start_beat: start,
                    tempo_map: Arc::clone(&map),
                    sample_rate: rate,
                    loop_range: None,
                })
                .meter(self.transport.bases.meter.get()),
            ))
        });
        // The edit hook's timeline: the same object the renderer advances, or
        // one stopped at beat 0 for a frozen clock.
        let ctx = clock.offline();
        let pending = match self
            .graph
            .gather_for_bounce(rate, serial, main_output_topology)
        {
            Ok(pending) => pending,
            Err(tutti_export::RenderError::Fork(
                tutti_graph::ForkError::NoOutputs { .. }
                | tutti_graph::ForkError::NoSuchNode { .. },
            )) => {
                return Err(BounceConfigError::GraphHasNoOutputs.into());
            }
            Err(e) => return Err(e.into()),
        };
        // A new bounce: its own stall clock, from its first collect.
        self.bounce.restart_stall_watch();
        let drops = self.bounce.send.clone();
        let job = BounceJob {
            cut: Cut::of(self),
            executor: self.background.executor().downgrade(),
            pending: ToMain::new(pending, drops.clone()),
            edit: edit.map(|edit| ToMain::new(edit, drops.clone())),
            ctx,
            clock: clock.render_clock(),
            main_output: target,
            stems: taps,
            figures,
            drops,
            token: Arc::clone(&self.bounce.token),
            handle: BounceHandle::default(),
            kind: PhantomData,
        };
        // Offline is announced here, on the control thread, before the job
        // can render a block.
        C::Float::announce_render_mode(self);
        Ok(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputPort, MainOutput, Phase, PhaseCx};
    use tutti_export::Stage;
    use tutti_fixtures::block_on;
    use tutti_graph::ForkByClone;
    use tutti_tempo::{TempoPoint, Transition};
    use tutti_units::{Bpm, SampleRate};

    const RATE: SampleRate = SampleRate::SR_48K;

    /// A render of a few blocks into memory.
    fn short() -> Bounce<NodeKey, ToMemory, f32> {
        Bounce::new()
            .main_output(ToMemory::new())
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.01)))
    }

    /// A detached session whose outputs hear a dc node: something to bounce.
    fn session() -> crate::DetachedSession {
        let mut session = Session::builder().build_detached(RATE).unwrap();
        let dc = session
            .graph()
            .add(ForkByClone(tutti_nodes::testing::Const::mono(0.0)));
        session.graph().connect(dc, MainOutput).unwrap();
        session
    }

    /// Starts `bounce`, awaits it and applies the render.
    fn bounced<M: Destination>(
        session: &mut crate::DetachedSession,
        bounce: Bounce<NodeKey, M, f32>,
    ) -> Result<Bounced<M>, BounceError> {
        let rendered = block_on(session.bounce(bounce)?);
        session.apply(rendered)
    }

    /// The planes of a finished buffers bounce.
    fn planes(out: Result<Bounced<ToMemory>, BounceError>) -> Vec<Vec<f32>> {
        out.expect("renders").into_main_output().into_planes()
    }

    /// **A bounce renders what was edited, with no `update` between.** A node
    /// inserted and wired since the last update is in the fork: the planes
    /// hold its level, not silence.
    #[test]
    fn a_bounce_renders_an_edit_no_update_has_committed() {
        let mut session = session();
        let half = session
            .graph()
            .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
        session.graph().connect(half, MainOutput).unwrap();
        assert!(session.graph().has_uncommitted_edits(), "no update has run");
        let out = planes(bounced(&mut session, short()));
        assert!(out[0].iter().all(|&s| s == 0.5), "the edit is heard");
    }

    /// **`Beat::new(a)..Beat::new(b)` is the range from beat `a` to beat `b`**, and the
    /// same for seconds and frames.
    #[test]
    fn a_rust_range_is_a_range() {
        assert_eq!(
            Range::from(Beat::new(0.0)..Beat::new(8.0)),
            Range::Beats(Beat::new(0.0), Beat::new(8.0))
        );
        assert_eq!(
            Range::from(Seconds::new(1.0)..Seconds::new(2.0)),
            Range::Seconds(Seconds::new(1.0), Seconds::new(2.0))
        );
        assert_eq!(
            Range::from(Frame::new(0)..Frame::new(48_000)),
            Range::Frames(Frame::new(0), Frame::new(48_000))
        );
    }

    /// 120 BPM for four beats, then 60: a step at beat 4, which is second 2.
    fn stepped() -> TempoMap {
        TempoMap::new([
            TempoPoint {
                beat: Beat::new(0.0),
                tempo: Bpm::of(120.0),
                transition: Transition::Step,
            },
            TempoPoint {
                beat: Beat::new(4.0),
                tempo: Bpm::of(60.0),
                transition: Transition::Step,
            },
        ])
        .expect("two steps")
    }

    /// **A range in beats is the frames the map puts between them.** At 120
    /// BPM and 48 kHz a beat is 24 000 frames, so beats 1 to 3 are 48 000; over
    /// a step to 60 BPM at beat 4 (second 2), beats 3 to 6 are one beat at 120
    /// (24 000) and two at 60 (96 000), 120 000 frames. The render starts on
    /// the range's first beat. Written out from the definition, not from the
    /// conversion's code.
    #[test]
    fn a_range_in_beats_is_the_frames_the_map_puts_between_them() {
        let constant = TempoMap::constant(Bpm::of(120.0)).unwrap();
        let (start, length) = span(
            Range::Beats(Beat::new(1.0), Beat::new(3.0)),
            None,
            &constant,
            RATE,
        )
        .unwrap();
        assert_eq!((start, length), (Beat::new(1.0), Samples::new(48_000)));
        let (start, length) = span(
            Range::Beats(Beat::new(3.0), Beat::new(6.0)),
            None,
            &stepped(),
            RATE,
        )
        .unwrap();
        assert_eq!((start, length), (Beat::new(3.0), Samples::new(120_000)));
    }

    /// **Seconds and frames name the same span as the beats they are.** Two
    /// seconds from second 1 is 96 000 frames, starting at beat 2 at 120 BPM;
    /// frames 48 000 to 144 000 are the same span. The duration is `f64`
    /// seconds, rounded once to the nearest frame: 2000 beats at 93 BPM is
    /// 61 935 484 frames, where an `f32` second count lands on another frame.
    #[test]
    fn seconds_and_frames_are_the_span_they_name() {
        let map = TempoMap::constant(Bpm::of(120.0)).unwrap();
        let by_seconds = span(
            Range::Seconds(Seconds::new(1.0), Seconds::new(3.0)),
            None,
            &map,
            RATE,
        );
        let by_frames = span(
            Range::Frames(Frame::new(48_000), Frame::new(144_000)),
            None,
            &map,
            RATE,
        );
        assert_eq!(by_seconds, Ok((Beat::new(2.0), Samples::new(96_000))));
        assert_eq!(by_frames, by_seconds);
        let long = (2000.0f64 / 93.0) * 60.0;
        assert_eq!(
            span(
                Range::Seconds(Seconds::new(0.0), Seconds::new(long)),
                None,
                &map,
                RATE
            )
            .unwrap()
            .1,
            Samples::new(61_935_484)
        );
    }

    /// **A range that is not a span, or keeps no frame, is refused by name.**
    /// Not a length (NaN, infinity, before the start) is `InvalidRange`;
    /// inverted, empty, or under half a frame is `EmptyRange`; the loop with
    /// none set is `NoRange`; the transport's loop is its beats.
    #[test]
    fn a_range_that_is_not_a_span_is_refused() {
        let map = TempoMap::constant(Bpm::of(120.0)).unwrap();
        let go = |r| span(r, None, &map, RATE);
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(
                go(Range::Seconds(Seconds::new(0.0), Seconds::new(bad))),
                Err(BounceConfigError::InvalidRange),
                "{bad}"
            );
            assert_eq!(
                go(Range::Beats(Beat::new(0.0), Beat::new(bad))),
                Err(BounceConfigError::InvalidRange),
                "{bad}"
            );
        }
        assert_eq!(
            go(Range::Beats(Beat::new(4.0), Beat::new(2.0))),
            Err(BounceConfigError::EmptyRange)
        );
        assert_eq!(
            go(Range::Seconds(Seconds::new(1.0), Seconds::new(1.0))),
            Err(BounceConfigError::EmptyRange)
        );
        // Under half a frame rounds to nothing.
        assert_eq!(
            go(Range::Seconds(
                Seconds::new(0.0),
                Seconds::new(0.4 / 48_000.0)
            )),
            Err(BounceConfigError::EmptyRange)
        );
        assert_eq!(go(Range::Loop), Err(BounceConfigError::NoRange));
        let looped = tutti_music::LoopRange::new(Beat::new(2.0), Beat::new(4.0));
        assert_eq!(
            span(Range::Loop, looped, &map, RATE),
            Ok((Beat::new(2.0), Samples::new(48_000)))
        );
    }

    /// **Nothing is guessed, and nothing is half-started.** A bounce with no
    /// range, loop or end is refused by name before anything is forked: no
    /// token is held after.
    #[test]
    fn a_bounce_that_misses_what_it_needs_is_refused_before_it_forks() {
        let mut session = session();
        fn refused<M: Destination>(
            session: &mut crate::DetachedSession,
            b: Bounce<NodeKey, M, f32>,
        ) -> BounceConfigError {
            match session.bounce(b) {
                Err(BounceError::Config(e)) => e,
                other => panic!("expected a config error, got {other:?}"),
            }
        }
        assert_eq!(
            refused(&mut session, Bounce::new().main_output(ToMemory::new())),
            BounceConfigError::NoRange
        );
        assert!(!session.bounce.in_flight());
    }

    /// **The range is the one named, else the loop, else none.** An explicit
    /// range wins over a set loop; `Range::Until` is the span from beat 0 to
    /// its end (two beats at 120 BPM and 48 kHz are 48 000 frames).
    #[test]
    fn the_range_is_named_then_the_loop() {
        let looped = tutti_music::LoopRange::new(Beat::new(1.0), Beat::new(2.0));
        let named = Range::Seconds(Seconds::new(0.0), Seconds::new(1.0));
        assert_eq!(chosen(Some(named), looped), Ok(named));
        assert_eq!(chosen(None, looped), Ok(Range::Loop));
        assert_eq!(chosen(None, None), Err(BounceConfigError::NoRange));

        let mut session = session();
        let until = Bounce::new()
            .main_output(ToMemory::new())
            .range(Range::Until(Beat::new(2.0)));
        assert_eq!(planes(bounced(&mut session, until))[0].len(), 48_000);
    }

    /// **The session's steps are reported as `Preparing`, in the band the
    /// render leaves it.** The edit runs after the fork is built (step 2 of
    /// 4) and its hook reads the handle: a streaming bounce's band is 0 to
    /// 0.05, so the reading is `Preparing` at 0.025; with a normalize the band
    /// is 0 to 0.03. After the render the reading is the last one, 1.0.
    #[test]
    fn the_sessions_steps_are_reported_as_preparing() {
        let mut session = session();
        let seen: Arc<Mutex<Vec<Progress>>> = Arc::default();
        let job_handle: Arc<Mutex<Option<BounceHandle>>> = Arc::default();
        let (into, handle) = (Arc::clone(&seen), Arc::clone(&job_handle));
        let job = session
            .bounce(short().edit(Box::new(move |_: PreparedGraph<'_>| {
                let handle = handle.lock().clone().expect("set before run");
                into.lock().extend(handle.progress());
            })))
            .expect("starts");
        let handle = job.handle();
        assert!(handle.progress().is_none(), "nothing before the job runs");
        *job_handle.lock() = Some(handle.clone());
        let rendered = block_on(job);
        drop(session.apply(rendered).unwrap());
        let at_edit = seen.lock()[0];
        assert_eq!(at_edit.stage(), Stage::Preparing);
        assert!((at_edit.fraction() - 0.025).abs() < 1e-6, "{at_edit:?}");
        assert_eq!(at_edit.frames(), None);
        let last = handle.progress().expect("reported");
        assert_eq!(last.stage(), Stage::Finalizing);
        assert_eq!(last.fraction(), 1.0);
    }

    /// **An applied bounce leaves the mode to the next `Phase::RenderMode`**:
    /// the apply releases the token and tells no plugin, so a bounce started
    /// before the phase sees Offline throughout, and the phase after the last
    /// apply announces Realtime. No plugin is needed: the mode asserted is the
    /// one the session announced.
    #[cfg(feature = "plugin")]
    #[test]
    fn an_applied_bounce_leaves_the_mode_to_the_phase() {
        use tutti_plugin_types::RenderMode;
        let mut session = session();
        let job = session.bounce(short()).unwrap();
        assert_eq!(session.render_mode(), RenderMode::Offline);
        session.apply(block_on(job)).unwrap();
        assert!(!session.bounce.in_flight(), "the token is released");
        assert_eq!(session.render_mode(), RenderMode::Offline, "not announced");
        // The next bounce starts with no Realtime in between.
        let job = session.bounce(short()).unwrap();
        assert_eq!(session.render_mode(), RenderMode::Offline);
        session.apply(block_on(job)).unwrap();
        session.run_phase(
            Phase::RenderMode,
            PhaseCx::at(Instant::now()),
            &mut Vec::new(),
        );
        assert_eq!(
            session.render_mode(),
            RenderMode::Realtime,
            "the phase announces"
        );
    }

    /// **An untrimmed bounce with the graph's tail keeps the programme's
    /// end.** A tail excludes latency, so a constant through a lookahead
    /// limiter (240 frames at 48 kHz, no tail) leaves the graph 240 frames
    /// late: rendered untrimmed, the buffers are 480 + 240 frames, the
    /// first 240 silent and every one after them programme. Trimmed (the
    /// default), they are the 480 frames of programme alone.
    #[test]
    fn an_untrimmed_bounce_renders_the_latency_past_the_programme() {
        use tutti_units::Db;
        const DURATION: usize = 480;
        const LATENCY: usize = 240;
        let mut session = session();
        let dc = session
            .graph()
            .add(ForkByClone(tutti_nodes::testing::Const::mono(0.25)));
        let lim = session.graph().add(tutti_nodes::LimiterNode::<f32>::from(
            tutti_nodes::LimiterConfig::new(tutti_buffers::ChannelTopology::MONO).controls(
                tutti_nodes::LimiterControls {
                    threshold: Db::new(-6.0),
                    ceiling: Db::new(-1.0),
                    ..tutti_nodes::LimiterControls::DEFAULT
                },
            ),
        ));
        session
            .graph()
            .set_sources(InputPort::new(lim, 0), [crate::OutputPort::new(dc, 0)])
            .unwrap();
        session.graph().connect(lim, MainOutput).unwrap();
        assert_eq!(
            session.graph_ref().node_latency(lim),
            Ok(Samples::new(LATENCY))
        );

        let request = || {
            Bounce::new()
                .main_output(ToMemory::new())
                .range(Range::Frames(Frame::new(0), Frame::new(DURATION as u64)))
        };

        let untrimmed = planes(bounced(
            &mut session,
            request().latency(BounceLatency::Keep),
        ));
        assert_eq!(untrimmed[0].len(), DURATION + LATENCY, "duration + latency");
        assert!(untrimmed[0][..LATENCY].iter().all(|&s| s == 0.0));
        assert!(
            untrimmed[0][LATENCY..].iter().all(|&s| s != 0.0),
            "every programme frame is kept"
        );

        let trimmed = planes(bounced(&mut session, request()));
        assert_eq!(trimmed[0].len(), DURATION);
        assert!(trimmed[0].iter().all(|&s| s != 0.0));
    }

    /// **A frozen bounce's timeline is stopped at beat 0**: what its
    /// `PrepareEdit` is handed (`PreparedGraph::ctx`, the timeline a node
    /// it builds binds to) reads not rolling, at beat 0. No forked node
    /// observes it while the render's transport is stopped; a hook does.
    #[test]
    fn a_frozen_bounces_timeline_is_stopped_at_beat_zero() {
        let mut session = session();
        let seen: Arc<Mutex<Option<(bool, tutti_units::Beat)>>> = Arc::default();
        let into = Arc::clone(&seen);
        bounced(
            &mut session,
            short().clock(ExportClock::frozen()).edit(Box::new(
                move |prepared: PreparedGraph<'_>| {
                    *into.lock() = Some((prepared.ctx.is_rolling(), prepared.ctx.beat()));
                },
            )),
        )
        .expect("the bounce renders");
        let (rolling, beat) = seen.lock().expect("the edit ran");
        assert!(!rolling, "a frozen bounce's timeline must not roll");
        assert_eq!(beat, tutti_units::Beat::new(0.0));
    }

    /// **The default clock is the project's tempo map from the range's first
    /// beat**: the edit hook's timeline reads the range's start beat, and the
    /// tempo of the session's map (a set tempo, not a default 120).
    #[test]
    fn the_default_clock_starts_at_the_ranges_first_beat_on_the_projects_tempo() {
        let mut session = session();
        session
            .set(
                session
                    .transport_handle()
                    .tempo()
                    .to(tutti_tempo::TempoMap::constant(Bpm::of(90.0)).unwrap()),
            )
            .expect("a tempo write is accepted");
        let seen: Arc<Mutex<Option<(tutti_units::Beat, Bpm)>>> = Arc::default();
        let into = Arc::clone(&seen);
        bounced(
            &mut session,
            Bounce::new()
                .main_output(ToMemory::new())
                .range(Range::Beats(Beat::new(3.0), Beat::new(4.0)))
                .edit(Box::new(move |prepared: PreparedGraph<'_>| {
                    *into.lock() = Some((
                        prepared.ctx.beat(),
                        prepared.ctx.tempo_map().tempo_at(prepared.ctx.beat()),
                    ));
                })),
        )
        .expect("renders");
        assert_eq!(
            seen.lock().expect("the edit ran"),
            (Beat::new(3.0), Bpm::of(90.0))
        );
    }

    /// **A bounce after a tempo write, with no update between, renders the new
    /// tempo**: the write is staged, and the bounce publishes it before it
    /// forks, so the fork's tempo map is the one written, not the one in force.
    #[test]
    fn a_bounce_after_a_tempo_set_renders_the_new_tempo_without_an_update() {
        let mut session = session();
        let before = session
            .get(session.transport_handle().tempo().effective())
            .unwrap();
        let written = Arc::new(tutti_tempo::TempoMap::constant(Bpm::of(75.0)).unwrap());
        session
            .set(session.transport_handle().tempo().to(Arc::clone(&written)))
            .expect("a tempo write is accepted");
        assert!(
            Arc::ptr_eq(
                &session
                    .get(session.transport_handle().tempo().effective())
                    .unwrap(),
                &before
            ),
            "staged only"
        );
        let seen: Arc<Mutex<Option<Option<Bpm>>>> = Arc::default();
        let into = Arc::clone(&seen);
        bounced(
            &mut session,
            Bounce::new()
                .main_output(ToMemory::new())
                .range(Range::Beats(Beat::new(0.0), Beat::new(1.0)))
                .edit(Box::new(move |prepared: PreparedGraph<'_>| {
                    *into.lock() = Some(prepared.ctx.tempo_map().constant_tempo());
                })),
        )
        .expect("renders");
        let seen = seen.lock().expect("the edit ran");
        assert_eq!(seen, Some(Bpm::of(75.0)));
    }

    /// **A bounce after a loop write, with no update between and no block
    /// rendered, renders the loop as written**: the default range is the
    /// loop, so the bounce is two beats (one second at 120 BPM), not refused
    /// for want of a range.
    #[test]
    fn a_bounce_after_a_loop_set_renders_the_loop_without_an_update() {
        let mut session = session();
        let region = tutti_timeline::LoopRegion::new(
            tutti_music::LoopRange::new(Beat::new(0.0), Beat::new(2.0)).unwrap(),
            true,
        );
        session
            .set(session.transport_handle().looping().to(region))
            .unwrap();
        let bounced = bounced(&mut session, Bounce::new().main_output(ToMemory::new()))
            .expect("the loop is the range");
        let frames = bounced.main_output().frames();
        assert_eq!(frames.get(), 48_000, "two beats at 120 BPM and 48 kHz");
    }

    /// **A bounce kept in flight past `BOUNCE_STALL_WARNING` is warned about
    /// once**, on the collect's `now`; its clock restarts once nothing is in
    /// flight, so the next leaked bounce is warned about too.
    ///
    /// The clock also restarts when a bounce starts: a host handing one
    /// bounce's slot to the next within a frame never lets a collect
    /// see nothing in flight, and two back-to-back bounces, each inside the
    /// bound, are not one long stall.
    #[test]
    fn a_leaked_bounce_is_warned_about_once() {
        let mut session = session();
        let collect = |session: &mut crate::DetachedSession, at: Instant| {
            session.run_phase(Phase::Collect, PhaseCx::at(at), &mut Vec::new());
            session.bounce.stall_warnings
        };
        let t0 = Instant::now();
        let job = session.bounce(short()).expect("the bounce starts");
        assert_eq!(collect(&mut session, t0), 0, "just started");
        let minute = Duration::from_secs(60);
        assert_eq!(
            collect(&mut session, t0 + BOUNCE_STALL_WARNING - minute),
            0,
            "inside the bound"
        );
        assert_eq!(collect(&mut session, t0 + BOUNCE_STALL_WARNING), 1);
        assert_eq!(
            collect(&mut session, t0 + BOUNCE_STALL_WARNING + minute),
            1,
            "once"
        );
        assert_eq!(collect(&mut session, t0 + 3 * BOUNCE_STALL_WARNING), 1);

        // The job dropped: nothing in flight, and the next bounce's clock
        // starts at its own first collect.
        drop(job);
        let t1 = t0 + 4 * BOUNCE_STALL_WARNING;
        assert_eq!(collect(&mut session, t1), 1);
        let job = session.bounce(short()).expect("the next starts");
        assert_eq!(collect(&mut session, t1 + minute), 1, "a new clock");
        assert_eq!(
            collect(&mut session, t1 + minute + BOUNCE_STALL_WARNING),
            2,
            "the second leak is warned about too"
        );
        drop(job);

        // Back to back, with no collect between: 50 minutes, then 59.
        let t2 = t1 + 4 * BOUNCE_STALL_WARNING;
        assert_eq!(collect(&mut session, t2), 2, "nothing in flight");
        let start = |session: &mut crate::DetachedSession| {
            session.bounce(short()).expect("a bounce starts")
        };
        let first = start(&mut session);
        assert_eq!(collect(&mut session, t2), 2);
        assert_eq!(collect(&mut session, t2 + 50 * minute), 2);
        session.apply(block_on(first)).unwrap();
        let second = start(&mut session);
        assert_eq!(
            collect(&mut session, t2 + 70 * minute),
            2,
            "the second bounce has been in flight for 20 minutes, not 70"
        );
        assert_eq!(collect(&mut session, t2 + 129 * minute), 2, "59 minutes");
        session.apply(block_on(second)).unwrap();
    }

    /// **A render cut from another session is refused `Stale`, and refusing it
    /// leaves this session's own bounce in flight**: the other session's token is
    /// the one the refused render drops.
    #[test]
    fn a_render_of_another_session_is_stale_and_releases_nothing_here() {
        let (mut here, mut there) = (session(), session());
        let ours = here.bounce(short()).unwrap();
        let theirs = block_on(there.bounce(short()).unwrap());
        assert!(there.bounce.in_flight(), "the render holds its token");
        assert!(matches!(here.apply(theirs), Err(BounceError::Stale)));
        assert!(here.bounce.in_flight(), "this session's token is untouched");
        assert!(
            matches!(here.bounce(short()), Err(BounceError::InFlight)),
            "this session's bounce is still the one in flight"
        );
        assert!(
            !there.bounce.in_flight(),
            "the refused render dropped its own token"
        );
        assert!(here.apply(block_on(ours)).is_ok(), "its own render applies");
    }

    /// **Dropping the awaited bounce cancels the render, and a bounce starts after
    /// the next collect.** The edit holds the render open until the future is
    /// dropped; the render then stops at its next check, short of rendering a
    /// block (its last reading is a preparing step), and its graph comes back to
    /// the session after its token is released.
    #[test]
    fn dropping_the_awaited_bounce_cancels_the_render() {
        use std::sync::mpsc::channel;
        let mut session = session();
        let (started, running) = channel();
        let (release, wait) = channel::<()>();
        let job = session
            .bounce(short().edit(Box::new(move |_: PreparedGraph<'_>| {
                started.send(()).unwrap();
                wait.recv().unwrap();
            })))
            .unwrap();
        let handle = job.handle();
        let mut awaited = Box::pin(job.into_future());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(
            awaited.as_mut().poll(&mut cx).is_pending(),
            "the render began"
        );
        running.recv().unwrap();
        drop(awaited);
        release.send(()).unwrap();
        // The render's graph is the last thing it hands back.
        drop(session.bounce.recv.recv().expect("the render ended"));
        assert!(!session.bounce.in_flight(), "the token is released");
        let last = handle.progress().expect("reported");
        assert_eq!(last.stage(), Stage::Preparing, "stopped before rendering");
        session.run_phase(Phase::Collect, PhaseCx::at(Instant::now()), &mut Vec::new());
        assert!(
            bounced(&mut session, short()).is_ok(),
            "the next bounce starts"
        );
    }

    /// **A bounce whose session is dropped resolves to `ShutDown`**: the weak
    /// executor does not upgrade, so nothing renders.
    #[test]
    fn a_bounce_of_a_dropped_session_is_shut_down() {
        let mut session = session();
        let job = session.bounce(short()).unwrap();
        drop(session);
        let rendered = block_on(job);
        assert!(
            matches!(rendered.result, Err(BounceError::ShutDown)),
            "{rendered:?}"
        );
    }

    /// **A panic in the render answers `Panicked` with its message**, and the
    /// token is released by the apply that reads it.
    #[test]
    fn a_panic_in_the_render_is_a_named_error() {
        let mut session = session();
        let job = session
            .bounce(short().edit(Box::new(|_: PreparedGraph<'_>| panic!("the edit gave up"))))
            .unwrap();
        let rendered = block_on(job);
        assert!(session.bounce.in_flight(), "the answer holds the token");
        match session.apply(rendered) {
            Err(BounceError::Panicked { message }) => assert_eq!(message, "the edit gave up"),
            other => panic!("expected Panicked, got {other:?}"),
        }
        assert!(!session.bounce.in_flight(), "the apply released it");
    }
}
