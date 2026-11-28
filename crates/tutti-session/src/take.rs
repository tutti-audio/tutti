//! Takes: a recording that knows where it belongs on the timeline.
//!
//! [`Session::record`] arms a take on an [`Input`] into a [`Destination`]: a
//! [`ToFile`](tutti_codec::ToFile), [`ToMemory`](crate::ToMemory) or
//! [`ToSink`](crate::ToSink). It opens a plan tap on the input's inlet node, so the take
//! is made of what the graph was given, frame for frame on the graph's own clock
//! ([`TapReader::next_frame`]), and runs the pump into the destination on the session's
//! executor, which the audio thread wakes by a bell as the tap fills. Nothing is shifted in
//! what the take holds: where the take belongs is metadata, a [`TakePlacement`] the take
//! states term by term.
//!
//! # The handle is the take's end
//!
//! A [`Take`] is awaited: its future resolves, once, to the [`Recorded`] the take holds
//! or to the [`TakeError`] that ended it. `take.await` waits for the take's own end (its
//! punch-out, or its input's removal) and [`finish`](Take::finish) stops the take and
//! returns the same future. Dropping the handle or the future stops the take; the pump
//! still closes the destination and commits on the executor, and a failure nobody is left
//! to receive is logged by the pump.
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
use std::future::{Future, IntoFuture};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use tutti_rt::Mutex;

use futures_channel::oneshot;
use futures_lite::FutureExt;
use thiserror::Error;
use tutti_buffers::{ChannelLayout, Float, FoldError, Interleaved, InterleavedMut};
use tutti_dsp::FftFloat;
use tutti_engine::StreamLatency;
use tutti_graph::{MissingSource, NodeKey, TapAlign, TapOptions, TapReader};
use tutti_rt::{Bell, Listener};
use tutti_tempo::TempoMap;
use tutti_timeline::{Recording, TakeId};
use tutti_units::{At, Beat, Frame, SampleRate, Samples, Seconds};

use crate::bounce::{OutputOf, render_output::Sealed};
use crate::take_writer::Writer;
use crate::transport_handle::When;
use crate::{Destination, Input, Session, Start, TakeLatency, TransportHandle, Trim};

/// What the pump of a take of `D` writes into.
type WriterOf<D, F> = <D as Sealed<F>>::Writer;

/// How many frames the pump moves a pass.
const PASS_FRAMES: usize = 1024;

/// Where a take starts and, if it has one, where it stops.
///
/// ```
/// use tutti_session::Punch;
/// use tutti_units::{At, Beat};
///
/// // Beat 16 to beat 48.
/// let punch = Punch::new(Beat::new(16.0)).until(Beat::new(48.0)).unwrap();
/// // Open-ended, from the next block.
/// let _ = Punch::new(At::NextBlock);
/// // Backwards is refused where it is made.
/// assert!(Punch::new(Beat::new(16.0)).until(Beat::new(8.0)).is_err());
/// # let _ = (punch, At::NextBlock);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Punch {
    at: At,
    until: Option<At>,
}

impl Default for Punch {
    /// [`Punch::new`] at [`At::NextBlock`]: a take that starts at once.
    fn default() -> Self {
        Self::new(At::NextBlock)
    }
}

impl Punch {
    /// A take that starts at `at` and runs until it is finished.
    pub fn new(at: impl Into<At>) -> Self {
        Self {
            at: at.into(),
            until: None,
        }
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

/// What one take's pump reports through: its one end, to the handle's future.
///
/// A report dropped without [`end`](Self::end) (the executor dropped the pump's future unrun or
/// part-way) leaves the future without an answer, which it reads as [`TakeError::Dropped`].
struct Report<D: Destination<F>, F: FftFloat> {
    reply: Option<oneshot::Sender<Result<Recorded<D, F>, TakeError>>>,
    /// Set when the session drops: the pump stops.
    closed: Arc<AtomicBool>,
    /// Stops the transport recording this take once the pump is done with it.
    recorder: Recorder,
}

impl<D: Destination<F>, F: FftFloat> Report<D, F> {
    fn new(
        reply: oneshot::Sender<Result<Recorded<D, F>, TakeError>>,
        closed: Arc<AtomicBool>,
        recorder: Recorder,
    ) -> Self {
        Self {
            reply: Some(reply),
            closed,
            recorder,
        }
    }

    /// The session is gone: the pump stops and closes.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The take's end. Never blocks. A failure with no handle or future left to receive it
    /// is logged here, the one place it can be.
    fn end(mut self, result: Result<Recorded<D, F>, TakeError>) {
        let Some(reply) = self.reply.take() else {
            return;
        };
        if let Err(Err(error)) = reply.send(result) {
            tracing::warn!("a take ended with no one to tell and did not commit: {error}");
        }
    }
}

impl<D: Destination<F>, F: FftFloat> Drop for Report<D, F> {
    fn drop(&mut self) {
        // However the pump ended (its punch-out, a finish, the input's
        // removal, a failure, a drop), the transport stops recording the take.
        self.recorder.stop();
    }
}

/// A take's way to the transport: its [`Recording`] commands, sent through the
/// session's handle from the take's handle or its pump.
#[derive(Clone, Debug)]
struct Recorder {
    /// Shared, so a `Take` stays small.
    handle: Arc<TransportHandle>,
    id: TakeId,
}

impl Recorder {
    fn new(handle: TransportHandle, id: TakeId) -> Self {
        Self {
            handle: Arc::new(handle),
            id,
        }
    }

    /// The take begins recording at `when`.
    fn start(&self, when: When) -> Result<(), TakeError> {
        self.handle
            .send_recording(Recording::Start(self.id), when)
            .map_err(|_| TakeError::TransportFull)
    }

    /// The take stops recording at `when`.
    fn stop_at(&self, when: When) -> Result<(), TakeError> {
        self.handle
            .send_recording(Recording::Stop(self.id), when)
            .map_err(|_| TakeError::TransportFull)
    }

    /// The take stops recording at the next block: a repeat of a stop already
    /// sent is not sent again, and one the queue refuses is owed and sent by
    /// a later [`Session::update`].
    fn stop(&self) {
        let _owed = self.stop_at(When::NextBlock);
    }
}

/// What the pump shares with the take's handle.
#[derive(Debug, Default)]
struct Shared {
    placement: OnceLock<TakePlacement>,
}

/// A recording that has ended: what it holds and where it belongs. Its
/// [`output`](Self::output) is what the take's [`Destination`] holds: a [`Written`](tutti_export::Written) for a
/// [`ToFile`](tutti_codec::ToFile), the [`Rendered`](tutti_export::Rendered) planes for a [`ToMemory`](crate::ToMemory),
/// and `()` for a [`ToSink`](crate::ToSink), which has what it was given.
///
/// Only a take written to a file is a clip: a take into memory or a sink has no file to
/// read back.
///
/// ```compile_fail,E0599
/// # use tutti_session::{Recorded, ToMemory};
/// fn f(done: Recorded<ToMemory>) {
///     let _ = done.clip::<f32>();
/// }
/// ```
///
/// ```compile_fail,E0599
/// # use tutti_session::{Recorded, ToSink};
/// # use tutti_buffers::Sink;
/// fn f<S: Sink<Sample = f32> + Send + 'static>(done: Recorded<ToSink<S>>)
/// where
///     S::Error: Send + Sync + 'static,
/// {
///     let _ = done.clip::<f32>();
/// }
/// ```
pub struct Recorded<D: Destination<F>, F: FftFloat = f32> {
    output: OutputOf<D, F>,
    placement: TakePlacement,
    frames: Samples,
    lost: Samples,
    end: TakeEnd,
}

impl<D: Destination<F>, F: FftFloat> fmt::Debug for Recorded<D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recorded")
            .field("output", &self.output)
            .field("placement", &self.placement)
            .field("frames", &self.frames)
            .field("lost", &self.lost)
            .field("end", &self.end)
            .finish()
    }
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

impl<D: Destination<F>, F: FftFloat> Recorded<D, F> {
    /// Why the take ended.
    #[must_use]
    pub fn end(&self) -> TakeEnd {
        self.end
    }

    /// What the destination holds.
    #[must_use]
    pub fn output(&self) -> &OutputOf<D, F> {
        &self.output
    }

    /// What the destination holds, owned: the planes of a take into memory move out
    /// without a copy.
    #[must_use]
    pub fn into_output(self) -> OutputOf<D, F> {
        self.output
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
    /// the output is the whole take.
    #[must_use]
    pub fn lost(&self) -> Samples {
        self.lost
    }
}

#[cfg(all(feature = "sampler", codec))]
impl<F: FftFloat> Recorded<tutti_codec::ToFile, F> {
    /// The take as a clip, in the session's float, that starts where it
    /// belongs ([`TakePlacement::start_beat`]): add it to the graph, in place of
    /// the input if you like (`graph.replace(input.node(), done.clip()?)`), and
    /// a bounce can fork it. Present with the `sampler` feature and a
    /// `tutti-codec` `decode-*` feature on.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`](crate::Error::Decode) when the file cannot be read
    /// back (a format whose decoder is not built in);
    /// [`Error::Place`](crate::Error::Place) for a wave the sampler cannot
    /// place.
    pub fn clip<G: tutti_sampler::SamplerFloat>(&self) -> crate::Result<tutti_sampler::Clip<G>> {
        let wave = tutti_codec::read_wave::<G>(&self.output.path)?;
        Ok(tutti_sampler::Clip::new(wave)?.at(self.placement.start_beat))
    }
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
    /// The transport refused the take's start or its punch-out: too many
    /// takes are recording ([`RECORDING_CAPACITY`](tutti_timeline::RECORDING_CAPACITY)),
    /// or its command queue is full.
    #[error("the transport refused the take: too many takes recording, or its queue is full")]
    TransportFull,
    /// The sink's width is not the input's.
    #[error("the take's sink is {sink:?} wide but the input is {input:?}")]
    Width {
        /// The input's.
        input: ChannelLayout,
        /// The sink's.
        sink: ChannelLayout,
    },
    /// The file could not be created, or its encoding cannot take the input (a fold or a
    /// resample it cannot make).
    #[error("the take's file could not be created: {0}")]
    Open(#[source] Arc<tutti_codec::EncodeError>),
    /// The planes' channels cannot be folded from the input's.
    #[error("the take's channels cannot be folded from the input's: {0}")]
    Fold(#[from] FoldError),
    /// The sink failed: a write, or closing it. What was written before the
    /// failure is in the sink.
    ///
    /// A take written to a file keeps the file in `unfinished` (a full disk on a write, a
    /// failed header, close or sync): the take is written in place, so `unfinished` names the
    /// path itself, with the frames so far, and the app owns the file, to move or delete. It is
    /// unfinished: a FLAC file may end in a cut frame, and a WAV's or AIFF's header states the
    /// whole frames on disk only if
    /// [`header_describes_frames`](tutti_codec::Unfinished::header_describes_frames) says so.
    /// `unfinished` is `None` for a destination that is not a file.
    #[error("the take's sink failed")]
    Sink {
        /// The sink's own error.
        #[source]
        source: Arc<dyn std::error::Error + Send + Sync>,
        /// The file kept, for a file target.
        unfinished: Option<tutti_codec::Unfinished>,
    },
    /// The sink refused a write, yet its `finalize` reported no error: it broke
    /// [`Sink`](tutti_buffers::Sink)'s contract, which has `finalize` report the first failure.
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
    /// The take's stop at the transport once its start is sent; `None` until
    /// then, when there is nothing to stop.
    recorder: Option<Recorder>,
}

impl StopOnDrop {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.bell.ring();
        if let Some(recorder) = &self.recorder {
            recorder.stop();
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A take in flight, in the state `S`: [`Armed`] until its first frame arrives
/// ([`rolling`](Take::rolling)), [`Rolling`] after, into the destination `D` at the graph's
/// float `F`.
///
/// **The handle is the take's one entry.** It is awaited (`take.await`): the take's future
/// resolves, once, to the [`Recorded`] the take holds or to the [`TakeError`] that ended it,
/// when the take ends by itself (its punch-out, or its input's removal), without stopping
/// it. [`finish`](Take::finish) stops the take and returns the same future. Neither needs
/// [`Session::update`]: the pump runs on the session's executor, and the future is answered
/// when the pump has closed the destination.
///
/// **Dropping the handle or the future stops the take**, without waiting: the pump closes
/// the destination on the executor and commits, and a failure that no one is left to receive
/// is logged by the pump (`tracing`, on the executor's thread, never the audio thread). A
/// take whose session is dropped first commits in the session's drop, and its future still
/// receives the result when it is polled.
#[must_use = "dropping a take stops it; keep it while it records, or finish it"]
pub struct Take<S, D: Destination<F>, F: FftFloat = f32> {
    id: TakeId,
    window: Window,
    shared: Arc<Shared>,
    stop: StopOnDrop,
    reply: oneshot::Receiver<Result<Recorded<D, F>, TakeError>>,
    state: std::marker::PhantomData<fn() -> S>,
}

impl<S, D: Destination<F>, F: FftFloat> fmt::Debug for Take<S, D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Take")
            .field("id", &self.id)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl<S, D: Destination<F>, F: FftFloat> Take<S, D, F> {
    /// The take's id, the one the transport records it by.
    #[must_use]
    pub fn id(&self) -> TakeId {
        self.id
    }

    /// The frames the punch resolved to when the take was armed.
    #[must_use]
    pub fn window(&self) -> Window {
        self.window
    }

    fn into_state<T>(self) -> Take<T, D, F> {
        Take {
            id: self.id,
            window: self.window,
            shared: self.shared,
            stop: self.stop,
            reply: self.reply,
            state: std::marker::PhantomData,
        }
    }
}

impl<D: Destination<F>, F: FftFloat> Take<Armed, D, F> {
    /// This take as [`Rolling`] once its first frame has arrived; the armed
    /// take back, unchanged, before.
    ///
    /// # Errors
    ///
    /// The take itself, while it is still waiting for its punch-in.
    pub fn rolling(self) -> Result<Take<Rolling, D, F>, Self> {
        if self.shared.placement.get().is_some() {
            Ok(self.into_state())
        } else {
            Err(self)
        }
    }
}

impl<D: Destination<F>, F: FftFloat> Take<Rolling, D, F> {
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

impl<S: InFlight, D: Destination<F>, F: FftFloat> Take<S, D, F> {
    /// Stops the take at once and returns the future of its end, without waiting. The pump
    /// sees the stop on its next wake, closes the destination and answers the future with
    /// the [`Recorded`] it holds, or with the [`TakeError`] when the destination failed or
    /// the pump ended without a result. A take with a punch-out that already ended answers
    /// the same way.
    ///
    /// Never blocks and never joins: the pump closes the destination on its own thread.
    ///
    /// A session dropped before the future is answered commits the take in its own drop. On
    /// a `Take` that outlives its session the stop does nothing: that drop already ended it.
    pub fn finish(self) -> Finishing<D, F> {
        self.stop.stop();
        self.into_future()
    }
}

impl<S: InFlight, D: Destination<F>, F: FftFloat> IntoFuture for Take<S, D, F> {
    type Output = Result<Recorded<D, F>, TakeError>;
    type IntoFuture = Finishing<D, F>;

    /// Awaits the take's own end (its punch-out, or its input's removal), without stopping it.
    fn into_future(self) -> Finishing<D, F> {
        Finishing {
            reply: self.reply,
            _stop: self.stop,
        }
    }
}

/// The end of a take ([`Take::finish`], or `take.await`): resolves, once, to what the take
/// holds or to the error that ended it. Dropped before it resolves, it stops the take.
///
/// The pump answers it through a oneshot, so no [`Session::update`] is needed to wake it.
/// The answer is [`TakeError::Dropped`] when the executor dropped the pump without a result.
#[must_use = "a take ends whether or not its end is awaited, but its result is only here"]
pub struct Finishing<D: Destination<F>, F: FftFloat = f32> {
    reply: oneshot::Receiver<Result<Recorded<D, F>, TakeError>>,
    /// Stops the take when this future is dropped, ended or not.
    _stop: StopOnDrop,
}

impl<D: Destination<F>, F: FftFloat> Future for Finishing<D, F> {
    type Output = Result<Recorded<D, F>, TakeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let answered = std::task::ready!(Pin::new(&mut self.reply).poll(cx));
        // A sender dropped unanswered: the executor dropped the pump before it ended.
        Poll::Ready(answered.unwrap_or(Err(TakeError::Dropped)))
    }
}

impl<D: Destination<F>, F: FftFloat> fmt::Debug for Finishing<D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Finishing").finish_non_exhaustive()
    }
}

/// A request to arm a take: record `input` into a [`Destination`], from a [`Punch`].
///
/// [`Session::record`] takes it. The punch is [`Punch::new`] at
/// [`At::NextBlock`] (a take that starts at once) until [`punch`](Self::punch).
/// The destination is a [`ToFile`](tutti_codec::ToFile) (an integer format dithered as its
/// `IntPcm` says), a [`ToMemory`](crate::ToMemory) or a [`ToSink`](crate::ToSink).
///
/// ```no_run
/// # use tutti_session::{Input, Punch, Record};
/// # use tutti_codec::{Encoding, Format, Pcm, ToFile};
/// # use tutti_buffers::FloatWidth;
/// # use tutti_units::Beat;
/// # fn go(input: &Input) {
/// let wav = Format::Wav(Pcm::Float(FloatWidth::F32));
/// let _ = Record::new(input, ToFile::new("vox.wav", Encoding::new(wav)))
///     .punch(Punch::new(Beat::new(16.0)));
/// # }
/// ```
#[derive(Debug)]
pub struct Record<D, F: Float = f32> {
    input: Input<F>,
    destination: D,
    punch: Punch,
}

impl<D: Destination<F>, F: FftFloat> Record<D, F> {
    /// A take of `input` into `destination`, starting at once.
    pub fn new(input: &Input<F>, destination: D) -> Self {
        Self {
            input: *input,
            destination,
            punch: Punch::new(At::NextBlock),
        }
    }

    /// Where the take starts and, if it has one, where it stops.
    #[must_use]
    pub fn punch(mut self, punch: Punch) -> Self {
        self.punch = punch;
        self
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C>
where
    C::Float: FftFloat,
{
    /// Arms the take `request` describes: opens a plan tap on its input's inlet
    /// node and runs the pump into its destination on the session's executor. The take records what the
    /// graph is given by the input, so it is on the graph's clock whatever the
    /// device does; the module docs say where it is placed.
    ///
    /// The pump is a future that awaits the tap's bell: the audio thread rings it as it
    /// writes each block, so the pump runs only when there are frames. It runs on a
    /// thread of its own, whether or not the app calls [`update`](Self::update).
    ///
    /// The tap compiles in the next [`update`](Self::update). The pump drops
    /// what comes before the punch-in; [`Take::rolling`] turns once its first frame
    /// has arrived, and the take's future ([`Take::finish`], or the take awaited) answers
    /// once at the end.
    ///
    /// # Errors
    ///
    /// [`TakeError::NotAnInput`] for a node that is not an input of this
    /// session; [`TakeError::Tap`] when it has left the graph;
    /// [`TakeError::TransportStopped`], [`TakeError::Punch`] and
    /// [`TakeError::Past`] for a punch that cannot be resolved;
    /// [`TakeError::TransportFull`] when the transport refuses the take's
    /// recording; [`TakeError::NotStarted`] when the executor refuses the pump, before any file
    /// or tap exists; [`TakeError::Open`], [`TakeError::Fold`] and [`TakeError::Width`] for
    /// the destination.
    /// Nothing is left open on an error.
    pub fn record<D: Destination<C::Float>>(
        &mut self,
        request: Record<D, C::Float>,
    ) -> Result<Take<Armed, D, C::Float>, TakeError> {
        let Record {
            input,
            destination,
            punch,
        } = request;
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
            map: self.transport.bases.tempo.get(),
            rate,
        };
        let window = resolve(&punch, &anchor, playhead.playing())?;

        // The pump starts first, before any file or tap exists, so a refused start is
        // this call's error and leaves nothing open. It waits for its parts, which come
        // through `parts` once the destination and the tap are open; a take that fails
        // before then stops it, by the stop flag and a ring.
        let (bell, mut listener) = self.background.bells().bell();
        let stop = Arc::new(AtomicBool::new(false));
        let parts: Arc<Mutex<Option<Parts<D, C::Float>>>> = Arc::default();
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
                report.end(ended);
            })
            .map_err(|error| TakeError::NotStarted(error.kind()))?;
        let mut stop = StopOnDrop {
            stop,
            bell,
            recorder: None,
        };

        // The id seeds the file's dither, so it is minted before the open; an open that
        // fails leaves a gap in the ids, which only need to be unique.
        let id = self.transport.handle.next_take_id();
        let writer = destination.open(layout, rate, id.get())?;

        let ports: Vec<_> = (0..width).map(|channel| node.output(channel)).collect();
        // A second of frames: the pump moves what the tap holds each time it wakes.
        let capacity = rate.one_second();
        let reader = self
            .graph
            .taps()
            .open_ports(&ports, TapOptions::new(capacity).align(TapAlign::Raw))?;

        // The transport records from the punch-in to the punch-out, or to the
        // take's end. A start the transport refuses fails the arm; a
        // punch-out it refuses ends the take through the stop that follows.
        let recorder = Recorder::new(self.transport.handle.clone(), id);
        let next = playhead.frame() + playhead.span();
        let when = |frame: Frame| {
            if frame <= next {
                When::NextBlock
            } else {
                When::Frame(frame)
            }
        };
        recorder.start(when(window.start))?;
        stop.recorder = Some(recorder.clone());
        if let Some(end) = window.end {
            recorder.stop_at(when(end))?;
        }
        let shared = Arc::new(Shared::default());
        let pump = Pump {
            reader,
            writer,
            window,
            terms: (lead, offset),
            probe: self.inputs.latency_probe(),
            anchor,
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop.stop),
        };
        let (reply, answer) = oneshot::channel();
        let report = Report::new(reply, self.background.closed(), recorder);
        *parts.lock() = Some((pump, report));
        stop.bell.ring();
        Ok(Take {
            id,
            window,
            shared,
            stop,
            reply: answer,
            state: std::marker::PhantomData,
        })
    }
}

/// What the pump future waits for: the pump and what it reports through.
type Parts<D, F> = (Pump<D, F>, Report<D, F>);

/// Awaits the pump's parts: `None` when the take stopped before they came.
async fn await_parts<D: Destination<F>, F: FftFloat>(
    listener: &mut Listener,
    parts: &Mutex<Option<Parts<D, F>>>,
    stop: &AtomicBool,
) -> Option<Parts<D, F>> {
    loop {
        if let Some(found) = parts.lock().take() {
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

/// The pump of one take: the tap's reader into the destination's writer, between the
/// punch-in and the punch-out, on the graph's frames.
struct Pump<D: Destination<F>, F: FftFloat> {
    reader: TapReader<F>,
    writer: WriterOf<D, F>,
    window: Window,
    /// The inlet's lead and the input's trim.
    terms: (Samples, Trim),
    probe: LatencyProbe,
    anchor: Anchor,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl<D: Destination<F>, F: FftFloat> Pump<D, F> {
    async fn run(
        mut self,
        report: &Report<D, F>,
        stop: &mut Listener,
    ) -> Result<Recorded<D, F>, TakeError> {
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
                // The tap rings as it writes a block and when it ends; the stop
                // bell rings for a stop or the session's end.
                let frames = self
                    .reader
                    .notified()
                    .expect("a session's tap carries the session's bell");
                futures_lite::future::or(frames, stop.rung()).await;
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
                    if !write_silence(&mut self.writer, silence, gap) {
                        failed = true;
                        break 'pump;
                    }
                    written += gap;
                    lost += gap;
                } else {
                    let found = self.place(lo);
                    placement = Some(found);
                    let _ = self.shared.placement.set(found);
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
                if self.writer.write(kept).is_err() {
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
            if write_silence(&mut self.writer, silence, gap) {
                written += gap;
                lost += gap;
            } else {
                failed = true;
            }
        }
        let output = self.writer.close()?;
        if failed {
            return Err(TakeError::SinkRefused);
        }
        // A take finished before its punch-in has nothing to place.
        let placement = placement.ok_or(TakeError::NeverStarted)?;
        Ok(Recorded {
            output,
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
        let latency = TakeLatency::new(input, output, lead).offset(offset);
        let start = latency.place(first);
        TakePlacement {
            first,
            latency,
            start,
            start_beat: self.anchor.beat_at(start),
        }
    }
}

/// Writes `frames` frames of silence, a pass at a time. `false`: the writer
/// refused.
fn write_silence<F: Float>(
    writer: &mut impl Writer<F>,
    silence: Interleaved<'_, F>,
    frames: usize,
) -> bool {
    let mut left = frames;
    while left > 0 {
        let now = left.min(silence.frames().get());
        if writer
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
    use crate::ToSink;
    use crate::take_writer::SinkWriter;
    use tutti_buffers::{Sink, SinkFailed};
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
            self.0.lock().extend_from_slice(frames.samples());
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
        let pump = Pump::<ToSink<Keep>, f32> {
            reader,
            writer: SinkWriter::open(Keep(Arc::clone(&kept)), ChannelLayout::MONO).unwrap(),
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
        let handle = TransportHandle::new(tutti_timeline::Transport::new(SampleRate::SR_48K));
        let id = handle.next_take_id();
        let (reply, _answer) = oneshot::channel();
        let report = Report::new(reply, Arc::default(), Recorder::new(handle, id));
        let executor = tutti_platform::Executor::new();
        let bells = tutti_rt::Bells::new(&executor).expect("the bells' tick task");
        let (_bell, mut listener) = bells.bell();
        // Every frame is in the tap and the punch-out is inside them: the pump runs to
        // its end in one poll, with nothing to await.
        let mut pumping = Box::pin(pump.run(&report, &mut listener));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let Poll::Ready(Ok(recorded)) = pumping.as_mut().poll(&mut cx) else {
            panic!("the pump awaited a tap that held every frame it needed");
        };
        let kept = kept.lock();
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
