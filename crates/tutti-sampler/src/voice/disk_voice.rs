//! Disk streaming sample playback, fed by the streamer thread.
//!
//! Two types, one tier: [`DiskSource`] is the reader of the streamer's ring (the
//! ring, its control cell and its width), and [`DiskVoice`] is a timeline clip
//! over it: it places its read on the transport exactly as the memory tier's
//! placed read does (`interp::place`, the same gate and step, from the block's
//! `Env`) and reads the ring at that position through `LiveRead`: the ring
//! indexed by position, the memory tier's tap layout, the
//! one kernel — so a clip plays what the same clip in memory plays at the same
//! clock frame, and a jump is a crossfade from where it was, not a flush.
//!
//! Every count crossing the ring is in frames. Nothing on the `process`
//! path allocates, locks, or blocks — but a fork's: it reads its file
//! (`offline_read`), which is why a disk voice forks only for an offline
//! render.

use std::ops::Range;
use std::sync::Arc;

use crate::SamplerFloat;
use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_graph::{
    Cx, ForkCause, ForkFaultKind, ForkHealth, ForkSource, Forked, IntoNode, Io, Node, NodeParts,
    Prepare, RenderMode, Shape, Status, declick_frames,
};
use tutti_units::{Amplitude, PlaybackRate, ReadRate, SamplePosition, SampleRate, SrcRatio, Tail};

use super::clock::{BlockClock, Clock};
use super::command::GainError;
use super::fault::FirstError;
use super::interp::{Gate, Place, past_window, place};
use super::live_read::{LiveRead, Out};
use super::memory_source::VoiceWindow;
use super::offline_read::OfflineRead;
use super::types::PlayDirection;
use crate::lanes::{Gather, Lanes, UNPREPARED_FRAMES, audible_gain};
use crate::streamer::{Cells, SharedReader, StatusCell, StreamFile, StreamId, StreamReader};
use tutti_timeline::TempoReader;

/// The streaming tier's bare reader: the streamer's ring, its control cell
/// and its width. A [`DiskVoice`] wraps one and reads the ring by the clock.
///
/// The audio thread holds the ring through a `SharedReader` (an `Arc`) and
/// the ring's one `PosReader`, which this source owns and a clone does not
/// get (`LiveRead`'s module docs): every access is an atomic.
///
/// It is not a graph node: it knows neither where on the timeline it plays
/// nor its file. A [`DiskVoice`] is, and forks.
pub struct DiskSource<F: SamplerFloat> {
    /// Boxed: a voice in a pool should not carry the reader's jump state
    /// inline (it is built on the control thread, where the box is).
    read: Box<LiveRead<F>>,

    /// The stream's scalar cells (speed, stretch, gain, conversion ratio,
    /// underruns): `None` in a severed copy.
    shared_state: Option<Arc<Cells>>,

    /// Output width — the declaration.
    channels: ChannelLayout,
    /// `channels.count()`, cached.
    stride: usize,
}

// Hand-rolled: `read` holds the ring and `shared_state` an `Arc<Cells>`.
impl<F: SamplerFloat> std::fmt::Debug for DiskSource<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskSource")
            .field("has_shared_state", &self.shared_state.is_some())
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat> Clone for DiskSource<F> {
    fn clone(&self) -> Self {
        Self {
            read: self.read.clone(),
            shared_state: self.shared_state.clone(),
            channels: self.channels,
            stride: self.stride,
        }
    }
}

impl<F: SamplerFloat> DiskSource<F> {
    /// A source over `consumer` through its one position `reader` (test rings),
    /// at the ring's own width.
    #[cfg(test)]
    pub(crate) fn new(
        consumer: SharedReader<F>,
        reader: tutti_rt::PosReader<F>,
        shared_state: Arc<Cells>,
    ) -> Self {
        let width =
            crate::voice_width(consumer.channels()).expect("a test ring's width is a voice's");
        let place = Place::for_topology(consumer.topology(), width)
            .expect("a test ring is placeable at its own width");
        let control = consumer.control_reader();
        Self::with_reader(
            consumer,
            Some(crate::streamer::RingReader::new(reader, control)),
            shared_state,
            width,
            place,
        )
    }

    /// A source over `consumer` through its one `reader` and the reader of its
    /// map (`None`: silent), `width` wide (at most [`MAX_CHANNELS`],
    /// the widest frame the read path stacks), placing the ring's channels by
    /// `place`: the [`Place`] of the ring's topology into `width`
    /// ([`Place::for_topology`], built, and so refusing, when the stream was
    /// opened). The file's own width and order give the identity.
    pub(crate) fn with_reader(
        consumer: SharedReader<F>,
        reader: Option<crate::streamer::RingReader<F>>,
        shared_state: Arc<Cells>,
        width: ChannelLayout,
        place: Place<F>,
    ) -> Self {
        let channels = width;
        let stride = channels.count() as usize;
        Self {
            read: Box::new(LiveRead::new(consumer, reader, stride, place)),
            shared_state: Some(shared_state),
            channels,
            stride,
        }
    }

    /// Output width — this unit's `outputs()`.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Forgets the position and any fade: the next block starts afresh.
    pub fn reset_interpolation(&mut self) {
        self.read.reset();
    }
}

impl<F: SamplerFloat> DiskSource<F> {
    /// Let go of the live stream: the ring's reader, and
    /// the control cell. A copy severed so never touches the live stream.
    fn sever(&mut self) {
        self.shared_state = None;
        self.reset_interpolation();
        self.read.sever();
    }

    /// Output width, as a count.
    #[inline]
    fn width(&self) -> usize {
        self.stride
    }
}

// ---------------------------------------------------------------------------
// DiskVoice — a timeline clip over a stream.
//
// Per frame: the placement (`interp::place`, the memory tier's) gives the
// position the transport puts the playhead at inside the window, `None`
// outside it or on a standing transport; the ring is read there. The streamer
// fills ahead of where the voice reads; a jump is the reader's own crossfade
// (`LiveRead`).
// ---------------------------------------------------------------------------

/// Wiring for [`DiskVoice::new`]: the window plus the file sample rate.
/// `shared_state` stays a separate wiring arg (it must be the same `Cells`
/// the `inner` unit holds). The transport is not wiring: a voice reads it
/// from each block's `Env`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiskVoiceConfig {
    /// Span of timeline this voice occupies. Mirrors `MemorySource` — see
    /// [`VoiceWindow`].
    pub window: VoiceWindow,
    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    ///
    /// The rate the streamer recorded from the file's header when it opened the
    /// stream (`StreamReader::file_rate`), not one recovered from the session
    /// rate: that one moves on a device restart, and the file's does not.
    pub file_sample_rate: SampleRate,
}

/// A `DiskSource`'s stream, placed on the timeline.
///
/// A timeline clip must sound only while the playhead is inside its
/// `[start, start + duration)` window, and there play the file frame the
/// transport puts the playhead on. This does exactly what the memory tier's
/// placed read does — the same gate (`window_position`)
/// placed and stepped the same way (`interp::place`), from the block's `Env`
/// — and reads the ring at the position that gives, so live, forked and
/// in-memory voices of one clip play the same samples at the same frame.
///
/// # A fork reads the file itself
///
/// A fork for an offline render (an export) cannot read the ring: its reads
/// would move the live voice's window. So the fork (`fork_copy`,
/// a graph fork's source for this node and for a `VoiceNode` or pool holding
/// it) is cut off from the stream, and handed the stream's file, as the
/// streamer records it (path, rate, loop), to read on demand on the render's
/// thread. It plays the same window of the file on the render's transport
/// (it reads its render's `Env`), read as the memory tier reads it; see
/// `offline_read`. That path blocks on file I/O and is never taken live: a
/// live fork of a disk voice is refused.
///
/// # Under a stretch it sounds on time
///
/// A stretched or transposed disk voice runs a
/// [`PhaseVocoder`](tutti_dsp::PhaseVocoder) seated on the file's own history: it
/// primes ahead of its window, from the ring, and sounds from the frame its place on
/// the timeline falls on, with no latency (see `stretched`). The ring follows the position
/// the voice reads, so the history before a seat is what the streamer fetches first; a
/// frame the ring has not yet got is a counted underrun and reads as silence into the
/// vocoder, so the voice sounds on time with a gap, and never waits. A loop wrap is a
/// seek.
pub struct DiskVoice<F: SamplerFloat> {
    inner: DiskSource<F>,
    /// The playback controls: the streamer's cell, shared with `inner`, while
    /// live; a private snapshot of it in a fork.
    shared_state: Arc<Cells>,

    /// Span of timeline this voice occupies.
    window: VoiceWindow,

    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    file_sample_rate: SampleRate,

    /// The stream this voice consumes, as what a fork reads: the file it
    /// plays and whether the stream is still served. `None` for a voice not
    /// built by [`DiskVoice::from_reader`] (a test's bare ring), whose fork
    /// then plays silence.
    origin: Option<Origin<F>>,

    /// `Some` in a fork ([`fork_copy`](Self::fork_copy)): it never touches
    /// the ring or the streamer, and plays what this holds instead. Boxed: a
    /// live voice (every voice in a pool) should not carry the pages' room,
    /// and it is built on the control thread, where a fork is taken.
    offline: Option<Box<Offline<F>>>,

    /// The rate `prepare` last gave this voice, `None` until one did: the
    /// rate the step converts to. A fork never told a rate renders nothing
    /// and says so rather than play off pitch; a live voice never told one
    /// steps by the streamer's conversion for the session rate.
    sample_rate: Option<SampleRate>,
    /// The live read's fade length in frames: `DECLICK` at `sample_rate`, or at
    /// the file's rate until a rate is given.
    declick: usize,

    /// The render mode `prepare` last gave this voice: a fork reads its file
    /// on the render's thread, which only an offline render allows.
    render_mode: RenderMode,

    /// The transport as this voice reads it when it is a graph node of its
    /// own (a voice in a pool or a `VoiceNode` reads its owner's).
    clock: Clock,

    /// The project's tempo map, when this voice reads one: lent once per
    /// block to its clock ([`tempo`](Self::tempo)).
    tempo: Option<TempoReader>,

    /// A block's positions when this voice is a graph node of its own,
    /// sized from `Prepare::max_block` in `prepare` (empty until then and in
    /// a clone, so a voice that only ever plays in a pool or a `VoiceNode`,
    /// which read through their own scratch, carries none).
    positions: Vec<Option<SamplePosition>>,
}

/// What a live voice keeps of its stream for a fork to read.
///
/// Read-only: nothing reachable through it can drive the stream. The file is
/// immutable and the status says whether the streamer's record is gone; the
/// loop as applied and the direction come from the ring's published control,
/// when the fork is taken (a loop set later on the stream is not a copy this
/// would have missed).
#[derive(Clone)]
struct Origin<F: SamplerFloat> {
    file: Arc<StreamFile<F>>,
    status: Arc<StatusCell>,
}

/// A forked disk voice's own playback: the file it reads, the direction it
/// reads in, and where its failures go.
#[derive(Clone, Debug, Default)]
struct Offline<F: SamplerFloat> {
    /// The direction the stream played in when the fork was taken.
    direction: PlayDirection,
    /// The file, from the streamer's record when the fork was taken. `None`
    /// when the stream was gone by then: silence.
    read: Option<OfflineRead<F>>,
    /// The first failure since this copy was forked (its stream gone, its
    /// file unreadable, no render rate), what the fork's health probe
    /// reports ([`fault`](DiskVoice::fault)). Fresh at every fork, so a copy
    /// never reports another's.
    fault: Arc<FirstError>,
}

/// Why a forked disk voice renders silence where its file should be, other
/// than the file itself (`OfflineReadError`).
#[derive(Debug, thiserror::Error)]
enum OfflineFault {
    /// Its stream ended (stopped, or its channel restarted on another file)
    /// before the copy was forked.
    #[error(
        "its disk stream had ended (stopped, or its channel restarted on another \
                 file) when it was forked, so there was no file to play"
    )]
    StreamGone,
    /// It was asked to render before it was prepared with a sample rate.
    #[error("it was rendered before it was given a sample rate")]
    NoRenderRate,
    /// It was prepared for a realtime render, where its file read would block
    /// the audio thread.
    #[error("it was prepared for a realtime render, and its file read blocks")]
    PreparedRealtime,
}

/// Why a disk voice refused a fork.
#[derive(Debug, thiserror::Error)]
#[error(
    "a disk voice forks only for an offline render: a realtime fork cannot read the \
             stream's ring, and reading its file would block the audio thread"
)]
pub(crate) struct RealtimeDiskFork;

/// A forked voice's failure latch, as the fork's health probe: a failure it
/// latched is [`ForkFaultKind::Failed`].
pub(crate) struct VoiceHealth(pub(crate) Arc<FirstError>);

impl ForkHealth for VoiceHealth {
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)> {
        self.0
            .fault()
            .map(|e| (ForkFaultKind::Failed, ForkCause::from_arc(e)))
    }
}

impl<F: SamplerFloat> std::fmt::Debug for DiskVoice<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoice")
            .field("inner", &self.inner)
            .field("window", &self.window)
            .field("file_sample_rate", &self.file_sample_rate)
            .field("has_origin", &self.origin.is_some())
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat> Clone for DiskVoice<F> {
    /// A copy sharing the stream (the ring, without its reader: see
    /// `LiveRead`) and the control cell. Not a fork: see
    /// `fork_copy`.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared_state: Arc::clone(&self.shared_state),
            window: self.window,
            file_sample_rate: self.file_sample_rate,
            origin: self.origin.clone(),
            offline: self.offline.clone(),
            sample_rate: self.sample_rate,
            render_mode: self.render_mode,
            declick: self.declick,
            clock: Clock::new(),
            tempo: self.tempo.clone(),
            // Unsized, so a clone stays allocation-free as it was: a copy
            // inserted as a node is prepared, which sizes it.
            positions: Vec::new(),
        }
    }
}

impl<F: SamplerFloat> DiskVoice<F> {
    /// Places a `DiskSource`'s stream on the timeline.
    ///
    /// Construction (streamer stream registration, ring allocation) happens on
    /// the ECS/streamer side; this only binds the already-built unit to a
    /// timeline window. `shared_state` must be the same `Cells` the `inner`
    /// unit holds.
    pub(crate) fn new(
        inner: DiskSource<F>,
        shared_state: Arc<Cells>,
        config: DiskVoiceConfig,
    ) -> Self {
        Self {
            inner,
            shared_state,
            window: config.window,
            file_sample_rate: config.file_sample_rate,
            origin: None,
            offline: None,
            sample_rate: None,
            render_mode: RenderMode::Realtime,
            declick: declick_frames(config.file_sample_rate).get(),
            clock: Clock::new(),
            tempo: None,
            positions: Vec::new(),
        }
    }

    /// Builds the voice that plays a stream, placed on the timeline at
    /// `window` (the transport it reads is its block's, from the graph). The reader is the stream's one, moved in: it is the
    /// value of the adopt ([`DiskStreamer::adopt`](crate::DiskStreamer::adopt)),
    /// so it cannot be built into a second voice.
    ///
    /// The window is musical time; the gate converts them to
    /// file frames using the file's own rate ([`StreamReader::file_rate`], the
    /// rate the streamer recorded when it opened the stream), so a session
    /// rate that moves later (a device restart) leaves the gate's frames where
    /// they were. The voice is as wide as the open asked for, and plays the
    /// file's channels where they are unless the file's order is not the
    /// engine's (a WAV 7.1): that was permuted to it when the stream opened.
    ///
    /// The voice also keeps the stream's file, and whether the stream is still
    /// served, so a fork of it for an offline render (an export) can read the
    /// same file on its own rather than through the live ring; see
    /// `DiskVoice::fork_copy`.
    ///
    /// Control thread; the returned voice is then moved into the graph.
    pub fn from_reader(reader: StreamReader<F>, window: VoiceWindow) -> Self {
        let StreamReader {
            ring,
            reader,
            cells,
            file,
            status,
            place,
            width,
            ..
        } = reader;
        let file_rate = file.file_rate();
        let inner = DiskSource::with_reader(ring, Some(reader), Arc::clone(&cells), width, place);
        let mut voice = Self::new(
            inner,
            cells,
            DiskVoiceConfig {
                window,
                file_sample_rate: file_rate,
            },
        );
        voice.origin = Some(Origin { file, status });
        voice
    }

    /// The streamer stream this voice reads: its ring's stream, which a loop
    /// forwarded for the voice names so the streamer ignores it once the
    /// stream is gone.
    pub(crate) fn stream(&self) -> StreamId {
        self.inner.read.ring().region_id()
    }

    /// The voice's window on the timeline.
    pub fn window(&self) -> VoiceWindow {
        self.window
    }

    /// A copy of this voice for an offline render, sharing nothing with
    /// it: cut off from the stream (no ring reader, no control cell — a
    /// private snapshot of the controls' values, `Cells::detached`), and
    /// handed the stream's file to read itself, with the loop and the
    /// direction the streamer has applied now (read from the ring's published
    /// control: a loop it downgraded is the downgrade, not the request). A
    /// stream that has ended by now is a latched failure
    /// ([`fault`](Self::fault)): the export fails naming the voice rather than
    /// write its silence.
    ///
    /// Reads the ring's control cell (a lock, and an owning `Arc` that frees
    /// here): control thread only, as every fork is.
    pub(crate) fn fork_copy(&self) -> Self {
        let mut copy = self.clone();
        let applied = self.inner.read.ring().applied();
        copy.inner.sever();
        copy.shared_state = Arc::new(self.shared_state.detached());
        copy.clock = Clock::new();
        let fault = Arc::new(FirstError::default());
        let (read, direction) = match &self.origin {
            Some(origin) if origin.status.closed() => {
                fault.latch(OfflineFault::StreamGone);
                (None, applied.direction)
            }
            Some(origin) => (
                Some(OfflineRead::new(
                    Arc::clone(&origin.file),
                    applied.loop_,
                    Arc::clone(&fault),
                )),
                applied.direction,
            ),
            // A fork of a fork keeps the file it was handed; a voice over a
            // bare ring has none.
            None => match self.offline.as_ref() {
                Some(o) => (
                    o.read
                        .clone()
                        .map(|read| read.relatched(Arc::clone(&fault))),
                    o.direction,
                ),
                None => (None, applied.direction),
            },
        };
        copy.origin = None;
        copy.offline = Some(Box::new(Offline {
            read,
            direction,
            fault,
        }));
        copy
    }

    /// The loop a fork plays: the loop the stream had applied when it was
    /// forked (tests). `None` live, or for a fork with no file.
    #[cfg(test)]
    pub(crate) fn forked_loop(&self) -> Option<Option<super::Loop>> {
        self.offline
            .as_ref()?
            .read
            .as_ref()
            .map(OfflineRead::loop_setting)
    }

    /// A fork's failure latch; `None` live.
    pub(crate) fn fault(&self) -> Option<Arc<FirstError>> {
        self.offline.as_ref().map(|o| Arc::clone(&o.fault))
    }

    /// Tells the stream how fast a wrapping time-stretcher wants its source.
    ///
    /// `1 / stretch`, or [`ReadRate::UNITY`] when nothing wraps this voice. See
    /// `Cells::read_rate` for why the factor lands there rather than at the
    /// per-sample advance.
    ///
    /// Callers must publish unity when they stop stretching. The rate
    /// belongs to the filter, not to the voice, so a voice returned to 1.0x
    /// that never re-published would keep reading at the old factor. Both
    /// branches of the pool's read set it every block for exactly that reason.
    #[inline]
    pub fn set_stretch_rate(&self, rate: ReadRate) {
        self.shared_state.set_stretch_rate(rate);
    }

    /// Moves the voice's window. The next frame reads at the new window.
    pub fn set_window(&mut self, window: VoiceWindow) {
        self.window = window;
    }

    /// Publishes a new output gain. `&self`: the write lands in the shared
    /// `Cells`, so no exclusivity is needed.
    ///
    /// # Errors
    ///
    /// [`GainError::NotFinite`] for a NaN or infinite gain; the voice keeps the
    /// gain it had.
    pub fn set_gain(&self, gain: Amplitude) -> Result<(), GainError> {
        GainError::check(gain)?;
        self.store_gain(gain);
        Ok(())
    }

    /// [`set_gain`](Self::set_gain) for a gain already checked: a store, so the
    /// audio thread's drain can apply what a control-side entry passed.
    pub(crate) fn store_gain(&self, gain: Amplitude) {
        self.shared_state.set_gain(gain);
    }

    /// The current output gain, read from the shared state — so a clone reports
    /// what the frontend last published, not what it was cloned at.
    pub fn gain(&self) -> Amplitude {
        self.shared_state.gain()
    }

    /// The file's own sample rate (the gate's rate: it measures the clock in
    /// the file's frames).
    pub fn file_sample_rate(&self) -> SampleRate {
        self.file_sample_rate
    }

    /// Sets the playback speed magnitude, in the stream's `Cells`. The next block reads at the
    /// position the new speed gives, crossfaded.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.shared_state.set_speed(speed);
    }

    /// Output width, as a count.
    #[inline]
    pub(crate) fn width(&self) -> usize {
        self.inner.width()
    }

    /// The rate the gate converts the clock at, relative to the file's frames:
    /// varispeed and the stretcher's rate, no conversion (the gate measures in
    /// the file's own frames).
    #[inline]
    fn window_rate(&self) -> ReadRate {
        self.shared_state
            .effective_speed()
            .read_rate(SrcRatio::UNITY)
            .then(self.shared_state.stretch_rate())
    }

    /// File frames per output frame: varispeed and the stretcher's rate, with
    /// the conversion from the file's rate to the rate this unit renders at —
    /// derived from the two rates, as the memory tier derives it. A live voice
    /// never told a rate takes the streamer's conversion for the session.
    #[inline]
    fn step_rate(&self) -> ReadRate {
        self.step_at(self.shared_state.stretch_rate())
    }

    /// File frames per frame a time stretcher's vocoder takes in: varispeed
    /// and the conversion, without the stretch (the vocoder's own consumption
    /// supplies it).
    #[inline]
    pub(crate) fn input_step(&self) -> ReadRate {
        self.step_at(ReadRate::UNITY)
    }

    /// [`step_rate`](Self::step_rate) at the stretcher's rate `stretch`.
    #[inline]
    fn step_at(&self, stretch: ReadRate) -> ReadRate {
        let speed = self.shared_state.effective_speed();
        match self.sample_rate {
            Some(render_rate) => speed
                .read_rate(SrcRatio::for_rates(self.file_sample_rate, render_rate))
                .then(stretch),
            None if self.offline.is_some() => ReadRate::UNITY,
            None => self.shared_state.read_rate(),
        }
    }

    /// The gate: this voice's window, in the file's frames, at the window
    /// rate.
    #[inline]
    pub(crate) fn gate(&self) -> Gate {
        Gate {
            window: self.window,
            source_rate: self.file_sample_rate,
            rate: self.window_rate(),
        }
    }

    /// Render block frames `range` (at most the lanes' length) into frames
    /// `0..range.len()` of the first `n` lanes: the voice's own channels,
    /// then silence on any lane past them. The live read claims the ring
    /// once per run of the block (a transport change or a loop wrap starts a
    /// run); a fork reads its file frame by frame. The block read of a slot
    /// holding this voice (`PlaybackSlot::render_lanes`), through the
    /// owner's position scratch (`positions`, at least `range.len()` long).
    ///
    /// Returns `false` when no frame of a live voice has a position (its
    /// window is closed, or the transport stands): every frame is +0.0, the
    /// lanes are not written and the ring is not read. The reader's
    /// state is left as a block of silent frames leaves it, so the next
    /// sounding block starts where it would have had the silence been
    /// rendered. A fork always returns `true`.
    pub(crate) fn render_lanes(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        lanes: &mut Lanes<F>,
        n: usize,
        positions: &mut [Option<SamplePosition>],
        gather: &mut Gather<F>,
    ) -> bool {
        let w = self.width().min(n);
        let frames = range.len();
        if self.offline.is_some() {
            lanes.clear(n, frames);
            self.offline_render(clock, range, positions, |i, f| lanes.put(i, &f[..w]));
            return true;
        }
        let sounds = self.live_place(clock, range.clone(), positions);
        if !sounds && !force_no_skip() {
            self.inner.read.silence();
            return false;
        }
        lanes.clear(n, frames);
        let mut out: Out<'_, fn(usize, &[F]), F> = Out::Lanes {
            lanes: &mut lanes.lanes_mut()[..w],
            gather,
        };
        self.live_read(clock, range, positions, &mut out);
        true
    }

    /// Render live frames `range` without the silent-block skip (the
    /// callers that always render), frame `i - range.start` handed to `emit`:
    /// the placed positions, read from the ring,
    /// one ring read per run (its generation the read's segment, so a jump
    /// is crossfaded). `positions` is scratch, at least `range.len()` long.
    fn live_render<E: FnMut(usize, &[F])>(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        out: &mut Out<'_, E, F>,
    ) {
        self.live_place(clock, range.clone(), positions);
        self.live_read(clock, range, positions, out);
    }

    /// The straight positions of live frames `range` in `positions[..len]`;
    /// whether any frame has one. The compiler aligns the voice in the graph:
    /// the stream reads no earlier or later than its clock.
    fn live_place(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
    ) -> bool {
        let positions = &mut positions[..range.len()];
        place(clock, range, self.gate(), self.step_rate(), positions)
    }

    /// Read the ring at the positions [`live_place`](Self::live_place) left,
    /// one read per run of `range`.
    fn live_read<E: FnMut(usize, &[F])>(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &[Option<SamplePosition>],
        out: &mut Out<'_, E, F>,
    ) {
        let rate = self.step_rate();
        let declick = self.declick;
        let gain = self.shared_state.gain();
        // Borrowed beside `inner`, not cloned: `self` holds a strong
        // reference, so a clone only adds two atomic refcount updates.
        let state = &*self.shared_state;
        let base = range.start;
        for run in clock.runs_in(range) {
            let (a, b) = (run.start, run.end);
            self.inner.read.render(
                &positions[a - base..b - base],
                run.generation,
                rate,
                declick,
                gain,
                state,
                a - base,
                out,
            );
        }
    }

    /// A fork's frames `range`, frame `i - range.start` handed to `emit`: the
    /// placement is the live one, and where the live voice reads the ring, this
    /// reads the file at the placed position. `positions` is scratch, at least
    /// `range.len()` long.
    fn offline_render(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        emit: impl FnMut(usize, &[F]),
    ) {
        let frames = range.len();
        self.offline_place(clock, range, &mut positions[..frames]);
        self.offline_read(&positions[..frames], emit);
    }

    /// A fork's placement of frames `range`, as the live voice's: the straight
    /// positions in `positions` (as many as `range`). Once the playhead is past
    /// the window's end the file is closed.
    fn offline_place(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
    ) {
        let gate = self.gate();
        let rate = self.step_rate();
        place(clock, range.clone(), gate, rate, positions);
        if past_window(clock, range, gate)
            && let Some(read) = self.offline.as_mut().and_then(|o| o.read.as_mut())
        {
            read.close();
        }
    }

    /// A fork's file read at `positions`, frame `i` handed to `emit`.
    fn offline_read(
        &mut self,
        positions: &[Option<SamplePosition>],
        mut emit: impl FnMut(usize, &[F]),
    ) {
        let w = self.width().min(MAX_CHANNELS);
        let silent = [F::ZERO; MAX_CHANNELS];
        let gain = audible_gain::<F>(self.shared_state.gain());
        let told_rate = self.sample_rate.is_some();
        let Some(offline) = self.offline.as_mut() else {
            (0..positions.len()).for_each(|i| emit(i, &silent[..w]));
            return;
        };
        let direction = offline.direction;
        let mut frame = [F::ZERO; MAX_CHANNELS];
        for (i, pos) in positions.iter().enumerate() {
            let Some(pos) = pos else {
                emit(i, &silent[..w]);
                continue;
            };
            // A copy never told a rate latches, inside its window.
            if !told_rate {
                offline.fault.latch_static(&OfflineFault::NoRenderRate);
                emit(i, &silent[..w]);
                continue;
            }
            // Its read blocks on file I/O, which a realtime render never waits on.
            if !self.render_mode.is_offline() {
                offline.fault.latch_static(&OfflineFault::PreparedRealtime);
                emit(i, &silent[..w]);
                continue;
            }
            match offline.read.as_mut() {
                Some(read) => {
                    read.read_into(*pos, direction, self.inner.read.place(), &mut frame[..w]);
                }
                None => frame[..w].fill(F::ZERO),
            }
            for s in frame[..w].iter_mut() {
                *s *= gain;
            }
            emit(i, &frame[..w]);
        }
    }

    /// The positions the transport puts a stretching voice's output frames
    /// `range` at, in `positions` (as many as `range`): where its read seats.
    /// Whether any frame has one.
    pub(crate) fn seat_positions(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
    ) -> bool {
        let positions = &mut positions[..range.len()];
        if self.offline.is_some() {
            self.offline_place(clock, range, positions);
            positions.iter().any(Option::is_some)
        } else {
            self.live_place(clock, range, positions)
        }
    }

    /// Reads the voice at `positions` (`None`: silence) into the first `n`
    /// lanes, frame `i` into frame `i`, gained, in the read `generation` (a
    /// new one is a jump wherever it lands). The vocoder's input: positions
    /// that follow one another by [`input_step`](Self::input_step), apart
    /// from the clock.
    pub(crate) fn read_at(
        &mut self,
        positions: &[Option<SamplePosition>],
        generation: u64,
        lanes: &mut Lanes<F>,
        n: usize,
        gather: &mut Gather<F>,
    ) {
        let w = self.width().min(n);
        let frames = positions.len();
        lanes.clear(n, frames);
        if self.offline.is_some() {
            self.offline_read(positions, |i, f| lanes.put(i, &f[..w]));
            return;
        }
        let rate = self.input_step();
        let declick = self.declick;
        let gain = self.shared_state.gain();
        let state = &*self.shared_state;
        let mut out: Out<'_, fn(usize, &[F]), F> = Out::Lanes {
            lanes: &mut lanes.lanes_mut()[..w],
            gather,
        };
        self.inner.read.render(
            positions, generation, rate, declick, gain, state, 0, &mut out,
        );
    }

    /// A stretching voice's piece in which nothing sounds: the ring's reader
    /// claims nothing, as for a silent block of [`render_lanes`](Self::render_lanes).
    pub(crate) fn idle(&mut self) {
        self.inner.read.silence();
    }

    /// Run at `sample_rate` under `mode`: the step converts the file's rate to
    /// it, and a fork reads its file only when `mode` is offline. What
    /// [`Node::prepare`] does; allocation-free.
    pub(crate) fn set_render(&mut self, sample_rate: SampleRate, mode: RenderMode) {
        self.sample_rate = Some(sample_rate);
        self.render_mode = mode;
        self.declick = declick_frames(sample_rate).get();
    }

    /// Forget the read's jump state (a slot's flush at a jump, a reset).
    pub(crate) fn flush(&mut self) {
        self.inner.reset_interpolation();
    }

    /// Block frames `range` of this voice as a node of its own into `out`
    /// (its first `out.len()` channels), through `positions` (at least
    /// `range.len()` long).
    fn render_piece(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        out: &mut [&mut [F]],
    ) {
        let base = range.start;
        let emit = |i: usize, f: &[F]| {
            for (ch, &s) in out.iter_mut().zip(f) {
                ch[base + i] = s;
            }
        };
        if self.offline.is_some() {
            self.offline_render(clock, range, positions, emit);
        } else {
            self.live_render(clock, range, positions, &mut Out::Frames(emit));
        }
    }

    /// A block longer than its positions, through a stack array in pieces
    /// of [`UNPREPARED_FRAMES`]: only a voice driven by hand without a
    /// `prepare` (no positions) or past the `max_block` it was prepared for
    /// reaches this, so the prepared render never initialises the array.
    #[cold]
    #[inline(never)]
    fn render_unprepared(&mut self, clock: &BlockClock<'_>, frames: usize, out: &mut [&mut [F]]) {
        let mut positions = [None; UNPREPARED_FRAMES];
        let mut from = 0;
        while from < frames {
            let to = (from + UNPREPARED_FRAMES).min(frames);
            self.render_piece(clock, from..to, &mut positions, out);
            from = to;
        }
    }
}

impl<F: SamplerFloat> DiskVoice<F> {
    /// This voice reading the project's tempo map through `tempo`
    /// ([`TransportSettings::tempo_reader`](tutti_timeline::TransportSettings::tempo_reader)).
    /// Over a map whose tempo changes, how long a placed voice has played is
    /// the map's seconds between its start and the playhead, not the beats
    /// over the tempo of the moment; see
    /// [`VoicePool::tempo`](super::VoicePool::tempo). It matters
    /// only when the voice is a graph node of its own (a voice in a pool or a `VoiceNode` reads its owner's). A fork reads the same map.
    #[must_use]
    pub fn tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }
}

impl<F: SamplerFloat> Node<F> for DiskVoice<F> {
    /// No inputs, the file's width out; a source, never skipped. It plays its
    /// window of the transport, as a placed memory voice does, so it has no
    /// ring-out.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.inner.channels())
            .with_tail(Tail::None)
            // A `DiskSource`'s read, placed on the transport as a memory
            // voice is: `Moderate` for the same reason.
            .with_cost(tutti_graph::Cost::Moderate)
    }

    /// The render rate, and the block's position scratch sized from
    /// `max_block` (a re-prepare at another size resizes it).
    fn prepare(&mut self, p: &Prepare) {
        self.set_render(p.sample_rate(), p.render_mode());
        let frames = p.max_block().get();
        self.positions.clear();
        self.positions.resize(frames, None);
        self.positions.shrink_to_fit();
    }

    /// The block in one piece through the positions `prepare` sized; a
    /// voice driven by hand unprepared (or past its `max_block`) through
    /// `render_unprepared`'s stack pieces.
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        // Lent for the block and given back after it: the render borrows the
        // whole voice.
        let mut reader = self.tempo.take();
        let map = reader.as_mut().map(TempoReader::lend);
        let clock = self.clock.observe_with(cx.env, map.as_deref());
        let frames = io.frames();
        let (_, mut outs) = io.split();
        let n = self.width().min(outs.len()).min(MAX_CHANNELS);
        for c in n..outs.len() {
            outs.get(c).fill(F::ZERO);
        }
        let mut refs: [&mut [F]; MAX_CHANNELS] = std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        if frames <= self.positions.len() {
            let mut positions = std::mem::take(&mut self.positions);
            self.render_piece(&clock, 0..frames, &mut positions, &mut refs[..n]);
            self.positions = positions;
        } else {
            self.render_unprepared(&clock, frames, &mut refs[..n]);
        }
        drop(map);
        self.tempo = reader;
        Status::Modified
    }

    /// The read's jump state forgotten, the transport forgotten.
    fn reset(&mut self) {
        self.flush();
        self.clock.reset();
    }
}

/// The handle a host keeps on a [`DiskVoice`] inserted as a node of its
/// own: the stream's playback controls (its cells, which the voice reads once
/// per block). Control thread. The stream's direction and loop are not here:
/// they are the streamer's, set through [`StreamHandle`](crate::StreamHandle).
#[derive(Clone)]
pub struct DiskVoiceControls {
    state: Arc<Cells>,
}

impl std::fmt::Debug for DiskVoiceControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoiceControls")
            .field("gain", &self.state.gain())
            .finish_non_exhaustive()
    }
}

impl DiskVoiceControls {
    /// Publishes a new output gain, as
    /// [`DiskVoice::set_gain`](DiskVoice::set_gain).
    ///
    /// # Errors
    ///
    /// [`GainError::NotFinite`] for a NaN or infinite gain.
    pub fn set_gain(&self, gain: Amplitude) -> Result<(), GainError> {
        GainError::check(gain)?;
        self.state.set_gain(gain);
        Ok(())
    }

    /// The current output gain.
    pub fn gain(&self) -> Amplitude {
        self.state.gain()
    }

    /// Sets the playback speed (varispeed).
    pub fn set_speed(&self, speed: PlaybackRate) {
        self.state.set_speed(speed);
    }
}

/// A disk voice's fork: [`DiskVoice::fork_copy`] of a template that shares
/// the stream's file, status and cells with the live voice (never its ring
/// reader), taken when the fork is.
struct DiskVoiceFork<F: SamplerFloat>(DiskVoice<F>);

impl<F: SamplerFloat> ForkSource<F> for DiskVoiceFork<F> {
    fn fork(&self, mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        if matches!(mode, RenderMode::Realtime) {
            return Err(ForkCause::new(RealtimeDiskFork));
        }
        let fork = self.0.fork_copy();
        let health = fork.fault().map(VoiceHealth);
        let forked = Forked::new(Box::new(fork));
        Ok(match health {
            Some(h) => forked.with_health(Arc::new(h)),
            None => forked,
        })
    }
}

impl<F: SamplerFloat> IntoNode<F> for DiskVoice<F> {
    type Controls = DiskVoiceControls;

    fn into_parts(self) -> NodeParts<DiskVoiceControls, F> {
        let controls = DiskVoiceControls {
            state: Arc::clone(&self.shared_state),
        };
        let fork = DiskVoiceFork(self.clone());
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod live_loop;

// Whether a test has switched the silent-block skip off (see
// `DiskVoice::render_lanes`), so it can compare against the unskipped
// render. The audio thread's code reads a constant `false` outside tests.
#[cfg(test)]
thread_local! {
    pub(crate) static NO_SKIP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn force_no_skip() -> bool {
    NO_SKIP.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn force_no_skip() -> bool {
    false
}

#[cfg(test)]
mod tests;
