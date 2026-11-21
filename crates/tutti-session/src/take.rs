//! Takes: a recording that knows where it belongs on the timeline (design 030
//! section 5).
//!
//! [`Session::record`] arms a take on an [`Input`]. It opens a plan tap on the
//! input's inlet node, so the take is made of what the graph was given, frame
//! for frame on the graph's own clock ([`TapReader::next_frame`]), and runs the
//! pump into a sink on the session's executor, which the audio thread wakes by a
//! bell as the tap fills. Nothing is resampled or shifted
//! in the file: where the take belongs is metadata, a [`TakePlacement`] the take
//! states term by term.
//!
//! # Where the take is placed
//!
//! A sample the inlet emits at graph frame `F` was played by the performer in
//! answer to what the device played [`output`](TakeLatency::output) after it
//! was rendered, and reached the graph [`input`](TakeLatency::input) plus the
//! inlet's [`lead`](TakeLatency::lead) after it was sung, so the take's first
//! frame belongs at
//!
//! ```text
//! start = F - (input + output + lead) + offset
//! ```
//!
//! A latency the device has not reported is not a zero: the term is `None` in
//! [`TakeLatency`], left out of the sum, and [`TakeLatency::is_complete`] says so.
//!
//! # Punch
//!
//! A [`Punch`] names where the take starts and, optionally, where it stops, as
//! an [`At`]. A beat is turned into a frame on the control side, through the
//! project's tempo map, at the frame the transport will reach if it keeps
//! rolling from where the playhead is now ([`Session::playhead`]); a beat punch while the
//! transport is stopped is refused ([`TakeError::TransportStopped`]), and a
//! locate between arming and the punch is not followed. The pump drops tapped
//! frames before the punch-in and stops at the punch-out. A punch-in already
//! past starts at once, and the take reports the real `F`.
//!
//! # A gap is silence in place
//!
//! A tap that the pump cannot drain in time loses frames
//! ([`TapReader::take_lost`]); a take, unlike a plain recording, keeps its
//! alignment over the gap by writing silence for it, and counts the frames
//! ([`Recorded::lost`]). A dropout in the capture itself reaches the take as the
//! silence the inlet played.

use std::fmt;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;

use futures_lite::FutureExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use thiserror::Error;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, Interleaved, InterleavedMut, Sink, SinkFailed};
use tutti_codec::{FileEncoder, Format, Spec};
use tutti_engine::StreamLatency;
use tutti_graph::{MissingSource, NodeKey, TapAlign, TapOptions, TapReader};
use tutti_platform::{Staged, StagedCommitError};
use tutti_rt::{Bell, Listener, Mint};
use tutti_tempo::TempoMap;
use tutti_units::{At, Beat, Frame, SampleRate, Samples, Seconds};

use crate::{Input, Session, SessionEvent, Start, TakeLatency, Trim};

/// How many frames the pump moves a pass.
const PASS_FRAMES: usize = 1024;

tutti_rt::id! {
    /// One take of a session: the number [`TakeEvent`]s carry.
    pub struct TakeId;
}

/// Where a take starts and, if it has one, where it stops.
///
/// ```
/// use tutti_session::Punch;
/// use tutti_units::{At, Beat};
///
/// // Beat 16 to beat 48.
/// let punch = Punch::at(Beat::new(16.0)).until(Beat::new(48.0)).unwrap();
/// // Open-ended, from the next block.
/// let _ = Punch::now();
/// // Backwards is refused where it is made.
/// assert!(Punch::at(Beat::new(16.0)).until(Beat::new(8.0)).is_err());
/// # let _ = (punch, At::NextBlock);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Punch {
    at: At,
    until: Option<At>,
}

impl Default for Punch {
    /// [`Punch::now`].
    fn default() -> Self {
        Self::now()
    }
}

impl Punch {
    /// A take that starts at `at` and runs until it is finished.
    pub fn at(at: impl Into<At>) -> Self {
        Self {
            at: at.into(),
            until: None,
        }
    }

    /// A take that starts at once, from the next block.
    #[must_use]
    pub fn now() -> Self {
        Self::at(At::NextBlock)
    }

    /// This punch ending at `until`.
    ///
    /// # Errors
    ///
    /// [`PunchError`] when `until` is not after the punch-in: the same kind of
    /// position (two beats, two frames) that does not come later, or
    /// [`At::NextBlock`], which is a start and never an end. A beat against a
    /// frame cannot be compared until the take is armed; [`Session::record`]
    /// checks it there ([`TakeError::Punch`]).
    pub fn until(self, until: impl Into<At>) -> Result<Self, PunchError> {
        let until = until.into();
        let later = match (self.at, until) {
            (_, At::NextBlock) => false,
            (At::Beat(a), At::Beat(b)) => b.get() > a.get(),
            (At::Frame(a), At::Frame(b)) => b > a,
            // Not comparable until the transport's position is known.
            _ => true,
        };
        if later {
            Ok(Self {
                until: Some(until),
                ..self
            })
        } else {
            Err(PunchError { at: self.at, until })
        }
    }

    /// Where the take starts.
    #[must_use]
    pub fn start(&self) -> At {
        self.at
    }

    /// Where the take stops, if it is not open-ended.
    #[must_use]
    pub fn end(&self) -> Option<At> {
        self.until
    }
}

/// A punch that ends where it begins or before ([`Punch::until`]).
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[error("a punch ending at {until:?} is not after its start at {at:?}")]
pub struct PunchError {
    /// The punch-in.
    pub at: At,
    /// The refused punch-out.
    pub until: At,
}

/// The frames of the graph's clock a punch resolved to, when the take was
/// armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The first frame kept; one already past means "at once".
    pub start: Frame,
    /// The first frame not kept, or `None` for an open-ended take.
    pub end: Option<Frame>,
}

/// Where a take goes: a file, or a sink of the caller's.
///
/// A file is encoded in the format given and staged: it is written to a
/// temporary file beside its path and put in place when the take ends, so the
/// path holds either what it held before or the whole take. A take that fails
/// before then removes the temporary. A commit that fails ends the take with
/// [`TakeError::Commit`], which keeps the whole file at
/// [`kept`](StagedCommitError::kept) and leaves the path alone; the app owns
/// the kept file, and nothing removes it. To dither, wrap a
/// file's sink yourself ([`DitherSink`](crate::DitherSink)) and give it to
/// [`sink`](Self::sink); such a take has no path, so no
/// [`clip`](Recorded::clip).
pub struct TakeTarget<F: Float = f32> {
    kind: TargetKind<F>,
}

enum TargetKind<F: Float> {
    File {
        path: PathBuf,
        format: Format,
        create_new: bool,
    },
    Sink(Box<dyn DynSink<F>>),
}

impl<F: Float> fmt::Debug for TakeTarget<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TargetKind::File {
                path,
                format,
                create_new,
            } => f
                .debug_struct("TakeTarget::File")
                .field("path", path)
                .field("format", format)
                .field("create_new", create_new)
                .finish(),
            TargetKind::Sink(_) => f.debug_struct("TakeTarget::Sink").finish_non_exhaustive(),
        }
    }
}

impl<F: Float> TakeTarget<F> {
    /// A file at `path`, in `format`, as wide as the input and at the graph's
    /// rate.
    pub fn file(path: impl Into<PathBuf>, format: Format) -> Self {
        Self {
            kind: TargetKind::File {
                path: path.into(),
                format,
                create_new: false,
            },
        }
    }

    /// A WAV file of 32-bit float at `path`. An `f64` session keeps its frames
    /// whole in [`file`](Self::file) with `Pcm::Float64`.
    pub fn wav(path: impl Into<PathBuf>) -> Self {
        Self::file(path, Format::Wav(tutti_codec::Pcm::Float32))
    }

    /// Refuses a path that already exists instead of replacing it: the take
    /// is still written beside the path, and the commit that finds the path
    /// taken ends the take with [`TakeError::Commit`] of kind
    /// [`AlreadyExists`](std::io::ErrorKind::AlreadyExists). The path is left
    /// alone and the whole take is kept at the error's
    /// [`kept`](StagedCommitError::kept), the app's to move or delete. A sink
    /// target writes no file, so this does nothing to it.
    #[must_use]
    pub fn create_new(mut self) -> Self {
        if let TargetKind::File { create_new, .. } = &mut self.kind {
            *create_new = true;
        }
        self
    }

    /// A sink of the caller's. It must be as wide as the input and at the
    /// graph's rate (a sink carries no rate to check); a width that differs is
    /// [`TakeError::Width`].
    pub fn sink<S>(sink: S) -> Self
    where
        S: Sink<Sample = F> + Send + 'static,
        S::Error: Send + Sync + 'static,
    {
        Self {
            kind: TargetKind::Sink(Box::new(sink)),
        }
    }
}

/// A sink's [`Error`](Sink::Error) behind the trait object that forces its type
/// to be erased.
type SinkClosed = Arc<dyn std::error::Error + Send + Sync>;

/// A [`Sink`] that can be finalized through a box.
trait DynSink<F: Float>: Send {
    fn layout(&self) -> ChannelLayout;
    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed>;
    fn finalize(self: Box<Self>) -> Result<(), SinkClosed>;
}

impl<F: Float, S> DynSink<F> for S
where
    S: Sink<Sample = F> + Send,
    S::Error: Send + Sync + 'static,
{
    fn layout(&self) -> ChannelLayout {
        Sink::layout(self)
    }

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        Sink::write(self, frames)
    }

    fn finalize(self: Box<Self>) -> Result<(), SinkClosed> {
        Sink::finalize(*self).map_err(|e| Arc::new(e) as SinkClosed)
    }
}

/// What the device reports of its delay, read when a take starts: the
/// capture's and the playback's, in time, turned into frames at the graph's
/// rate. A direction nobody has reported is `None`.
#[derive(Debug)]
pub(crate) struct LatencyProbe {
    input: Arc<StreamLatency>,
    output: Option<Arc<StreamLatency>>,
}

impl LatencyProbe {
    pub(crate) fn new(input: Arc<StreamLatency>, output: Option<Arc<StreamLatency>>) -> Self {
        Self { input, output }
    }

    fn read(&self, rate: SampleRate) -> (Option<Samples>, Option<Samples>) {
        let frames = |cell: &StreamLatency| {
            cell.get()
                .map(|delay| Seconds::new(delay.as_secs_f64()).to_samples(rate))
        };
        (frames(&self.input), self.output.as_deref().and_then(frames))
    }
}

/// Where a take that has started belongs: the first frame it holds, the latency
/// and the start they give. Metadata: nothing is shifted in the file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TakePlacement {
    first: Frame,
    latency: TakeLatency,
    start: Start,
    start_beat: Beat,
}

impl TakePlacement {
    /// The graph frame of the take's first frame: `F`.
    #[must_use]
    pub fn first_frame(&self) -> Frame {
        self.first
    }

    /// The latency.
    #[must_use]
    pub fn latency(&self) -> &TakeLatency {
        &self.latency
    }

    /// Where the take belongs on the graph's clock.
    #[must_use]
    pub fn start(&self) -> Start {
        self.start
    }

    /// Where it belongs on the timeline: the beat the transport stood at, at
    /// [`start`](Self::start), taking the tempo map as it is now and the
    /// transport as rolling without a locate since it was armed.
    #[must_use]
    pub fn start_beat(&self) -> Beat {
        self.start_beat
    }
}

/// How a graph frame maps to a beat: the playhead at arming and the tempo.
#[derive(Debug)]
struct Anchor {
    beat: Beat,
    frame: Frame,
    map: Arc<TempoMap>,
    rate: SampleRate,
}

impl Anchor {
    /// The frames from the anchor to `frame`, signed.
    fn since(&self, frame: Frame) -> i128 {
        i128::from(frame.get()) - i128::from(self.frame.get())
    }

    fn beat_at(&self, start: Start) -> Beat {
        let delta = match start {
            Start::At(frame) => self.since(frame),
            Start::BeforeOrigin(n) => -(n.get() as i128) - i128::from(self.frame.get()),
        };
        let now = self.map.seconds_at(self.beat).get();
        self.map
            .beat_at_seconds(Seconds::new(now + delta as f64 / self.rate.get()))
    }

    /// The graph frame the transport reaches at `beat` if it rolls on from the
    /// playhead, or `None` when the beat is already behind it.
    fn frame_of(&self, beat: Beat) -> Option<Frame> {
        if beat.get().partial_cmp(&self.beat.get()) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        let frames = self
            .map
            .view(self.rate)
            .frames_between(self.beat, beat)
            .max(0) as u64;
        Some(self.frame + Samples::new(usize::try_from(frames).unwrap_or(usize::MAX)))
    }
}

/// The takes of a session: their numbering, and the channel their pumps report through
/// to [`Session::update`]. Unbounded, read only by the control thread, so a pump never
/// waits.
#[derive(Debug)]
pub(crate) struct Takes {
    mint: Mint,
    tx: Sender<TakeEvent>,
    rx: Receiver<TakeEvent>,
}

impl Takes {
    pub(crate) fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            mint: Mint::new(),
            tx,
            rx,
        }
    }

    /// The id of the next take begun.
    pub(crate) fn next_id(&self) -> TakeId {
        TakeId::mint(&self.mint)
    }

    /// Pushes every event the pumps sent since the last call onto `out`, in the order
    /// sent.
    pub(crate) fn land(&mut self, out: &mut Vec<SessionEvent>) {
        out.extend(self.rx.try_iter().map(SessionEvent::Take));
    }
}

/// What one take's pump reports through: its events, in order, and exactly one end.
///
/// A report dropped without [`end`](Self::end) (the executor dropped the pump's future
/// unrun or part-way) ends the take with [`TakeError::Dropped`].
struct Report {
    tx: Sender<TakeEvent>,
    id: TakeId,
    /// Set when the session drops: nothing sent lands.
    closed: Arc<AtomicBool>,
    ended: bool,
}

impl Report {
    fn new(takes: &Takes, id: TakeId, closed: Arc<AtomicBool>) -> Self {
        Self {
            tx: takes.tx.clone(),
            id,
            closed,
            ended: false,
        }
    }

    /// The session is gone: nothing sent will land.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The first frame is in. Never blocks; the session may be gone, and then there is
    /// no one to tell.
    fn started(&self, placement: TakePlacement) {
        let _ = self.tx.send(TakeEvent::Started {
            id: self.id,
            placement,
        });
    }

    /// The take's end, after every event it sent.
    fn end(mut self, result: Result<Recorded, TakeError>) {
        self.ended = true;
        let _ = self.tx.send(match result {
            Ok(recorded) => TakeEvent::Finished {
                id: self.id,
                recorded,
            },
            Err(error) => TakeEvent::Failed { id: self.id, error },
        });
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.ended {
            // The session may be gone; then nothing waits for this end.
            let _ = self.tx.send(TakeEvent::Failed {
                id: self.id,
                error: TakeError::Dropped,
            });
        }
    }
}

/// What the pump shares with the take's handle.
#[derive(Debug, Default)]
struct Shared {
    placement: OnceLock<TakePlacement>,
}

/// A recording that has ended: what it holds and where it belongs.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    path: Option<PathBuf>,
    placement: TakePlacement,
    frames: Samples,
    lost: Samples,
    end: TakeEnd,
}

/// Why a take ended ([`Recorded::end`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TakeEnd {
    /// By its punch-out, or by [`Take::finish`] (or the handle's drop).
    Completed,
    /// The input's node was removed from the graph: the take holds what the
    /// input played until then, and nothing after.
    InputRemoved,
}

impl Recorded {
    /// Why the take ended.
    #[must_use]
    pub fn end(&self) -> TakeEnd {
        self.end
    }

    /// The file the take was written to; `None` for a sink of the caller's.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Where it belongs.
    #[must_use]
    pub fn placement(&self) -> &TakePlacement {
        &self.placement
    }

    /// The latency of the placement.
    #[must_use]
    pub fn latency(&self) -> &TakeLatency {
        &self.placement.latency
    }

    /// Where it belongs on the graph's clock ([`TakePlacement::start`]).
    #[must_use]
    pub fn start(&self) -> Start {
        self.placement.start
    }

    /// The frames written, the silence over any gap included.
    #[must_use]
    pub fn frames(&self) -> Samples {
        self.frames
    }

    /// The frames the tap lost and the take filled with silence: zero means
    /// the file is the whole take.
    #[must_use]
    pub fn lost(&self) -> Samples {
        self.lost
    }

    /// The take as a clip, in the session's float, that starts where it
    /// belongs ([`TakePlacement::start_beat`]): add it to the graph, in place of
    /// the input if you like (`graph.replace(input.node(), done.clip()?)`), and
    /// a bounce can fork it. Present with the `sampler` feature and a
    /// `tutti-codec` `decode-*` feature on.
    ///
    /// # Errors
    ///
    /// [`TakeError::NoFile`] for a take written to a sink of the caller's;
    /// [`Error::Decode`](crate::Error::Decode) when the file cannot be read
    /// back (a format whose decoder is not built in);
    /// [`Error::Place`](crate::Error::Place) for a wave the sampler cannot
    /// place.
    #[cfg(all(feature = "sampler", codec))]
    pub fn clip<F: tutti_sampler::SamplerFloat>(&self) -> crate::Result<tutti_sampler::Clip<F>> {
        let path = self.path().ok_or(TakeError::NoFile)?;
        let wave = tutti_codec::read_wave::<F>(path)?;
        Ok(tutti_sampler::Clip::new(wave)?.at(self.placement.start_beat))
    }
}

/// What a take reports to [`Session::update`] ([`SessionEvent::Take`]).
///
/// An event is reported only while the session lives: a session dropped before a take's
/// [`Finished`](Self::Finished) lands loses it, as does a [`Take`] that outlives its
/// session, whose stop ring finds no bells.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TakeEvent {
    /// The first frame of the take arrived (the punch-in was reached).
    Started {
        /// The take.
        id: TakeId,
        /// Where it belongs.
        placement: TakePlacement,
    },
    /// The take ended, by its punch-out, by [`Take::finish`] or because its
    /// input was removed ([`Recorded::end`]), and its sink closed. Reported
    /// once.
    Finished {
        /// The take.
        id: TakeId,
        /// What it holds and where it belongs.
        recorded: Recorded,
    },
    /// The take failed: its sink, its job, or a transport it never reached.
    /// Reported once, instead of [`Finished`](Self::Finished).
    Failed {
        /// The take.
        id: TakeId,
        /// Why.
        error: TakeError,
    },
}

/// Why a take could not be armed or did not end cleanly. Convertible into the
/// session's [`Error`](crate::Error) (`Error::Take`).
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum TakeError {
    /// The input is not one this session holds.
    #[error("{0:?} is not an input of this session")]
    NotAnInput(NodeKey),
    /// The input's inlet node, or a port of it, is not in the graph.
    #[error(transparent)]
    Tap(#[from] MissingSource),
    /// The punch cannot be: its end is not after its start.
    #[error(transparent)]
    Punch(#[from] PunchError),
    /// A beat punch while the transport is stopped: the frame it would reach
    /// is not known until it rolls. Punch at a frame, or roll first.
    #[error("a punch at {0:?} needs a rolling transport to know its frame")]
    TransportStopped(Beat),
    /// The punch-out is already past: there is nothing to record.
    #[error("the punch ends at {end}, before the graph's frame {now}")]
    Past {
        /// The first frame the take would not keep.
        end: Frame,
        /// The graph frame the playhead stood at.
        now: Frame,
    },
    /// The sink's width is not the input's.
    #[error("the take's sink is {sink:?} wide but the input is {input:?}")]
    Width {
        /// The input's.
        input: ChannelLayout,
        /// The sink's.
        sink: ChannelLayout,
    },
    /// The file could not be created.
    #[error("the take's file could not be created: {0}")]
    Open(#[source] Arc<tutti_codec::Error>),
    /// The finished file could not be put in place (the path exists under
    /// [`TakeTarget::create_new`], or the file system refused the move). The
    /// whole take is kept at [`kept`](StagedCommitError::kept) and the path is
    /// untouched; the file is the app's to move or delete.
    #[error(transparent)]
    Commit(#[from] StagedCommitError),
    /// The sink failed: a write, or closing it. What was written before the
    /// failure is in the sink.
    ///
    /// A take written to a file keeps the file beside its path in `unfinished`
    /// (a full disk on a write, a failed header or close) and leaves the path alone: the frames so far are
    /// there, and the app owns the file, to move or delete. It is unfinished:
    /// a FLAC file may end in a cut frame, and a WAV's or
    /// AIFF's header states the whole frames on disk only if
    /// [`header_describes_frames`](tutti_codec::Unfinished::header_describes_frames) says so.
    /// `unfinished` is `None` for a sink of the caller's.
    #[error("the take's sink failed")]
    Sink {
        /// The sink's own error.
        #[source]
        source: Arc<dyn std::error::Error + Send + Sync>,
        /// The file kept, for a file target.
        unfinished: Option<tutti_codec::Unfinished>,
    },
    /// The sink refused a write, yet its `finalize` reported no error: it broke
    /// [`Sink`]'s contract, which has `finalize` report the first failure.
    #[error("the sink refused a write and its finalize reported no error")]
    SinkRefused,
    /// The executor refused to start the take's pump (a thread the OS would not
    /// give), before any file or tap was opened.
    #[error("the take's pump could not start: {0}")]
    NotStarted(std::io::ErrorKind),
    /// The take's pump panicked; `message` is the panic's, when it was a string.
    #[error("the take's pump panicked: {message}")]
    Panicked {
        /// The panic's message, or a placeholder for a payload of another type.
        message: String,
    },
    /// The executor dropped the take's pump unrun or part-way (a shutdown, a host that
    /// discards the future): it ended without a result.
    #[error("the executor dropped the take's pump without a result")]
    Dropped,
    /// The take was finished (or its handle dropped) before its punch-in was
    /// reached: it holds nothing.
    #[error("the take ended before its punch-in")]
    NeverStarted,
    /// A take written to a sink of the caller's has no file to make a clip
    /// from.
    #[error("the take was not written to a file")]
    NoFile,
}

/// A take that is in flight: [`Armed`] or [`Rolling`].
pub trait InFlight: sealed::Sealed {}

mod sealed {
    pub trait Sealed {}
}

/// The state of a take whose punch-in has not been seen.
#[derive(Clone, Copy, Debug)]
pub enum Armed {}

/// The state of a take whose first frame has arrived.
#[derive(Clone, Copy, Debug)]
pub enum Rolling {}

impl sealed::Sealed for Armed {}
impl sealed::Sealed for Rolling {}
impl InFlight for Armed {}
impl InFlight for Rolling {}

/// Stops the pump when the last handle of the take goes: the flag, then a ring to
/// wake a pump that awaits its tap.
#[derive(Debug)]
struct StopOnDrop {
    stop: Arc<AtomicBool>,
    bell: Bell,
}

impl StopOnDrop {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.bell.ring();
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A take in flight, in the state `S`: [`Armed`] until its first frame arrives
/// ([`rolling`](Take::rolling)), [`Rolling`] after.
///
/// Both states [`finish`](Take::finish), which stops it at once; a take with a
/// punch-out ends by itself. **Dropping the handle stops the take too**, without
/// waiting: the pump closes its sink on the executor and the end is still
/// reported ([`TakeEvent::Finished`]), in a later [`update`](Session::update).
#[must_use = "dropping a take stops it; keep it while it records, or finish it"]
pub struct Take<S> {
    id: TakeId,
    window: Window,
    shared: Arc<Shared>,
    stop: StopOnDrop,
    state: std::marker::PhantomData<fn() -> S>,
}

impl<S> fmt::Debug for Take<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Take")
            .field("id", &self.id)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl<S> Take<S> {
    /// The take's id, the one its events carry.
    #[must_use]
    pub fn id(&self) -> TakeId {
        self.id
    }

    /// The frames the punch resolved to when the take was armed.
    #[must_use]
    pub fn window(&self) -> Window {
        self.window
    }

    fn into_state<T>(self) -> Take<T> {
        Take {
            id: self.id,
            window: self.window,
            shared: self.shared,
            stop: self.stop,
            state: std::marker::PhantomData,
        }
    }
}

impl Take<Armed> {
    /// This take as [`Rolling`] once its first frame has arrived; the armed
    /// take back, unchanged, before.
    ///
    /// # Errors
    ///
    /// The take itself, while it is still waiting for its punch-in.
    pub fn rolling(self) -> Result<Take<Rolling>, Self> {
        if self.shared.placement.get().is_some() {
            Ok(self.into_state())
        } else {
            Err(self)
        }
    }
}

impl Take<Rolling> {
    /// Where the take belongs: known once it is rolling.
    ///
    /// # Panics
    ///
    /// Never: a rolling take has its placement.
    #[must_use]
    pub fn placement(&self) -> &TakePlacement {
        self.shared
            .placement
            .get()
            .expect("a rolling take has its placement")
    }
}

impl<S: InFlight> Take<S> {
    /// Stops the take at once and returns without waiting. The pump sees the stop on
    /// its next wake and closes its sink; what the take holds arrives in a later
    /// [`Session::update`] as [`TakeEvent::Finished`], or [`TakeEvent::Failed`] when the
    /// sink failed or the pump ended without a result. A take with a punch-out that
    /// already ended reports the same way.
    ///
    /// Never blocks and never joins: the pump closes the sink on its own thread.
    ///
    /// The result is lost when the [`Session`] is gone. A `Take` that outlives its
    /// session, or a session dropped before the take's [`TakeEvent::Finished`] lands,
    /// has no bells to carry the stop ring, so the parked pump's waker is released and
    /// no event arrives.
    pub fn finish(self) {
        self.stop.stop();
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Arms a take on `input`: opens a plan tap on its inlet node and runs the
    /// pump into `target` on the session's executor. The take records what the
    /// graph is given by the input, so it is on the graph's clock whatever the
    /// device does; the module docs say where it is placed.
    ///
    /// The pump is a future that awaits the tap's bell: the audio thread rings it as it
    /// writes each block, so the pump runs only when there are frames. It runs on a
    /// thread of its own, whether or not the app calls [`update`](Self::update).
    ///
    /// The tap compiles in the next [`update`](Self::update). The pump drops
    /// what comes before the punch-in; [`TakeEvent::Started`] lands in the
    /// update after its first frame, and [`TakeEvent::Finished`] or
    /// [`TakeEvent::Failed`] once at the end.
    ///
    /// # Errors
    ///
    /// [`TakeError::NotAnInput`] for a node that is not an input of this
    /// session; [`TakeError::Tap`] when it has left the graph;
    /// [`TakeError::TransportStopped`], [`TakeError::Punch`] and
    /// [`TakeError::Past`] for a punch that cannot be resolved;
    /// [`TakeError::NotStarted`] when the executor refuses the pump, before any file
    /// or tap exists; [`TakeError::Open`] and [`TakeError::Width`] for the target.
    /// Nothing is left open on an error.
    pub fn record(
        &mut self,
        input: &Input<C::Float>,
        punch: Punch,
        target: TakeTarget<C::Float>,
    ) -> Result<Take<Armed>, TakeError> {
        let node = input.node();
        let held = self
            .inputs
            .held(node)
            .ok_or(TakeError::NotAnInput(NodeKey::from(node)))?;
        let (lead, offset) = (held.lead, held.config.offset);
        let width = self
            .graph
            .node_outputs(node)
            .map_err(|missing| MissingSource::Node {
                node: missing.node(),
            })?;
        let layout = ChannelLayout::from_count(u16::try_from(width).unwrap_or(u16::MAX));

        let rate = self.graph.prepared().sample_rate();
        let playhead = self.playhead();
        let anchor = Anchor {
            beat: playhead.beat(),
            frame: playhead.frame(),
            map: self.transport.settings().tempo_map(),
            rate,
        };
        let window = resolve(&punch, &anchor, playhead.playing())?;

        // The pump starts first, before any file or tap exists, so a refused start is
        // this call's error and leaves nothing open. It waits for its parts, which come
        // through `parts` once the target and the tap are open; a take that fails
        // before then stops it, by the stop flag and a ring.
        let (bell, mut listener) = self.background.bells().bell();
        let stop = Arc::new(AtomicBool::new(false));
        let parts: Arc<Mutex<Option<Parts<C::Float>>>> = Arc::default();
        let (waiting, stopping) = (Arc::clone(&parts), Arc::clone(&stop));
        let pump_bell = bell.clone();
        self.background
            .spawn_pump("tutti-take", pump_bell, async move {
                let Some((pump, report)) = await_parts(&mut listener, &waiting, &stopping).await
                else {
                    return;
                };
                let ended = AssertUnwindSafe(pump.run(&report, &mut listener))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|payload| {
                        Err(TakeError::Panicked {
                            message: tutti_platform::panic_message(payload.as_ref()),
                        })
                    });
                // The session's drop closed the report: no event carries a failed
                // commit, so it is logged here.
                if let Err(error) = &ended
                    && report.is_closed()
                {
                    tracing::warn!("a take ended with the session and did not commit: {error}");
                }
                report.end(ended);
            })
            .map_err(|error| TakeError::NotStarted(error.kind()))?;
        let stop = StopOnDrop { stop, bell };

        let (sink, path) = open_target(target, layout, rate)?;

        let ports: Vec<_> = (0..width).map(|channel| node.out(channel)).collect();
        // A second of frames: the pump moves what the tap holds each time it wakes.
        let capacity = rate.one_second();
        let reader = self.graph.taps().open(
            &ports,
            TapOptions::new(capacity)
                .align(TapAlign::Raw)
                .bell(stop.bell.clone()),
        )?;

        let id = self.inputs.takes().next_id();
        let shared = Arc::new(Shared::default());
        let pump = Pump {
            reader,
            sink,
            path,
            window,
            terms: (lead, offset),
            probe: self.inputs.latency_probe(),
            anchor,
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop.stop),
        };
        let report = Report::new(self.inputs.takes(), id, self.background.closed());
        *parts.lock().unwrap_or_else(PoisonError::into_inner) = Some((pump, report));
        stop.bell.ring();
        Ok(Take {
            id,
            window,
            shared,
            stop,
            state: std::marker::PhantomData,
        })
    }
}

/// What the pump future waits for: the pump and what it reports through.
type Parts<F> = (Pump<F>, Report);

/// Awaits the pump's parts: `None` when the take stopped before they came.
async fn await_parts<F: Float>(
    listener: &mut Listener,
    parts: &Mutex<Option<Parts<F>>>,
    stop: &AtomicBool,
) -> Option<Parts<F>> {
    loop {
        if let Some(found) = parts.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Some(found);
        }
        if stop.load(Ordering::Acquire) {
            return None;
        }
        listener.rung().await;
    }
}

/// The frames a punch means, against the playhead the take was armed at.
fn resolve(punch: &Punch, anchor: &Anchor, rolling: bool) -> Result<Window, TakeError> {
    let frame_of = |at: At| -> Result<Frame, TakeError> {
        match at {
            At::Frame(frame) => Ok(frame),
            At::NextBlock => Ok(anchor.frame),
            At::Beat(beat) if !rolling => Err(TakeError::TransportStopped(beat)),
            // Already behind the playhead: at once.
            At::Beat(beat) => Ok(anchor.frame_of(beat).unwrap_or(anchor.frame)),
        }
    };
    let start = frame_of(punch.start())?;
    let end = punch.end().map(frame_of).transpose()?;
    if let Some(end) = end {
        // A beat against a frame, compared now that both are frames.
        if end <= start {
            return Err(PunchError {
                at: punch.start(),
                until: punch.end().unwrap_or(At::NextBlock),
            }
            .into());
        }
        if end <= anchor.frame {
            return Err(TakeError::Past {
                end,
                now: anchor.frame,
            });
        }
    }
    Ok(Window { start, end })
}

/// The target as a sink at the input's width, and the file it writes if it is
/// one.
fn open_target<F: Float>(
    target: TakeTarget<F>,
    layout: ChannelLayout,
    rate: SampleRate,
) -> Result<(Box<dyn DynSink<F>>, Option<PathBuf>), TakeError> {
    match target.kind {
        TargetKind::File {
            path,
            format,
            create_new,
        } => {
            let staged = Staged::new(&path);
            let staged = if create_new {
                staged.create_new()
            } else {
                staged
            };
            let encoder = FileEncoder::create(&Spec::of_width(format, layout, rate), staged)
                .map_err(|e| TakeError::Open(Arc::new(e)))?;
            let file = StagedSink {
                encoder,
                layout,
                failed: false,
                sample: PhantomData,
            };
            Ok((Box::new(file), Some(path)))
        }
        TargetKind::Sink(sink) => {
            if sink.layout() != layout {
                return Err(TakeError::Width {
                    input: layout,
                    sink: sink.layout(),
                });
            }
            Ok((sink, None))
        }
    }
}

/// A staged file as the [`Sink`] a take writes: whole frames at the take's
/// width, the encoder keeping the first write failure for
/// [`finalize`](Sink::finalize), which closes the file and puts it in place.
struct StagedSink<F> {
    encoder: FileEncoder,
    layout: ChannelLayout,
    /// A write failed: later writes are refused, and the encoder reports the
    /// first failure at [`finalize`](Sink::finalize), keeping the file.
    failed: bool,
    sample: PhantomData<fn(F)>,
}

/// How a [`StagedSink`] ended without its file in place.
#[derive(Debug, Error)]
enum StagedClosed {
    /// The finished file could not be put in place; it is kept.
    #[error(transparent)]
    Commit(StagedCommitError),
    /// A write or the close failed; the unfinished file is kept.
    #[error("{source}")]
    Failed {
        #[source]
        source: tutti_codec::Error,
        unfinished: tutti_codec::Unfinished,
    },
}

impl<F: Float> Sink for StagedSink<F> {
    type Sample = F;
    type Error = StagedClosed;

    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        if self.failed {
            return Err(SinkFailed);
        }
        self.encoder.write(frames).map_err(|_| {
            self.failed = true;
            SinkFailed
        })
    }

    fn finalize(self) -> Result<(), Self::Error> {
        self.encoder.finish_keeping().map_err(|kept| match kept {
            tutti_codec::Kept::Commit(commit) => StagedClosed::Commit(commit),
            tutti_codec::Kept::Unfinished { source, unfinished } => {
                StagedClosed::Failed { source, unfinished }
            }
        })
    }
}

/// A sink's failure to finalize as the take's: a commit's is
/// [`TakeError::Commit`], which names the file it kept.
fn closing_failed(error: SinkClosed) -> TakeError {
    match error.downcast_ref::<StagedClosed>() {
        Some(StagedClosed::Commit(commit)) => TakeError::Commit(commit.clone()),
        Some(StagedClosed::Failed { source, unfinished }) => TakeError::Sink {
            source: Arc::new(source.clone()),
            unfinished: Some(unfinished.clone()),
        },
        None => TakeError::Sink {
            source: error,
            unfinished: None,
        },
    }
}

/// The pump of one take: the tap's reader into the sink, between the
/// punch-in and the punch-out, on the graph's frames.
struct Pump<F: Float> {
    reader: TapReader<F>,
    sink: Box<dyn DynSink<F>>,
    path: Option<PathBuf>,
    window: Window,
    /// The inlet's lead and the input's trim.
    terms: (Samples, Trim),
    probe: LatencyProbe,
    anchor: Anchor,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl<F: Float> Pump<F> {
    async fn run(
        mut self,
        report: &Report,
        listener: &mut Listener,
    ) -> Result<Recorded, TakeError> {
        let layout = self.reader.layout();
        let mut scratch = vec![F::ZERO; layout.interleaved_len(Samples::new(PASS_FRAMES))];
        let silence = vec![F::ZERO; layout.interleaved_len(Samples::new(PASS_FRAMES))];
        // A whole number of frames at the tap's width, so the view builds; a
        // zero-width tap has nothing to take.
        let Ok(silence) = Interleaved::new(&silence, layout) else {
            return Err(TakeError::NeverStarted);
        };
        let mut placement: Option<TakePlacement> = None;
        // The graph frame the next frame written belongs to.
        let mut cursor: Option<Frame> = None;
        let (mut written, mut lost) = (0usize, 0usize);
        let mut failed = false;
        let mut removed = false;

        'pump: while !self.stop.load(Ordering::Acquire) && !report.is_closed() {
            // The tap's end before its ring, so a frame written before the end
            // is never missed.
            let gone = self.reader.source_gone() || self.reader.writer_gone();
            let run = self.reader.contiguous().get().min(PASS_FRAMES);
            if run == 0 {
                // Every frame the input played is read and its node is gone:
                // nothing more will come.
                if gone {
                    removed = true;
                    break 'pump;
                }
                // The tap rings as it writes a block and when it ends.
                listener.rung().await;
                continue;
            }
            let first = self.reader.next_frame();
            // `run` whole frames at the tap's width, so the view builds.
            let Ok(window) = InterleavedMut::new(
                &mut scratch[..layout.interleaved_len(Samples::new(run))],
                layout,
            ) else {
                failed = true;
                break 'pump;
            };
            let Ok(popped) = self.reader.pop_into(window) else {
                failed = true;
                break 'pump;
            };
            let popped = popped.get();
            let past = first + Samples::new(popped);
            // The window of this pass: [lo, hi).
            let lo = first.max(self.window.start);
            let hi = self.window.end.map_or(past, |end| past.min(end));
            let over = self.window.end.is_some_and(|end| first >= end);
            if !over && lo < hi {
                // A gap before this pass: the frames the tap lost, kept in
                // place as silence so the take stays aligned.
                if let Some(next) = cursor {
                    let gap = lo.since(next).map_or(0, Samples::get);
                    if !write_silence(&mut *self.sink, silence, gap) {
                        failed = true;
                        break 'pump;
                    }
                    written += gap;
                    lost += gap;
                } else {
                    let found = self.place(lo);
                    placement = Some(found);
                    let _ = self.shared.placement.set(found);
                    report.started(found);
                }
                let from = lo.since(first).map_or(0, Samples::get);
                let to = hi.since(first).map_or(0, Samples::get);
                // `popped` whole frames at the tap's width, so the view builds.
                let Ok(popped_view) = Interleaved::new(
                    &scratch[..layout.interleaved_len(Samples::new(popped))],
                    layout,
                ) else {
                    failed = true;
                    break 'pump;
                };
                let kept = popped_view.slice(Samples::new(from)..Samples::new(to));
                if self.sink.write(kept).is_err() {
                    failed = true;
                    break 'pump;
                }
                written += to - from;
                cursor = Some(hi);
            }
            if self.window.end.is_some_and(|end| past >= end) || over {
                break 'pump;
            }
        }
        // A punch-out the tap's gap straddled: silence up to it, so the take is
        // exactly the frames the punch names.
        if !failed
            && !removed
            && let (Some(end), Some(next)) = (self.window.end, cursor)
            && self.reader.next_frame() >= end
        {
            let gap = end.since(next).map_or(0, Samples::get);
            if write_silence(&mut *self.sink, silence, gap) {
                written += gap;
                lost += gap;
            } else {
                failed = true;
            }
        }
        let closed = self.sink.finalize();
        if let Err(error) = closed {
            return Err(closing_failed(error));
        }
        if failed {
            return Err(TakeError::SinkRefused);
        }
        // A take finished before its punch-in has nothing to place.
        let placement = placement.ok_or(TakeError::NeverStarted)?;
        Ok(Recorded {
            path: self.path,
            placement,
            frames: Samples::new(written),
            lost: Samples::new(lost),
            end: if removed {
                TakeEnd::InputRemoved
            } else {
                TakeEnd::Completed
            },
        })
    }

    /// The placement of a take whose first frame is `first`: the device's
    /// delay as it reports now.
    fn place(&self, first: Frame) -> TakePlacement {
        let (input, output) = self.probe.read(self.anchor.rate);
        let (lead, offset) = self.terms;
        let latency = TakeLatency::new(input, output, lead, offset);
        let start = latency.place(first);
        TakePlacement {
            first,
            latency,
            start,
            start_beat: self.anchor.beat_at(start),
        }
    }
}

/// Writes `frames` frames of silence, a pass at a time. `false`: the sink
/// refused.
fn write_silence<F: Float>(
    sink: &mut dyn DynSink<F>,
    silence: Interleaved<'_, F>,
    frames: usize,
) -> bool {
    let mut left = frames;
    while left > 0 {
        let now = left.min(silence.frames().get());
        if sink
            .write(silence.slice(Samples::ZERO..Samples::new(now)))
            .is_err()
        {
            return false;
        }
        left -= now;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tutti_units::Bpm;

    /// A sink that keeps what it is written.
    struct Keep(Arc<Mutex<Vec<f32>>>);

    impl Sink for Keep {
        type Sample = f32;
        type Error = core::convert::Infallible;
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::MONO
        }
        fn write(&mut self, frames: Interleaved<'_, f32>) -> Result<(), SinkFailed> {
            self.0.lock().unwrap().extend_from_slice(frames.samples());
            Ok(())
        }
        fn finalize(self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// **A gap the tap lost is silence in place, counted, and the take is
    /// exactly the frames of its window.** Frames 0..100 arrive, 100..150 are
    /// lost, 150..250 arrive: the take holds 250 frames, the lost 50 are zeros
    /// where they were, and the later frames are not shifted back.
    #[test]
    fn a_lost_gap_is_silence_in_place_and_counted() {
        let (reader, mut writer) = TapReader::pair(1, Samples::new(4096));
        let ramp = |from: usize, to: usize| (from..to).map(|i| i as f32 + 1.0).collect::<Vec<_>>();
        writer.push_interleaved(Interleaved::new(&ramp(0, 100), writer.layout()).unwrap());
        writer.skip(50);
        writer.push_interleaved(Interleaved::new(&ramp(150, 250), writer.layout()).unwrap());

        let kept = Arc::new(Mutex::new(Vec::new()));
        let map = Arc::new(TempoMap::constant(Bpm::of(120.0)).unwrap());
        let pump = Pump {
            reader,
            sink: Box::new(Keep(Arc::clone(&kept))),
            path: None,
            window: Window {
                start: Frame::new(0),
                end: Some(Frame::new(250)),
            },
            terms: (Samples::new(0), Trim::NONE),
            probe: LatencyProbe::new(Arc::default(), None),
            anchor: Anchor {
                beat: Beat::new(0.0),
                frame: Frame::new(0),
                map,
                rate: SampleRate::SR_48K,
            },
            shared: Arc::default(),
            stop: Arc::default(),
        };
        let mut takes = Takes::new();
        let report = Report::new(&takes, takes.next_id(), Arc::default());
        let executor = tutti_platform::Executor::new();
        let bells = tutti_rt::Bells::new(&executor).expect("the bells' tick task");
        let (_bell, mut listener) = bells.bell();
        // Every frame is in the tap and the punch-out is inside them: the pump runs to
        // its end in one poll, with nothing to await.
        let mut pumping = Box::pin(async move {
            let ended = pump.run(&report, &mut listener).await;
            report.end(ended);
        });
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            pumping.as_mut().poll(&mut cx).is_ready(),
            "the pump awaited a tap that held every frame it needed"
        );
        let mut events = Vec::new();
        takes.land(&mut events);
        let [
            SessionEvent::Take(TakeEvent::Started { .. }),
            SessionEvent::Take(TakeEvent::Finished { recorded, .. }),
        ] = events.as_slice()
        else {
            panic!("not a start then a finish: {events:?}");
        };
        let kept = kept.lock().unwrap();
        assert_eq!(kept.len(), 250);
        assert_eq!(recorded.lost(), Samples::new(50));
        assert_eq!(recorded.frames(), Samples::new(250));
        assert_eq!(kept[99], 100.0);
        assert!(
            kept[100..150].iter().all(|&s| s == 0.0),
            "the gap is silence"
        );
        assert_eq!(kept[150], 151.0, "later frames are not shifted back");
        assert_eq!(kept[249], 250.0);
    }
}
