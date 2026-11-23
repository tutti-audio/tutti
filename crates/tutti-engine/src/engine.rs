//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the graph's
//! [`Executor`] and the transport, and renders one
//! output buffer per block. It folds the graph's outputs to the device width, keeps
//! the declick, and applies timestamped transport commands on their frame.
//!
//! # Timestamped transport commands
//!
//! [`MotionFsm::schedule`] queues a play, stop, seek, tempo or loop change at
//! an [`At`]. Each block, the engine walks the commands due in it in time
//! order and **cuts the block's transport** at each one's frame: the pieces
//! before and after run under different transports. The executor never
//! splits a block: the engine advances its own clock piece by
//! piece, records each cut as a
//! [`TransportChange`](tutti_graph::TransportChange) in the block's `Env`,
//! and renders the whole block once. A node reads the transport at a frame
//! with `Env::transport_at`, and the graph's own `At::Beat` commands resolve
//! against the piece that reaches their beat.
//!
//! A block holds at most
//! [`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES) cuts. A command due past
//! that lands at the start of the next block and is counted late, like any
//! command already past due ([`MotionFsm::late_command_count`]). (`At::NextBlock`
//! commands, late ones and crossed beats all land at the block's first frame
//! and need no cut, so they are never deferred.) Commands due on one frame
//! apply in send order.
//!
//! **A tempo map of steps cuts the block too**: each step is
//! a change on the frame it falls on, taken beside the commands. It is not a
//! command, so it is never deferred: a block that needs more changes than it
//! holds ends before the one that does not fit, and the rest renders as the
//! next block, exactly (the walk is a function of the frame, not of where
//! blocks begin). A constant map has no step and none of this runs.
//!
//! **Untimed state is read once per block.** The tempo, loop and play state
//! are read at the start of the walk; a store from the control thread during
//! the block lands at the next one, and only an applied command changes them
//! at a cut.
//!
//! **Beats** are resolved with the graph's rule (`tutti_graph::Env::due`):
//! the first frame at or after the beat that playback reaches, the piece's
//! own transport deciding. A beat continuous playback already crossed is late;
//! one a seek or loop jumped over waits (`tutti_graph::BeatCrossings`).
//!
//! **The declick is audio only, and its fade-out ends on the command's
//! frame.** A declick stop or seek moves the transport on its command's
//! frame, as an immediate one does. The fade is the engine's, as a gain on
//! the output along [`FadeLaw::EqualAmplitude`]'s one side, continuous at
//! every frame:
//!
//! - **A timed command** (`At::Frame`, `At::Beat`) is seen ahead: the walk
//!   looks one fade ([`DECLICK`](tutti_graph::DECLICK)) past each block for the next declicked
//!   command playback reaches, and the gain falls on the *old*
//!   position's audio to reach zero exactly on its frame. Seen with less
//!   than a fade to go (sent late, or reached sooner after a tempo change),
//!   it falls over whatever frames are left: steeper, still continuous.
//! - **On the frame** the transport jumps or stops, sample-accurately, and
//!   the gain rises from zero over one fade ([`DECLICK`](tutti_graph::DECLICK)): the new position's audio after
//!   a seek, and after a stop whatever still sounds while stopped (live
//!   input, a reverb tail; transport-gated sources are already silent), so
//!   the output never comes back with a step.
//! - **With no lead time** (`At::NextBlock`, an untimed `send`, a late
//!   command) the jump is at the block's first frame and nothing could fade
//!   the audio before it: the gain is zero on that frame (the old audio's
//!   abrupt end, the one step, accepted) and the new audio fades in.
//!
//! A command the motion machine refuses after its fade-out still leaves the
//! gain at zero on its frame, and it fades back in from there. The motion
//! machine's own fade ramp is not used; the engine settles it the moment the
//! command lands.
//!
//! # A main output change
//!
//! A commit that changes the main output's generation can carry the old main output's
//! outputs beside the new main output's ([`Plan::main_output_bank`](tutti_graph::Plan::main_output_bank),
//! [`Editor::set_main_output_fade`]). Over the fade's length the engine folds each main output
//! with its own fold, `(MainOutputId, device)`, and mixes them by the fade law's
//! gains: the old main output's channels fade out as the new main output's fade in, frame by
//! frame, and past the fade the new main output is heard alone. The old main output's
//! channels align to the previous plan's total latency, so when the two totals
//! differ a source present in both main outputs is heard at two delays during the
//! fade and shifts once at its end. A commit with no fade
//! cuts at its block. A commit that installs while the fade runs and leaves
//! the main output's generation alone continues it from the frames already mixed. One
//! that declares the main output again ends it at its block and begins another from
//! the main output that was fading in: the main output that was fading out is dropped, a
//! step the session avoids by declaring only after the running fade ends.
//!
//! # A routing change
//!
//! A change of the output routing ([`OutputControl::set_routing`]) crossfades
//! the same rendered channels from the old routing's fold to the new one over the
//! declick length along [`FadeLaw::EqualAmplitude`], and a change made while a
//! main output change crossfades, or while the last routing change still fades, cuts.
//! See the `output` module's docs.
//!
//! # Limits
//!
//! [`Engine::new`] bounds the graph's editor ([`Editor::set_limits`]) to
//! what its fold scratch holds: at most [`MAX_CHANNELS`] main
//! outputs, and a `MaxBlock` no larger than its block capacity (the larger
//! of the prepared maximum and [`DEFAULT_BLOCK_CAPACITY`], or the
//! capacity given to [`Engine::with_capacity`]). A commit or a re-prepare
//! past them is refused on the control thread
//! (`CommitError::TooManyOutputs`, `CommitError::BlockTooLong`), so the
//! callback never meets a graph it cannot render.

use tutti_graph::{
    BeatCrossings, CommitError, Due, Editor, Env, Executor, FadeLaw, Limits, MainOutputId, Offset,
    TransportChanges, declick_frames, fade_gains, fade_in,
};

use crate::block_clock::{BlockClock, BlockClockReader, BlockDates, DateKeeper, Pose};
use crate::load::{self, BlockCounts, Budget, Load, Writers};
use crate::output::{DEFAULT_DEVICE_WIDTHS, OutputControl, OutputFolds, pack_in_use};
use crate::routing::StageTargets;
use crate::stage::OutputStage;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_buffers::ChannelLayout;
use tutti_buffers::MAX_CHANNELS;
use tutti_buffers::{Float, FoldError, Interleaved, InterleavedMut, Planes};
use tutti_platform::Instant;
use tutti_rt::{AudioThreadCell, Counter, Gauge, Reader, RtScope};
#[cfg(doc)]
use tutti_timeline::MotionFsm;
use tutti_timeline::{
    AudioTaken, FadeOut, IMMEDIATE_RESERVE, MotionCommand, SCHEDULE_CAPACITY, Scheduled,
    TransportAudio, TransportCommand,
};
use tutti_units::{At, Frame};
use tutti_units::{SampleRate, Samples, Seconds};

/// The block capacity an engine sizes its fold scratch for unless given
/// another ([`Engine::with_capacity`]): tutti-cpal's largest callback
/// (`MAX_FRAMES`). The engine bounds its editor's re-prepares to it.
pub const DEFAULT_BLOCK_CAPACITY: Samples = Samples::new(8192);

/// Why [`Engine::new`] refused a graph.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphEngineError {
    /// The executor is not the editor's: they were not built together.
    #[error("the executor is not the editor's")]
    NotAPair,
    /// What the editor already sent is past what the engine can run: more
    /// than [`MAX_CHANNELS`] main outputs, or a `MaxBlock` above the
    /// block capacity.
    #[error("the graph is past the engine's limits: {0}")]
    Limits(CommitError),
    /// Another engine holds `transport`'s audio side (its motion machine,
    /// playhead and commands): one built over it, or over a clone of it, is
    /// still alive. Two would both consume every seek and both write the
    /// playhead ([`Transport::audio`](tutti_timeline::Transport::audio)).
    #[error("{0}")]
    TransportHeld(AudioTaken),
}

/// The transport as a graph block sees it.
type GraphTransport = tutti_graph::TransportPosition;

/// The audio engine: ticks the graph + transport and renders one
/// output buffer per block from the audio callback.
///
/// # What it owns, and what it deliberately does not
///
/// `Engine` is the *audio-thread half* of the runtime and holds only what a
/// render needs: the audio half of the transport's [`MotionFsm`] (its state
/// machine, as a plain field), the graph's [`Executor`]
/// with the clock that feeds its `Env`, and the declick gain it puts on the
/// output. It owns no graph topology, no parameter storage and no device
/// configuration — the control thread keeps the graph's editing half (the
/// `tutti_graph::Editor`) and hands changes over by committing, so nothing
/// here allocates, locks, or edits a graph.
///
/// That split is the reason this is a distinct type rather than a method on the
/// transport or the graph. Both of those are edited from the control thread;
/// this is touched only from the callback. Fusing it into either would put a
/// control-thread API and an RT-only API on one object, where the compiler could
/// not say which methods are safe to call from where — and the failure is
/// silent, because a lock or an allocation on the audio thread produces a
/// dropout rather than an error.
///
/// [`process`](Self::process) is the per-block entry point the callback calls:
/// it drains pending motion, applies timestamped transport commands on their
/// frames (see the module docs), renders, then applies any declick fade.
/// [`process_segment`](Self::process_segment) is public separately because it
/// is the pure render — useful to drive directly in a test or an offline pass,
/// where transport motion and declicking are not in play.
pub struct Engine<F: Float = f32> {
    graph: AudioThreadCell<GraphRender<F>>,
    /// Cached from the transport so the fade path avoids a double deref.
    fader: AudioThreadCell<Fader<F>>,
    /// The block capacity (the editor's `MaxBlock` limit).
    capacity: Samples,
    /// Records each block's render time against its budget.
    load: AudioThreadCell<Writers>,
    /// The readers of what `load` writes.
    readings: (Gauge<Load>, Counter<BlockCounts>),
    /// The tap after the fader, the audio thread's ends.
    hearing: AudioThreadCell<crate::hearing::Hearing<F>>,
    /// Its control side, until the owner takes it.
    heard_taps: tutti_rt::Exclusive<Option<crate::HeardTaps<F>>>,
    /// A reader of the block clock the graph render writes.
    clock_reader: BlockClockReader,
    /// The folds from the main output to the device, published by
    /// [`support_output`](Self::support_output).
    output: OutputControl<F>,
    /// The audio thread's reader of `output`, lent once per call.
    folds: AudioThreadCell<Reader<OutputFolds<F>>>,
    /// The gain each device output is ramped to, published beside the folds.
    stage: AudioThreadCell<Reader<StageTargets>>,
    /// The audio thread's reader of the project's tempo map, lent once per
    /// segment: the one loan the segment's walk and render both read.
    tempo: AudioThreadCell<tutti_timeline::TempoReader>,
}

impl<F: Float> std::fmt::Debug for Engine<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("graph", &self.graph)
            .field("fader", &self.fader)
            .field("capacity", &self.capacity)
            .field("readings", &self.readings)
            .finish_non_exhaustive()
    }
}

/// The graph's executor, the clock that feeds its `Env`, and the
/// planar scratch its outputs land in before the fold.
struct GraphRender<F: Float> {
    /// Boxed once at construction, so an `Engine` moves cheaply.
    exec: Box<Executor<F>>,
    /// The transport's audio side: motion machine, playhead clock and
    /// commands. The graph reads the transport from `Env`, so the engine
    /// drives the clock itself; every node reads what it publishes from its
    /// block's `Env`.
    transport: TransportAudio,
    /// `SCRATCH_CHUNKS` planar channels of `stride` frames each.
    scratch: Vec<F>,
    stride: usize,
    crossings: BeatCrossings,
    /// The executor's [`dropped_event_count`](Executor::dropped_event_count) already
    /// forwarded to the block counts.
    dropped_seen: u64,
    /// Where each render call's start stands on the OS clock and the graph's
    /// frame clock: what a producer thread's `ClockMap` is fed
    /// ([`Engine::block_clock`]).
    clock: BlockClock,
    /// What dates each call the clock publishes.
    dates: DateKeeper,
    /// The main output of the plan the last block rendered, for the control side to
    /// retire the older main outputs' folds by ([`OutputControl::reclaim`]).
    installed: Arc<AtomicU64>,
    /// The widths of the pair the last block rendered
    /// ([`OutputControl::set_routing`] checks the new routing against it).
    in_use: Arc<AtomicU64>,
    /// Where each device output's gain ramp stands.
    stage: OutputStage,
    /// Where the crossfade of the last routing change stands.
    routing: RoutingRun,
    /// The newest routing generation this thread has finished fading from or
    /// cut, for the control side to drop its `previous` table by
    /// ([`OutputControl::reclaim`]).
    routing_ended: Arc<AtomicU64>,
}

/// How far the audio thread has got through a routing change's crossfade.
#[derive(Clone, Copy)]
struct RoutingRun {
    /// The generation of the table the last block lent; `None` before the
    /// first block, which adopts whatever it finds (nothing was heard to
    /// fade from).
    seen: Option<u64>,
    /// Frames of the fade rendered since `seen` was first lent.
    done: usize,
}

/// What the control side published, lent for one segment: the folds, the
/// gains the outputs ramp to and the tempo map the segment's walk ran on, so
/// the nodes' `Cx::tempo` is the map their transport position came from.
#[derive(Clone, Copy)]
struct Lent<'a, F: Float> {
    folds: &'a OutputFolds<F>,
    stage: &'a StageTargets,
    tempo: &'a tutti_tempo::TempoMap,
}

/// The main output banks a plan renders at most: the main output's channels and, while a
/// main output change crossfades, the old main output's.
const MAIN_OUTPUT_BANKS: usize = 2;

/// Planar chunks of the fold scratch: the rendered channels of both banks and
/// the main output crossfade's four.
const SCRATCH_CHUNKS: usize = MAIN_OUTPUT_BANKS * MAX_CHANNELS + 4;

/// The scratch a crossfade folds through (a main output change's, or a routing
/// change's): the block's worth of each.
struct Mixing<'a, F: Float> {
    /// One device channel of the new main output's fold.
    fold: &'a mut [F],
    /// The same channel of the old fold: the old main output's, or the replaced
    /// routing's of the same main output.
    old: &'a mut [F],
    /// The gain of the old side at each frame of the block, and the new
    /// side's.
    gain_old: &'a mut [F],
    gain_new: &'a mut [F],
}

/// The two main outputs a block folds into the device, and how far their fade has
/// got.
struct Crossfaded<'a, 'p, F: Float> {
    /// The new main output's fold into the device, and its channels.
    new: (&'a tutti_buffers::Fold<F>, &'p [&'p [F]]),
    /// The old fold of the same channels: the old main output's, or the replaced
    /// routing's of the same main output. `None` when the old main output's fold is not
    /// published, which fades the new main output in from silence.
    old: Option<(&'a tutti_buffers::Fold<F>, &'p [&'p [F]])>,
    /// The fade's law and length, and the frames of it already rendered.
    fade: FadeRun,
}

/// A fade over `length` frames along `law`, `done` of them behind it.
#[derive(Clone, Copy)]
struct FadeRun {
    law: FadeLaw,
    length: usize,
    done: usize,
}

impl<F: Float> Mixing<'_, F> {
    /// Folds the two main outputs into `device` and sums them under the fade's
    /// gains: frame `k` of the fade is `old * g_old + new * g_new` with
    /// `(g_old, g_new)` the fade law's gains at `k`, and from the fade's last
    /// frame on the new main output is the whole mix. Every sample of `device` is
    /// written. Allocation-free.
    fn crossfade(
        &mut self,
        main_outputs: Crossfaded<'_, '_, F>,
        mut device: InterleavedMut<'_, F>,
    ) -> Result<(), tutti_buffers::BufferError> {
        let Crossfaded { new, old, fade } = main_outputs;
        let frames = device.frames().get();
        let channels = device.channels();
        let FadeRun {
            law,
            length,
            done: first,
        } = fade;
        for (i, (go, gn)) in self.gain_old[..frames]
            .iter_mut()
            .zip(&mut self.gain_new[..frames])
            .enumerate()
        {
            (*go, *gn) = if first + i < length {
                fade_gains::<F>(law, Samples::new(first + i), Samples::new(length))
            } else {
                (F::ZERO, F::ONE)
            };
        }
        let new_planes = Planes::new(new.1)?;
        let old_planes = old.map(|(_, planes)| Planes::new(planes)).transpose()?;
        let block = device.samples_mut();
        for channel in 0..channels {
            new.0.fold_planar_channel(
                new_planes,
                channel,
                tutti_buffers::MonoMut::new(&mut self.fold[..frames]),
            )?;
            match (old, old_planes) {
                (Some((fold, _)), Some(planes)) => fold.fold_planar_channel(
                    planes,
                    channel,
                    tutti_buffers::MonoMut::new(&mut self.old[..frames]),
                )?,
                _ => self.old[..frames].fill(F::ZERO),
            }
            for (i, sample) in block[channel..].iter_mut().step_by(channels).enumerate() {
                *sample = self.old[i] * self.gain_old[i] + self.fold[i] * self.gain_new[i];
            }
        }
        Ok(())
    }
}

impl<F: Float> GraphRender<F> {
    /// Publishes one render call's start: the OS time it began at
    /// (`os_ns`), the frame it renders first, and `first`, the transport at
    /// that frame (after the commands due on it), for producer threads that
    /// hold an OS time and for the session's one-read playhead.
    /// `frames` is the whole call's length.
    fn publish_call(
        &mut self,
        os_ns: i64,
        rate: SampleRate,
        frames: usize,
        first: &GraphTransport,
    ) {
        let beat = first.beat();
        let pose = Pose {
            beat,
            seconds: self.transport.seconds_at(beat, first.tempo),
            tempo: first.tempo,
            playing: first.playing,
            span: Samples::new(frames),
        };
        self.clock.write(os_ns, self.exec.frame(), rate, pose);
    }
}

impl<F: Float> Engine<F> {
    /// Creates an engine that renders the graph run by `executor`.
    ///
    /// `executor` is the audio half of `editor`'s pair, whose `Prepare` comes
    /// from the device configuration (its rate, and the largest block the
    /// device hands over). The block capacity is the larger of that maximum
    /// and [`DEFAULT_BLOCK_CAPACITY`]; see
    /// [`with_capacity`](Self::with_capacity) for what building an engine
    /// does to the editor and the transport.
    ///
    /// # Errors
    ///
    /// The same as [`with_capacity`](Self::with_capacity).
    pub fn new(
        transport: &tutti_timeline::Transport,
        editor: &mut Editor<F>,
        executor: Executor<F>,
    ) -> Result<Self, GraphEngineError> {
        Self::with_capacity(transport, editor, executor, DEFAULT_BLOCK_CAPACITY)
    }

    /// As [`new`](Self::new), with the fold scratch sized for blocks of up
    /// to `capacity` frames (or the prepared maximum, if larger).
    ///
    /// The engine renders whole device blocks through the executor — no
    /// 64-frame chunking; a device block longer than the prepared maximum is rendered as
    /// consecutive graph blocks of at most that — and builds each block's
    /// `Env` from `transport`: the frame is the executor's clock, which
    /// tracks device time, and the transport snapshot comes from the
    /// [`TransportAudio`] the engine takes from `transport`
    /// ([`Transport::audio`](tutti_timeline::Transport::audio)) and drives, so
    /// it publishes the playhead, steady time and tempo in force.
    ///
    /// **Bounds the editor** ([`Editor::set_limits`]) to at most
    /// [`MAX_CHANNELS`] main outputs and a `MaxBlock` of at most the
    /// capacity, so every later commit or re-prepare past them is refused
    /// there, with a `CommitError`. Refused here, building nothing, when
    /// `executor` is not `editor`'s, or when what the editor already sent is
    /// past them. The editor is borrowed mutably for the check, so no commit
    /// can slip in beside it.
    ///
    /// **One engine per transport.** The engine holds `transport`'s one audio
    /// side ([`Transport::audio`](tutti_timeline::Transport::audio)): the
    /// playhead writer and the motion machine, which applies
    /// [`MotionFsm`]'s commands. While this engine is alive, another built
    /// over the same transport, or a clone of it, is refused with
    /// [`GraphEngineError::TransportHeld`] — two would both consume every
    /// seek and both write the playhead. It is given back when this engine is
    /// dropped, so a later engine resumes the same motion state.
    ///
    /// Nor can the graph hold a second writer: only that audio side writes
    /// a playhead. A node that
    /// follows the beat (`ClickNode`, a beat-synced LFO or automation lane)
    /// reads it from its block's `Env`, which shares nothing.
    ///
    /// Every render call is dated by the OS clock until [`dates`](Self::dates)
    /// says otherwise.
    ///
    /// Control thread. Allocates the fold scratch
    /// (`MAX_CHANNELS × capacity` samples).
    ///
    /// # Errors
    ///
    /// Returns an error, leaving `editor` unchanged, when:
    ///
    /// - [`GraphEngineError::NotAPair`]: `executor` is not `editor`'s.
    /// - [`GraphEngineError::TransportHeld`]: another engine over
    ///   `transport` (or a clone of it, or a transport sharing its `motion`
    ///   handle) is alive.
    /// - [`GraphEngineError::Limits`]: what the editor already sent has more
    ///   than [`MAX_CHANNELS`] main outputs or a `MaxBlock` above the
    ///   capacity.
    pub fn with_capacity(
        transport: &tutti_timeline::Transport,
        editor: &mut Editor<F>,
        executor: Executor<F>,
        capacity: Samples,
    ) -> Result<Self, GraphEngineError> {
        if !editor.is_paired_with(&executor) {
            return Err(GraphEngineError::NotAPair);
        }
        // Before the limits, which change the editor: a refused engine
        // leaves it as it was.
        let rate = executor.prepare().sample_rate();
        let audio = transport
            .audio(rate)
            .map_err(GraphEngineError::TransportHeld)?;
        // Mid re-prepare, the editor's `Prepare` is the one the executor will
        // adopt, and may be the larger.
        let stride = capacity
            .get()
            .max(executor.prepare().max_block().get())
            .max(editor.prepare().max_block().get());
        editor
            .set_limits(Limits {
                max_main_outputs: MAX_CHANNELS,
                max_block: stride,
            })
            .map_err(GraphEngineError::Limits)?;
        // Seeded from the executor; no test here can cover it, since the
        // executor is always fresh at `Engine::new`.
        let dropped_seen = executor.dropped_event_count();
        let (writers, load, blocks) = load::link();
        let (heard_taps, hearing) = crate::hearing::link();
        let clock = BlockClock::new();
        let clock_reader = clock.reader();
        // Every bare main output width places into every bare device width up to
        // `DEFAULT_DEVICE_WIDTHS` (`every_main_output_width_places_into_any_device`).
        let output = OutputControl::new(OutputFolds::up_to(DEFAULT_DEVICE_WIDTHS));
        editor.set_main_output_gate(Arc::new(output.clone()));
        let folds = AudioThreadCell::new(output.cell().reader());
        let stage = AudioThreadCell::new(output.stage().reader());
        let tempo = AudioThreadCell::new(audio.tempo_reader());
        Ok(Self {
            graph: AudioThreadCell::new(GraphRender {
                exec: Box::new(executor),
                transport: audio,
                scratch: vec![F::ZERO; SCRATCH_CHUNKS * stride],
                stride,
                crossings: BeatCrossings::new(),
                dropped_seen,
                clock,
                dates: DateKeeper::new(BlockDates::default()),
                installed: Arc::clone(output.installed()),
                in_use: Arc::clone(output.in_use_cell()),
                stage: OutputStage::new(),
                routing: RoutingRun {
                    seen: None,
                    done: 0,
                },
                routing_ended: Arc::clone(output.routing_ended()),
            }),
            fader: AudioThreadCell::new(Fader::new(fade_frames(rate))),
            capacity: Samples::new(stride),
            load: AudioThreadCell::new(writers),
            readings: (load, blocks),
            hearing: AudioThreadCell::new(hearing),
            heard_taps: tutti_rt::Exclusive::new(Some(heard_taps)),
            clock_reader,
            output,
            folds,
            stage,
            tempo,
        })
    }

    /// Builds the fold from every main output width into a device of `device`
    /// channels, so the engine can render into a buffer of that width.
    ///
    /// A new engine renders into buffers of up to 16 channels without being
    /// asked: the folds from each main output width (1 to `MAX_CHANNELS`) to each
    /// device width are built on the control side, in
    /// [`Engine::new`](Self::new), and published through a `Publisher` that each
    /// call lends once, so a plan change needs no rebuild and the audio thread
    /// never builds a fold. Call this when a stream is built for a device
    /// wider than that (or at all, to say so); it is a no-op for a width
    /// already supported. The device is taken to be the engine's order for its
    /// width where it has one, and the main output the same: see
    /// [`fold_to_width`](tutti_graph::fold_to_width). A buffer of a width not
    /// supported is not rendered into: the call is silent and counted in
    /// [`BlockCounts::silenced`], in
    /// every build.
    ///
    /// Where a mono main output lands: the centre of a 6, 8 or 12-wide device, the
    /// front pair of a 4-wide (quad) or a stereo one. An undeclared 4-wide
    /// main output is **treated as quad** into a 6, 8 or 12-wide device (its rears are
    /// the back pair, not the centre and LFE); a main output of independent buses says
    /// so with [`OutputControl::publish_main_output`] and stays by index.
    ///
    /// A callback mid-block keeps the folds it lent, and the next call reads
    /// the new ones. Returns at once, publishing nothing, for a width already
    /// supported.
    ///
    /// **One writer.** The update is read-modify-write (it publishes the
    /// current set plus this width), so two concurrent calls could lose one
    /// width. Call it from the one control thread that builds the stream.
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] (a zero width) for a device of no channels; nothing is published.
    pub fn support_output(&self, device: ChannelLayout) -> Result<(), FoldError> {
        self.output.support(device)
    }

    /// What dates every render call ([`BlockDates`]), fixed for the engine's
    /// life from here on; for `Frames` the origin is read now, on the
    /// control thread.
    #[must_use]
    pub fn dates(mut self, dates: BlockDates) -> Self {
        self.graph.get_mut().dates = DateKeeper::new(dates);
        self
    }

    /// The control side of the output folds, to keep when this engine moves to
    /// the audio thread: it declares what the graph's main output is
    /// ([`OutputControl::publish_main_output`]), states what a device is
    /// ([`OutputControl::state_device`]) and adds devices, publishing into the
    /// folds this engine lends.
    pub fn output_control(&self) -> OutputControl<F> {
        self.output.clone()
    }

    /// Frees the folds [`support_output`](Self::support_output) replaced that a block
    /// held at the time, and returns how many are still pinned. Control side;
    /// cheap when nothing is retired.
    pub fn reclaim(&self) -> usize {
        self.output.reclaim()
    }

    /// The block capacity: the largest `MaxBlock` the editor may re-prepare
    /// to.
    pub fn block_capacity(&self) -> Samples {
        self.capacity
    }

    /// The engine's [`Load`]: every [`process`](Self::process) and
    /// [`process_segment`](Self::process_segment) call's wall time against its
    /// block's real-time budget, smoothed. Any number of clones read the one
    /// cell, from any thread, and nothing resets on read.
    ///
    /// What it times is the whole call, which is what a device deadline
    /// counts, the plan's meter ops included; a driver's own work around
    /// it (tutti-cpal's sample-format conversion, one linear pass) is not in
    /// it. The budget is each graph block's frames at the rate that block ran
    /// at, summed, so a call that straddles a re-prepare's rate change is held
    /// to the rate in effect for each piece. A block refused the graph
    /// (rendered silent, an audible dropout) is not timed; it is counted in
    /// [`BlockCounts::silenced`].
    pub fn load(&self) -> Gauge<Load> {
        self.readings.0.clone()
    }

    /// What the engine's blocks amounted to: blocks timed, over budget and
    /// silenced, and the events the graph dropped (an event port past its
    /// capacity). Cumulative; an app diffs two readings. Any number of clones
    /// read the one cell, from any thread.
    pub fn blocks(&self) -> Counter<BlockCounts> {
        self.readings.1.clone()
    }

    /// A reader of the block clock: each [`process`](Self::process) and
    /// [`process_segment`](Self::process_segment) call publishes the OS time
    /// it began and the graph frame it began at, and a thread that holds an OS
    /// time (a hardware MIDI input) reads the latest to place it on a frame
    /// (`tutti-midi-runtime`'s `ClockMap`). `None` from the reader until the
    /// first call. Control side.
    pub fn block_clock(&self) -> BlockClockReader {
        self.clock_reader.clone()
    }

    /// The control side of the tap after the fader ("what I'm hearing"): the
    /// device-width output of every [`process`](Self::process) and
    /// [`process_segment`](Self::process_segment) call, read through a
    /// [`TapReader`](tutti_graph::TapReader). The one owner takes it: `None`
    /// once taken. See [`HeardTaps`](crate::HeardTaps).
    pub fn take_heard_taps(&mut self) -> Option<crate::HeardTaps<F>> {
        self.heard_taps.get_mut().take()
    }

    /// Renders the whole of `output` with no transport motion and no declick.
    ///
    /// `output` is an interleaved device buffer that carries its own width,
    /// which the engine has folds for (up to 16 channels, or a width added with
    /// [`support_output`](Self::support_output)). This is the pure render.
    ///
    /// One executor block per device block (up to its prepared maximum),
    /// under the
    /// transport as it stands. The graph's main output has no inputs. The main output is
    /// rendered at its **own** output width (at most [`MAX_CHANNELS`],
    /// which the editor's limits enforce) into scratch, then each frame is
    /// folded to the *output's* width — the device / target width — by
    /// [`fold_to_width`](tutti_graph::fold_to_width), built in
    /// [`support_output`](Self::support_output): a surround main output plays folded to a
    /// stereo device by the ITU/Dolby matrix, or straight through to a
    /// matching-width one; a mono main output feeds the centre of a surround device
    /// (its front pair, if it has no centre) and is not copied to every channel;
    /// a main output wider than a surround device is placed by position.
    ///
    /// # Two widths are live here; only one is the buffer's
    ///
    /// The output width (`out_ch`) comes from `output`'s own layout; the
    /// main output's from the plan's main outputs. They are different numbers
    /// from different sources, and confusing them writes past the end of one
    /// buffer or reads garbage from the other. The output width arrives
    /// welded to the buffer it strides, which is what makes a third
    /// confusion — a width disagreeing with its slice — unrepresentable
    /// rather than merely unlikely.
    #[inline]
    pub fn process_segment(&self, output: &mut InterleavedMut<'_, F>) {
        // A monotonic clock read, two per call, no lock and no allocation:
        // normally a vDSO read on Linux (a real syscall if the clocksource
        // is not vDSO-capable, e.g. some VMs), the commpage on macOS, QPC
        // on Windows.
        let start = Instant::now();
        let out_ch = output.channels();
        let output_layout = output.layout();
        let frames = output.frames().get();
        let output = output.samples_mut();
        // A free in a block is a debug panic: the hand-over of the heard tap
        // sends what it replaces back to the control side.
        let _rt = RtScope::enter();
        self.hearing.borrow_mut().swap();
        // Refused: a buffer of a width the engine has no folds for
        // (`support_output`), a caller's bug that must not index past the
        // fold. The loan lasts this statement; each segment lends again after
        // its `settle`, below.
        let mut reader = self.folds.borrow_mut();
        let mut stages = self.stage.borrow_mut();
        let mut tempos = self.tempo.borrow_mut();
        let declared = reader.lend().has_device(out_ch);
        // Refused only while the control side holds the graph
        // (`apply_pending` racing a callback): this block is silent.
        let graph = self.graph.try_borrow_mut();
        let (true, Some(mut graph)) = (declared, graph) else {
            output.fill(F::ZERO);
            self.heard(output, output_layout, 0, frames);
            if let Some(mut load) = self.load.try_borrow_mut() {
                load.silenced();
            }
            return;
        };
        let g = &mut *graph;
        let mut done = 0;
        let mut unplaced = false;
        let mut budget = Budget::new();
        while done < frames {
            let (bound, rate) = g.settle();
            let began = (done == 0).then(|| g.dates.date(None));
            let len = (frames - done).min(bound);
            // The segment's one loan of the tempo map, read before it is lent
            // (a publish in the gap is the next segment's), and handed to
            // everything the segment does with the map.
            let generation = g.transport.tempo_generation();
            let map = tempos.lend();
            let t = g.transport.begin_block(generation, &map);
            if let Some(os_ns) = began {
                g.publish_call(os_ns, rate, frames, &t);
            }
            // The tempo steps inside the block, if the map has any; a block
            // that would need more than it holds ends before the first that
            // does not fit, and the loop carries on from there.
            let ahead = g
                .transport
                .tempo_ahead(len, &t, tutti_graph::MAX_TRANSPORT_CHANGES, &map);
            let len = ahead.limit.unwrap_or(len);
            budget.add(Samples::new(len), rate);
            // Kept current, so a later `process` tells a late beat from one
            // jumped over across this block too.
            g.crossings.observe(&Env {
                frame: g.exec.frame(),
                sample_rate: rate,
                block_len: Samples::new(len),
                transport: t,
                changes: ahead.changes,
                play: tutti_graph::PlayClock::ZERO,
            });
            g.transport.advance(len, &t, &map);
            // Lent after `settle` installed the plan this segment renders:
            // the main output's folds were published before the commit that
            // carries its id, so a snapshot taken now holds them. The loan
            // ends with the segment.
            let (folds, stage) = (reader.lend(), stages.lend());
            let lent = Lent {
                folds: &folds,
                stage: &stage,
                tempo: &map,
            };
            unplaced |= !g.render(output, lent, out_ch, done, len, &t, &ahead.changes);
            self.heard(output, output_layout, done, len);
            g.dates.advance(len, rate);
            done += len;
        }
        let mut load = self.load.borrow_mut();
        if unplaced {
            load.silenced();
        }
        load.dropped_events(g.take_dropped_events());
        load.record(
            Seconds::new(start.elapsed().as_secs_f64()),
            budget.seconds(),
        );
    }

    /// Renders one block into `output`, an interleaved device buffer.
    ///
    /// How many frames is `output`'s own business — it is `output.len()`,
    /// which cannot disagree with the slice the way a separate `frames`
    /// argument could. The graph's main output is folded to `output`'s width (the device
    /// / target width, see [`support_output`](Self::support_output)) — see
    /// [`process_segment`](Self::process_segment). Untimed motion
    /// ([`MotionFsm::send`]) applies at the block's first frame; scheduled
    /// commands ([`MotionFsm::schedule`]) on their own frames (module docs).
    /// Called once per block from the audio callback. RT-safe: no
    /// allocation, no locks, no I/O.
    #[inline]
    pub fn process(&self, output: &mut InterleavedMut<'_, F>) {
        self.process_stamped(output, None);
    }

    /// [`process`](Self::process) for a call whose start the caller dates:
    /// the block clock ([`block_clock`](Self::block_clock)) records `os_ns`,
    /// a [`monotonic_ns`](tutti_platform::monotonic_ns) reading, as the time the
    /// call began, where `process` reads the clock itself.
    ///
    /// A caller that renders to a schedule passes the instant the block was
    /// due, so a reader mapping frames to the OS clock sees the schedule and
    /// not the jitter of the wake-up that ran it.
    #[inline]
    pub(crate) fn process_at(&self, output: &mut InterleavedMut<'_, F>, os_ns: i64) {
        self.process_stamped(output, Some(os_ns));
    }

    /// Forgets the device the last block rendered into, for a stream about to
    /// open. Control side.
    pub(crate) fn forget_device_in_use(&self) {
        self.output.forget_in_use();
    }

    /// Counts one device callback longer than the ceiling in
    /// [`BlockCounts::truncated`]. Audio thread.
    #[inline]
    pub(crate) fn count_truncated(&self) {
        // Only the audio callback borrows `load`, and this runs on that same
        // callback outside any other borrow of it, so the borrow is always free.
        if let Some(mut load) = self.load.try_borrow_mut() {
            load.truncated();
        }
    }

    /// Writes frames `done..done + len` of the rendered `output` to the heard
    /// tap. Audio thread.
    #[inline]
    fn heard(&self, output: &[F], layout: ChannelLayout, done: usize, len: usize) {
        if let Ok(block) = Interleaved::new(output, layout) {
            let block = block.slice(Samples::new(done)..Samples::new(done + len));
            self.hearing.borrow_mut().write(block);
        }
    }

    #[inline]
    fn process_stamped(&self, output: &mut InterleavedMut<'_, F>, stamp: Option<i64>) {
        // Timed for the load meter (`load`): see `process_segment`.
        let start = Instant::now();
        let out_ch = output.channels();
        let output_layout = output.layout();
        let frames = output.frames().get();
        let output = output.samples_mut();
        // A free in a block is a debug panic: the hand-over of the heard tap
        // sends what it replaces back to the control side.
        let _rt = RtScope::enter();
        self.hearing.borrow_mut().swap();
        // The audio thread never panics on a borrow. The graph is refused
        // only while the control side holds it (`apply_pending` racing a
        // callback): this block is silent, and the motion queue, which the
        // render state's motion machine drains, is left for the next one.
        // The fader and the schedule's pending list need no `try_`:
        // the fader is private to this method, and the pending list is
        // reached only under the graph borrow (here, or `apply_pending`'s).
        let mut reader = self.folds.borrow_mut();
        let mut stages = self.stage.borrow_mut();
        let mut tempos = self.tempo.borrow_mut();
        let declared = reader.lend().has_device(out_ch);
        let graph = self.graph.try_borrow_mut();
        let (true, Some(mut graph)) = (declared, graph) else {
            output.fill(F::ZERO);
            self.heard(output, output_layout, 0, frames);
            if let Some(mut load) = self.load.try_borrow_mut() {
                load.silenced();
            }
            return;
        };
        let g = &mut *graph;
        let mut done = 0;
        let mut unplaced = false;
        let mut budget = Budget::new();
        while done < frames {
            let (bound, rate) = g.settle();
            let began = (done == 0).then(|| g.dates.date(stamp));
            let len = (frames - done).min(bound);
            let frame0 = g.exec.frame();
            let mut pieces = GraphPieces {
                changes: TransportChanges::NONE,
            };
            let aim = self.fader.borrow().aim;
            // The segment's one loan of the tempo map: the walk and the
            // render below read it, and a publish after it is the next
            // segment's (the count is read first, so none is missed).
            let generation = g.transport.tempo_generation();
            let map = tempos.lend();
            let walk = self.walk(
                Track {
                    transport: &mut g.transport,
                    crossings: &mut g.crossings,
                },
                frame0,
                len,
                rate,
                aim,
                &mut pieces,
                (generation, &map),
            );
            let changes = pieces.changes;
            if let Some(os_ns) = began {
                g.publish_call(os_ns, rate, frames, &walk.start);
            }
            // The walk cut the block short where a tempo step did not fit
            // its changes; the rest is the next iteration's block.
            let len = walk.frames;
            budget.add(Samples::new(len), rate);
            // Lent after `settle` installed the plan this segment renders
            // (see `process_segment`).
            let (folds, stage) = (reader.lend(), stages.lend());
            let lent = Lent {
                folds: &folds,
                stage: &stage,
                tempo: &map,
            };
            unplaced |= !g.render(output, lent, out_ch, done, len, &walk.start, &changes);
            let mut fader = self.fader.borrow_mut();
            fader.fade = fade_frames(rate);
            fader.apply(&walk.gain, frame0, output, out_ch, done, len);
            self.heard(output, output_layout, done, len);
            g.dates.advance(len, rate);
            done += len;
        }
        let mut load = self.load.borrow_mut();
        if unplaced {
            load.silenced();
        }
        load.dropped_events(g.take_dropped_events());
        load.record(
            Seconds::new(start.elapsed().as_secs_f64()),
            budget.seconds(),
        );
    }

    /// Walks one block's scheduled transport commands in time order, running
    /// each piece between them through `pieces` and planning the declick
    /// gain over the block. See the module docs for the rules.
    ///
    /// `aim` is the fader's aim going in: an aim is pushed only where it
    /// changes, so a block with no command in sight plans nothing and the
    /// fader skips it.
    fn walk(
        &self,
        track: Track<'_>,
        frame0: Frame,
        mut frames: usize,
        rate: SampleRate,
        mut aim: Option<Frame>,
        pieces: &mut GraphPieces,
        (generation, map): (u64, &tutti_tempo::TempoMap),
    ) -> Walk {
        let Track {
            transport,
            crossings,
        } = track;
        let fade = fade_frames(rate);
        let mut walk = Walk {
            start: transport.begin_block(generation, map),
            gain: GainPlan::new(),
            frames,
        };
        let mut t = walk.start;
        let mut cursor = 0;
        // The crossing tracker as of the start of the current piece. Each piece is
        // observed with its real length once it is cut; before that, a copy
        // observes it with the rest of the block, to answer `crossed`.
        let mut base = *crossings;
        transport.drain_commands();
        loop {
            // The map's tempo steps from the cursor on, assuming no command
            // moves the transport first (one that does is followed by a new
            // look). They cost no more than the block's changes can hold: a
            // step past that ends the block here, and the rest is the next
            // one's. A constant map has none, and this is nothing.
            let ahead = transport.tempo_ahead(frames - cursor, &t, pieces.free(), map);
            if let Some(end) = ahead.limit {
                frames = cursor + end;
            }
            let env = Env {
                frame: frame0 + Samples::new(cursor),
                sample_rate: rate,
                block_len: Samples::new(frames - cursor),
                transport: t,
                changes: ahead.changes,
                play: tutti_graph::PlayClock::ZERO,
            };
            // The next declicked command ahead, within one fade of the end of
            // this block: the fade-out that ends on its frame may start here.
            let next = lead_target(transport.pending(), &env, fade);
            if next != aim {
                walk.gain.push(cursor, Gain::Aim(next));
                aim = next;
            }
            let mut ph = base;
            ph.observe(&env);
            // The earliest command due from the cursor on; send order breaks
            // ties.
            let mut best: Option<(usize, usize, bool)> = None;
            for (i, cmd) in transport.pending().iter().enumerate() {
                let (k, late) = match (cmd.at(), env.due(cmd.at())) {
                    (_, Due::In(k)) => (k.index(), false),
                    (At::Frame(_), Due::Late) => (0, true),
                    (At::Beat(b), _) if ph.crossed(b.get()) => (0, true),
                    _ => continue,
                };
                let earlier = best.is_none_or(|(bk, bi, _)| {
                    k < bk || (k == bk && cmd.seq() < transport.pending()[bi].seq())
                });
                if earlier {
                    best = Some((k, i, late));
                }
            }
            // A tempo step comes first when it is before every command (one on
            // its frame is taken by the command's own transport).
            let step = ahead.changes.as_slice().first().map(|c| c.at.index());
            let (k, i, late) = match (best, step) {
                (None, None) => break,
                (Some(cmd), None) => cmd,
                (Some(cmd), Some(s)) if cmd.0 <= s => cmd,
                (_, Some(s)) => {
                    base.observe(&Env {
                        block_len: Samples::new(s),
                        changes: TransportChanges::NONE,
                        ..env
                    });
                    transport.advance(s, &t, map);
                    cursor += s;
                    t = transport.position_after_tempo_change(map);
                    if let Some(o) = Offset::new(Samples::new(cursor), Samples::new(frames)) {
                        pieces.change(o, t);
                    }
                    continue;
                }
            };
            let at = cursor + k;
            if at > cursor {
                if !pieces.room() {
                    // No more cuts fit this block: the rest wait for the
                    // next one, where they land late.
                    break;
                }
                base.observe(&Env {
                    block_len: Samples::new(at - cursor),
                    ..env
                });
                transport.advance(at - cursor, &t, map);
                cursor = at;
            }
            let (applied, now) = transport.apply_command(i, late, map);
            if applied.declicked() {
                // A jump clears the fader's aim.
                walk.gain.push(cursor, Gain::Jump);
                aim = None;
            }
            t = now;
            match Offset::new(Samples::new(cursor), Samples::new(frames)) {
                Some(o) if cursor > 0 => pieces.change(o, t),
                _ => walk.start = t,
            }
        }
        base.observe(&Env {
            frame: frame0 + Samples::new(cursor),
            sample_rate: rate,
            block_len: Samples::new(frames - cursor),
            transport: t,
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        });
        *crossings = base;
        transport.advance(frames - cursor, &t, map);
        walk.frames = frames;
        walk
    }

    /// Installs every commit the engine's editor has sent, outside the audio
    /// callback.
    ///
    /// It also follows a re-prepare's rate change with the engine's clock and
    /// the transport's frame-timed commands — what the first block after it
    /// would do, done now. Returns `true` when the graph is running a plan
    /// with no re-prepare between its halves, `false` while a re-prepare is
    /// still half done.
    ///
    /// Not [`Executor::apply_pending`](tutti_graph::Executor::apply_pending),
    /// which drains only the executor's commit queue: this is the engine-level
    /// call that also moves the clock and the transport.
    ///
    /// A device restart calls it between the two halves of
    /// `Editor::reprepare` so the re-prepare finishes before the first block
    /// at the new rate, which then renders the re-prepared graph rather than
    /// the executor's silent checked-out block.
    /// Hosts reach it through [`Stopped::apply_pending`](crate::Stopped::apply_pending), which only
    /// a restart hook is handed.
    ///
    /// Allocates nothing itself; a commit it installs was built on the
    /// control side.
    ///
    /// # Panics
    ///
    /// If an audio callback is inside [`process`] or [`process_segment`] on
    /// this engine at the same moment: this borrows the executor and the
    /// transport schedule the audio thread otherwise owns, and their
    /// [`AudioThreadCell`]s refuse a second borrow in every build. The panic
    /// is on this, the control, side: a callback that meets this call's
    /// borrow instead renders a silent block and does not panic. Call it
    /// only while the stream is stopped (dropped, or not yet started), as
    /// `Stopped` arranges.
    ///
    /// [`process`]: Self::process
    /// [`process_segment`]: Self::process_segment
    pub fn apply_pending(&self) -> bool {
        let graph = &mut *self.graph.borrow_mut();
        graph.settle();
        graph.exec.pending_prepare().is_none() && graph.exec.plan().is_some()
    }
}

/// The render state a block's walk moves: the transport its commands go
/// through, and the crossings it observes.
struct Track<'a> {
    transport: &'a mut TransportAudio,
    crossings: &'a mut BeatCrossings,
}

/// What a block's walk leaves for after the render.
struct Walk {
    /// The transport at the block's first frame, after the commands that
    /// landed there.
    start: GraphTransport,
    /// The declick gain's events in this block.
    gain: GainPlan,
    /// How many of the block's frames this walk covers: all of them, unless a
    /// tempo step would have been the block's ninth change, where it ends.
    frames: usize,
}

/// Frames a declick fades over at `rate`: [`DECLICK`](tutti_graph::DECLICK), at least one.
fn fade_frames(rate: SampleRate) -> usize {
    declick_frames(rate).get()
}

/// Whether `command` asks for a declick (the motion machine grants it only
/// while the transport is audible, which the lead check mirrors).
fn declicks(command: &TransportCommand) -> bool {
    matches!(
        command,
        TransportCommand::Motion(
            MotionCommand::Stop {
                fade: FadeOut::Declick
            } | MotionCommand::Locate {
                fade: FadeOut::Declick,
                ..
            }
        )
    )
}

/// The frame of the next declicked command ahead of `env`'s first frame,
/// if playback reaches it within `env`'s block plus one fade: where a
/// fade-out has to end. Timed commands only (`At::Frame`, `At::Beat`); one
/// due on the first frame itself has no lead left.
fn lead_target(pending: &[Scheduled], env: &Env, fade: usize) -> Option<Frame> {
    if !env.transport.playing {
        return None;
    }
    let look = Env {
        block_len: env.block_len + Samples::new(fade),
        ..*env
    };
    pending
        .iter()
        .filter(|c| declicks(&c.command()) && !matches!(c.at(), At::NextBlock))
        .filter_map(|c| match look.due(c.at()) {
            Due::In(k) if k.index() > 0 => Some(look.frame_at(k)),
            _ => None,
        })
        .min()
}

/// One change to the declick gain, at an offset of the block.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gain {
    /// From here, the next declicked command lands on this frame (or none
    /// is in sight): the gain falls linearly to reach zero exactly there,
    /// starting at most one fade ahead of it.
    Aim(Option<Frame>),
    /// A declicked command landed here: the gain is zero on this frame, and
    /// fades in from it.
    Jump,
}

/// Capacity of a block's gain plan: an aim per walk step and a jump per
/// command, bounded by the commands a block can apply.
const GAIN_EVENTS: usize = 2 * (SCHEDULE_CAPACITY + IMMEDIATE_RESERVE + 2);

/// A block's declick gain events, in offset order, on the stack.
struct GainPlan {
    len: usize,
    items: [(usize, Gain); GAIN_EVENTS],
}

impl GainPlan {
    fn new() -> Self {
        Self {
            len: 0,
            items: [(0, Gain::Aim(None)); GAIN_EVENTS],
        }
    }

    fn push(&mut self, at: usize, gain: Gain) {
        // A later aim at the same offset replaces an earlier one.
        if let Some(last) = self.items[..self.len].last_mut()
            && last.0 == at
            && matches!(last.1, Gain::Aim(_))
            && matches!(gain, Gain::Aim(_))
        {
            last.1 = gain;
            return;
        }
        debug_assert!(self.len < GAIN_EVENTS, "bounded by the commands applied");
        if self.len < GAIN_EVENTS {
            self.items[self.len] = (at, gain);
            self.len += 1;
        }
    }
}

/// The declick gain, carried across blocks: a fade-out that ends exactly on
/// a declicked command's frame, and a fade-in that starts there.
///
/// A fade is one move of the gain from where it is to zero or to one, along
/// [`FadeLaw::EqualAmplitude`]: a fade-out is `from * out` over the frames up
/// to its target, a fade-in `from * out + in` over one [`DECLICK`](tutti_graph::DECLICK). The gain
/// is continuous by construction: a fade starts from wherever the gain is, so
/// the one step is a declicked command with no lead time (`At::NextBlock`, a
/// late command, an untimed `send`): the old audio ends on the jump frame
/// with the gain at zero there, and the new audio fades in.
#[derive(Clone, Copy, Debug)]
struct Fader<F: Float> {
    gain: F,
    aim: Option<Frame>,
    /// Frames a fade spans, at the rate of the block being shaped.
    fade: usize,
    /// The fade the gain is on, if any.
    run: Option<Run<F>>,
}

/// One fade of the gain: `n` frames from `from` to silence (a fade-out that
/// ends on `aim`) or to unity, `k` of them done.
#[derive(Clone, Copy, Debug)]
struct Run<F: Float> {
    from: F,
    k: usize,
    n: usize,
    /// The frame a fade-out ends on; `None`: a fade-in.
    aim: Option<Frame>,
}

impl<F: Float> Run<F> {
    /// The gain of the fade's next frame, and the run moved past it; `None`
    /// once it has reached its end.
    fn step(self) -> (F, Option<Self>) {
        let to = if self.aim.is_some() { F::ZERO } else { F::ONE };
        let gain = fade_in(
            FadeLaw::EqualAmplitude,
            self.from,
            to,
            Samples::new(self.k),
            Samples::new(self.n),
        );
        // A fade-in lasts one frame past its last law step, which is the
        // gain's exact end; a fade-out ends on its aim, which is zero.
        let next = Self {
            k: self.k + 1,
            ..self
        };
        (gain, (next.k <= self.n).then_some(next))
    }
}

impl<F: Float> Fader<F> {
    fn new(fade: usize) -> Self {
        Self {
            gain: F::ONE,
            aim: None,
            fade,
            run: None,
        }
    }

    /// A fade-in from silence, starting on the next frame.
    fn rise(fade: usize) -> Option<Run<F>> {
        Some(Run {
            from: F::ZERO,
            k: 0,
            n: fade,
            aim: None,
        })
    }

    /// Shape frames `base..base + frames` of the interleaved `output`
    /// (`channels` wide), the block that starts at `frame0`, by `plan`. The
    /// gain is per frame, the same across every channel.
    fn apply(
        &mut self,
        plan: &GainPlan,
        frame0: Frame,
        output: &mut [F],
        channels: usize,
        base: usize,
        frames: usize,
    ) {
        let fade = self.fade;
        let end = frame0.get() + frames as u64;
        // Nothing to do on a block with no event, full gain, and no fade-out
        // starting inside it: the common case.
        let quiet = self.aim.is_none_or(|to| to.get() > end + fade as u64);
        if plan.len == 0 && self.gain == F::ONE && self.run.is_none() && quiet {
            return;
        }
        let mut next = 0;
        for i in 0..frames {
            let mut jump = false;
            while next < plan.len && plan.items[next].0 == i {
                match plan.items[next].1 {
                    Gain::Aim(to) => self.aim = to,
                    // Clears the aim it lands on when read, so an aim at the
                    // next target pushed after it at this offset survives.
                    Gain::Jump => {
                        jump = true;
                        self.aim = None;
                    }
                }
                next += 1;
            }
            let x = frame0.get() + i as u64;
            if jump {
                self.gain = F::ZERO;
                self.run = Self::rise(fade);
            } else {
                match self.aim {
                    Some(to) if x >= to.get() => {
                        // On the target: zero, and the fade-in starts from
                        // here (whether or not the command was applied).
                        self.gain = F::ZERO;
                        self.aim = None;
                        self.run = Self::rise(fade);
                    }
                    Some(to) if to.get() - x <= fade as u64 => {
                        // A fade-out over the frames left, from the gain the
                        // frame has (a fade-in cut short continues from there).
                        let run = match self.run {
                            Some(r) if r.aim == Some(to) => r,
                            _ => Run {
                                from: self.gain,
                                k: 0,
                                n: (to.get() - x) as usize,
                                aim: Some(to),
                            },
                        };
                        (self.gain, self.run) = run.step();
                    }
                    _ => {
                        let run = match self.run {
                            Some(r) if r.aim.is_none() => Some(r),
                            _ if self.gain < F::ONE => Some(Run {
                                from: self.gain,
                                k: 0,
                                n: fade,
                                aim: None,
                            }),
                            _ => None,
                        };
                        match run {
                            Some(r) => (self.gain, self.run) = r.step(),
                            None => self.run = None,
                        }
                    }
                }
            }
            if self.gain != F::ONE {
                let at = (base + i) * channels;
                for s in &mut output[at..at + channels] {
                    *s *= self.gain;
                }
            }
        }
    }
}

/// Collects the cuts for the block's `Env`; the render happens once, after.
struct GraphPieces {
    changes: TransportChanges,
}

impl GraphPieces {
    /// How many more cuts fit in this block.
    fn free(&self) -> usize {
        tutti_graph::MAX_TRANSPORT_CHANGES - self.changes.len()
    }

    /// Whether another cut fits in this block.
    fn room(&self) -> bool {
        self.free() > 0
    }

    /// Record the transport changing to `t` at `at`.
    fn change(&mut self, at: Offset, t: GraphTransport) {
        // Cannot fail: `free` was checked before the cut, cuts come in time
        // order, and one at the same frame replaces.
        let _ = self.changes.push(at, t);
    }
}

impl<F: Float> GraphRender<F> {
    /// Bring the executor up to date before a graph block: install queued
    /// commits (a re-prepare's resume adopts its `Prepare` here, so the block
    /// length below is the new maximum and not a stale one), and follow a
    /// rate change with the clock. Returns the longest block to hand the
    /// executor, and its rate.
    ///
    /// The rate is followed on the block the re-prepare's **first** commit
    /// lands, not its resume: from that block the executor counts frames at
    /// the new rate (it has rescaled its clock and its own `At::Frame`
    /// commands, `Executor::pending_prepare`), and it renders silence while
    /// the transport rolls on. So the engine's clock steps the beat at the
    /// new rate from there too — a clock left at the old one would move the
    /// playhead by `old / new` of a frame per frame until the resume, a
    /// jump of the suspended stretch's worth — and the transport's
    /// frame-timed commands (`schedule`) move with the executor's.
    fn settle(&mut self) -> (usize, SampleRate) {
        self.exec.apply_pending();
        let rate = self
            .exec
            .pending_prepare()
            .unwrap_or(self.exec.prepare())
            .sample_rate();
        // A re-prepare that changed the rate: the executor has rescaled its
        // frame clock; the beat increment and the transport's own
        // frame-timed commands follow.
        self.transport.follow_rate(rate);
        let bound = self.exec.prepare().max_block().get();
        // The editor's limits keep every `MaxBlock` within the scratch.
        debug_assert!(bound <= self.stride, "MaxBlock {bound} past the scratch");
        (bound.min(self.stride), rate)
    }

    /// The events the executor dropped since the last call: a port past its
    /// capacity, or a full delay or feedback FIFO.
    fn take_dropped_events(&mut self) -> u64 {
        let total = self.exec.dropped_event_count();
        let new = total.wrapping_sub(self.dropped_seen);
        self.dropped_seen = total;
        new
    }

    /// Render one graph block of `len` frames into frames
    /// `at..at + len` of `output`, folded from the graph's width to
    /// `out_ch`.
    fn render(
        &mut self,
        output: &mut [F],
        lent: Lent<'_, F>,
        out_ch: usize,
        at: usize,
        len: usize,
        transport: &GraphTransport,
        changes: &TransportChanges,
    ) -> bool {
        // `settle` installed every queued commit, and the executor renders
        // without applying again: a commit sent since waits for the next
        // block, so the width, the block bound and the rate read here are the
        // ones the block renders with. The editor's limits refuse a graph
        // wider than the scratch.
        let (main_output, width, rendered) =
            self.exec.plan().map_or((MainOutputId::GENESIS, 0, 0), |p| {
                (p.main_output_id(), p.main_outputs(), p.rendered_outputs())
            });
        // While a main output change crossfades, the old main output's folds are still in
        // use: the control side retires none past the oldest main output rendered.
        let fade = self.exec.main_output_fade();
        let held = fade.map_or(main_output, |f| f.old);
        // A relaxed store: the control side only needs it to catch up.
        self.installed.store(held.get(), Ordering::Relaxed);
        debug_assert!(width <= MAX_CHANNELS, "the editor's limits refuse this");
        let width = width.min(MAX_CHANNELS);
        if width > 0 && out_ch > 0 {
            self.in_use
                .store(pack_in_use(width, out_ch), Ordering::Relaxed);
        }
        let block = &mut output[at * out_ch..(at + len) * out_ch];
        let stride = self.stride;
        let mut chunks = self.scratch.chunks_mut(stride);
        let mut outs: [&mut [F]; MAIN_OUTPUT_BANKS * MAX_CHANNELS] =
            std::array::from_fn(|_| chunks.next().expect("a chunk per rendered channel"));
        let mut mixing = Mixing {
            fold: chunks
                .next()
                .expect("a chunk to fold the new main output into"),
            old: chunks
                .next()
                .expect("a chunk to fold the old main output into"),
            gain_old: chunks
                .next()
                .expect("a chunk for the old main output's gains"),
            gain_new: chunks
                .next()
                .expect("a chunk for the new main output's gains"),
        };
        // The project's meter, lent once for this block and dropped with it;
        // the tempo map is the segment's, lent before its walk.
        {
            let meter = self.transport.meter();
            self.exec.render_applied(
                len,
                transport,
                changes,
                &[],
                &mut outs[..rendered.min(MAIN_OUTPUT_BANKS * MAX_CHANNELS)],
                Some(&*meter),
                Some(lent.tempo),
            );
        }
        // The block that ends the fade is the old main output's last: from here the
        // new main output is the oldest in use.
        self.installed.store(
            self.exec
                .main_output_fade()
                .map_or(main_output, |f| f.old)
                .get(),
            Ordering::Relaxed,
        );
        // Published after the block, not by the walk before it: through the
        // block the live playhead still reads its first frame (the last
        // block's end), and it only moves forward.
        self.transport.publish_position();
        // A new routing generation starts its fade from the first block that
        // lends it; the first block of all has nothing to fade from.
        let generation = lent.folds.generation();
        let length = declick_frames(self.exec.prepare().sample_rate()).get();
        if self.routing.seen != Some(generation) {
            let first = self.routing.seen.is_none();
            self.routing = RoutingRun {
                seen: Some(generation),
                done: if first { length } else { 0 },
            };
        }
        if width == 0 {
            // No fold plays, so nothing fades: the replaced table is free.
            self.routing.done = length;
            self.routing_ended.store(generation, Ordering::Release);
            block.fill(F::ZERO);
            return true;
        }
        // The fold of this width into the device, built on the control side.
        // `width` is within `1..=MAX_CHANNELS` (the editor's limits), so a
        // fold is there.
        match lent.folds.get(main_output, out_ch, width) {
            Some(fold) => {
                let mut device = ChannelLayout::try_from(out_ch)
                    .ok()
                    .and_then(|layout| InterleavedMut::new(block, layout).ok())
                    .expect("a fold exists only for a held device width, and the block is whole frames of it");
                // The rendered channels, a block long each, as one table on
                // the stack.
                let mut table: [&[F]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
                for (slot, out) in table.iter_mut().zip(&outs).take(width) {
                    *slot = &out[..len];
                }
                // The routing change's fade: the table it replaced folds the
                // same channels. A main output fade, or a main output the replaced table
                // never held, cuts it.
                let old_routing = lent
                    .folds
                    .previous()
                    .filter(|_| fade.is_none() && self.routing.done < length)
                    .and_then(|previous| previous.get(main_output, out_ch, width));
                let folded = match fade {
                    None => match old_routing {
                        Some(old) => mixing.crossfade(
                            Crossfaded {
                                new: (fold, &table[..width]),
                                old: Some((old, &table[..width])),
                                fade: FadeRun {
                                    law: FadeLaw::EqualAmplitude,
                                    length,
                                    done: self.routing.done,
                                },
                            },
                            device.reborrow(),
                        ),
                        None => Planes::new(&table[..width]).and_then(|planes| {
                            fold.fold_planar_to_interleaved(planes, device.reborrow())
                        }),
                    },
                    Some(fade) => {
                        // The old main output's channels follow the new main output's.
                        let old = fade.width.min(MAX_CHANNELS);
                        let mut old_table: [&[F]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
                        for (slot, out) in old_table.iter_mut().zip(&outs[width..]).take(old) {
                            *slot = &out[..len];
                        }
                        mixing.crossfade(
                            Crossfaded {
                                new: (fold, &table[..width]),
                                old: lent
                                    .folds
                                    .get(fade.old, out_ch, old)
                                    .map(|fold| (fold, &old_table[..old])),
                                fade: FadeRun {
                                    law: fade.fade.law,
                                    length: fade.fade.duration.get(),
                                    done: fade.done.get(),
                                },
                            },
                            device.reborrow(),
                        )
                    }
                };
                // Past the fade, or cut by a main output fade or a missing fold: the
                // control side may drop the replaced table.
                if old_routing.is_some() {
                    self.routing.done += len;
                } else {
                    self.routing.done = length;
                }
                if self.routing.done >= length {
                    self.routing_ended.store(generation, Ordering::Release);
                }
                debug_assert!(
                    folded.is_ok(),
                    "the main output's channels are a block long and the fold is for its width: {folded:?}"
                );
                if folded.is_err() {
                    // Not placed: silent, and the caller counts it.
                    device.samples_mut().fill(F::ZERO);
                    return false;
                }
                // The output gains, ramped over the declick length.
                let ramp = declick_frames(self.exec.prepare().sample_rate()).get();
                self.stage
                    .apply(device.samples_mut(), out_ch, lent.stage, ramp);
                true
            }
            // A main output width that cannot be placed on this device: silent, and
            // the caller counts it.
            None => {
                self.routing.done = length;
                self.routing_ended.store(generation, Ordering::Release);
                block.fill(F::ZERO);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::Bed;
    use tutti_buffers::ChannelTopology;
    use tutti_buffers::MaxBlock;
    use tutti_buffers::Placement;
    use tutti_graph::Prepare;
    use tutti_timeline::Timeline;
    use tutti_timeline::{MotionCommand, Transport};
    use tutti_units::Amplitude;

    /// `GraphEngineError` is an `Error` a caller can box and move; the bound is
    /// checked where the function is written, so a non-`Send` field fails the build.
    #[allow(dead_code)]
    fn the_engine_error_is_boxable() {
        fn err<T: std::error::Error + Send + Sync + 'static>() {}
        err::<GraphEngineError>();
    }

    /// **A `Frames` engine ignores the instant a timer says a block was due**:
    /// block 0 is the origin whatever `process_at` is given, and the next is
    /// 512 frames at 48 kHz later (10 666 667 ns, rounded), not the due times'
    /// difference.
    #[test]
    fn frames_dating_ignores_the_due_stamp() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec)
            .expect("empty graph")
            .dates(crate::BlockDates::Frames);
        let reader = engine.block_clock();
        let mut samples = vec![0.0f32; 512 * 2];
        let mut out = InterleavedMut::new(&mut samples, ChannelLayout::STEREO).unwrap();
        engine.process_at(&mut out, 5);
        let first = reader.read().expect("dated").os_ns;
        assert_ne!(first, 5, "the due stamp is not the date");
        engine.process_at(&mut out, 1_000_000_000_000);
        let second = reader.read().expect("dated").os_ns;
        assert_eq!(second - first, 10_666_667);
    }

    /// The audio thread is never the side that panics: a block that meets
    /// the graph held on the control side (`apply_pending` racing a callback)
    /// renders silence and leaves the command queue for the next block, which
    /// applies it.
    #[test]
    fn a_block_refused_the_graph_is_silent_and_retries() {
        use tutti_buffers::ChannelLayout;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let _ = transport.motion().send(MotionCommand::Play);
        let mut buf = vec![1.0f32; 64 * 2];
        let mut seg = vec![1.0f32; 64 * 2];
        {
            let _control = engine.graph.borrow_mut();
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            engine.process_segment(
                &mut InterleavedMut::new(&mut seg, ChannelLayout::STEREO).unwrap(),
            );
        }
        assert!(buf.iter().all(|&x| x == 0.0), "a refused block is silent");
        assert!(seg.iter().all(|&x| x == 0.0), "a refused segment is silent");
        assert!(!transport.is_rolling(), "the queued Play waits");
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        assert!(transport.is_rolling(), "the next block applies it");
    }

    /// A refused block is silence in the heard tap too: the tap keeps a
    /// take's timing, as a recording of the device would, and the frames are
    /// written (not counted lost). The plan tap, an op of the graph, has no
    /// frames for a block the graph did not render.
    #[test]
    fn a_refused_block_is_silence_in_the_heard_tap() {
        use tutti_buffers::ChannelLayout;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let mut engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let mut tap = engine
            .take_heard_taps()
            .expect("a fresh engine holds it")
            .open(ChannelLayout::STEREO, Samples::new(256));
        let mut buf = vec![1.0f32; 64 * 2];
        let mut seg = vec![1.0f32; 32 * 2];
        {
            let _control = engine.graph.borrow_mut();
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            engine.process_segment(
                &mut InterleavedMut::new(&mut seg, ChannelLayout::STEREO).unwrap(),
            );
        }
        let mut got = vec![f32::NAN; 96 * 2];
        assert_eq!(
            tap.pop_into(InterleavedMut::new(&mut got, tap.layout()).unwrap())
                .unwrap(),
            Samples::new(96)
        );
        assert!(got.iter().all(|&x| x == 0.0), "silence, whole frames");
        assert_eq!(tap.take_lost(), Samples::new(0));
    }

    /// The motion machine outlives the engine that ran it: an engine built
    /// after the first is dropped resumes the same state, so a transport
    /// left rolling can still be stopped.
    #[test]
    fn a_later_engine_resumes_the_motion_machine() {
        use tutti_buffers::ChannelLayout;
        let transport = Transport::new(SampleRate::SR_48K);
        let mut buf = vec![0.0f32; 64 * 2];
        {
            let (mut ed, exec) = Editor::new(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(512)),
            ));
            let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
            let _ = transport.motion().send(MotionCommand::Play);
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            assert!(transport.is_rolling());
        }
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("the first engine is gone");
        let _ = transport.motion().send(MotionCommand::stop_unfaded());
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        assert!(!transport.is_rolling(), "the machine knew it was rolling");
    }

    /// A clone of a transport shares its motion machine, which is where the
    /// one claim is held: an engine over it while the other's runs is refused.
    #[test]
    fn a_shared_motion_handle_is_refused_as_held() {
        let first = Transport::new(SampleRate::SR_48K);
        let second = first.clone();
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let _running = Engine::new(&first, &mut ed, exec).expect("empty graph");
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        assert!(matches!(
            Engine::new(&second, &mut ed, exec),
            Err(GraphEngineError::TransportHeld(_))
        ));
    }

    /// `send` and a `NextBlock` command are one queue, so they apply in
    /// the order they were sent: a `Stop` then a `Play` ends rolling, a
    /// `Play` then a `Stop` ends stopped.
    #[test]
    fn send_and_a_next_block_command_apply_in_send_order() {
        use tutti_buffers::ChannelLayout;
        let run = |first_is_play: bool| {
            let transport = Transport::new(SampleRate::SR_48K);
            let (mut ed, exec) = Editor::new(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(512)),
            ));
            let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
            let stop = MotionCommand::stop_unfaded();
            if first_is_play {
                transport.motion().send(MotionCommand::Play).expect("room");
                transport
                    .motion()
                    .schedule(At::NextBlock, stop)
                    .expect("room");
            } else {
                transport
                    .motion()
                    .schedule(At::NextBlock, stop)
                    .expect("room");
                transport.motion().send(MotionCommand::Play).expect("room");
            }
            let mut buf = vec![0.0f32; 64 * 2];
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            transport.is_rolling()
        };
        assert!(!run(true), "play then stop ends stopped");
        assert!(run(false), "stop then play ends rolling");
    }

    /// A locate sent with `send` lands on the block's first frame and
    /// its seek is taken once, after the command: the block starts at the
    /// target, a loop wrap inside it is counted once, and the next block
    /// continues from the end of this one without taking the seek again.
    ///
    /// Analytic figures: 120 BPM at 48 kHz is 1/24 000 beat a frame.
    #[test]
    fn a_locate_on_the_first_frame_is_taken_once_and_a_wrap_counted_once() {
        use tutti_buffers::ChannelLayout;
        use tutti_music::LoopRange;
        use tutti_timeline::LoopEdit;
        use tutti_units::Beat;
        let engine_for = |transport: &Transport| {
            let (mut ed, exec) = Editor::new(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(2048)),
            ));
            Engine::new(transport, &mut ed, exec).expect("empty graph")
        };

        let transport = Transport::new(SampleRate::SR_48K);
        let engine = engine_for(&transport);
        transport
            .motion()
            .send(MotionCommand::play_from(Beat::new(8.0)))
            .expect("room");
        let mut buf = vec![0.0f32; 64 * 2];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        let first = transport.beat().get();
        assert!((first - (8.0 + 64.0 / 24_000.0)).abs() < 1e-9, "{first}");
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        let second = transport.beat().get();
        assert!((second - (8.0 + 128.0 / 24_000.0)).abs() < 1e-9, "{second}");

        // Into a loop's last frames: one wrap, counted once.
        let transport = Transport::new(SampleRate::SR_48K);
        let engine = engine_for(&transport);
        transport
            .motion()
            .schedule(
                At::NextBlock,
                LoopEdit::over(LoopRange::new(Beat::new(0.0), Beat::new(4.0)).expect("ordered")),
            )
            .expect("room");
        transport
            .motion()
            .send(MotionCommand::play_from(Beat::new(3.99)))
            .expect("room");
        let mut buf = vec![0.0f32; 1024 * 2];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        let wrapped = transport.beat().get();
        let expected = 3.99 + 1024.0 / 24_000.0 - 4.0;
        assert!((wrapped - expected).abs() < 1e-9, "{wrapped} vs {expected}");
    }

    /// A block with no declicked command in sight plans no gain event, and
    /// the fader leaves the output untouched: the fast path is reachable.
    /// A command in sight plans its aim once, not at every walk step.
    #[test]
    fn a_quiet_block_plans_no_gain_event() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let _ = transport.motion().send(MotionCommand::Play);
        let mut render = engine.graph.borrow_mut();
        let mut pieces = GraphPieces {
            changes: TransportChanges::NONE,
        };
        let mut crossings = BeatCrossings::new();
        let mut tempos = engine.tempo.borrow_mut();
        let generation = render.transport.tempo_generation();
        let map = tempos.lend();
        let walk = engine.walk(
            Track {
                transport: &mut render.transport,
                crossings: &mut crossings,
            },
            Frame::ZERO,
            512,
            SampleRate::SR_48K,
            None,
            &mut pieces,
            (generation, &map),
        );
        assert_eq!(walk.gain.len, 0, "nothing planned");
        let mut out = vec![0.25f32; 512];
        let mut fader = Fader::new(fade_frames(SampleRate::SR_48K));
        fader.apply(&walk.gain, Frame::ZERO, &mut out, 1, 0, 512);
        assert!(out.iter().all(|&x| x == 0.25), "untouched");

        // A declicked stop 300 frames into the next block: one aim, at 0.
        transport
            .motion()
            .schedule(
                At::Frame(Frame::new(700)),
                tutti_timeline::MotionCommand::stop(),
            )
            .expect("room");
        let generation = render.transport.tempo_generation();
        let walk = engine.walk(
            Track {
                transport: &mut render.transport,
                crossings: &mut crossings,
            },
            Frame::ZERO,
            512,
            SampleRate::SR_48K,
            None,
            &mut pieces,
            (generation, &map),
        );
        assert_eq!(walk.gain.len, 1);
        assert_eq!(walk.gain.items[0], (0, Gain::Aim(Some(Frame::new(700)))));
    }

    /// A block refused the graph is a dropout the counter counts, not times:
    /// `silenced` counts both entry points and `blocks` neither.
    #[test]
    fn a_block_refused_the_graph_counts_as_silenced() {
        use tutti_buffers::ChannelLayout;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let mut buf = vec![0.0f32; 64 * 2];
        {
            let _control = engine.graph.borrow_mut();
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            engine.process_segment(
                &mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap(),
            );
        }
        let r = engine.blocks().get();
        assert_eq!((r.silenced, r.blocks), (2, 0));
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        let r = engine.blocks().get();
        assert_eq!((r.silenced, r.blocks), (2, 1));
    }

    /// `width` outputs, channel `c` holding `10 * width + c + 1`.
    struct Ramp(u16);

    impl tutti_graph::Node for Ramp {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::source_unassigned(tutti_buffers::ChannelLayout::from_count(self.0))
                .tail(tutti_units::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            for c in 0..usize::from(self.0) {
                io.output(c).fill((10 * usize::from(self.0) + c + 1) as f32);
            }
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// Wire the main outputs to a `Ramp` of `width` at key 1, replacing any, and
    /// commit.
    fn rewire(ed: &mut Editor, width: u16) {
        use tutti_graph::NodeKey;
        use tutti_graph::{OutputPort, PortSource};
        ed.insert(NodeKey(1), "ramp", tutti_graph::Unforkable(Ramp(width)));
        ed.spec_mut().topology.set_outputs(
            (0..width).map(|port| PortSource::Node(OutputPort::new(NodeKey(1), port))),
        );
        ed.commit().expect("commits");
    }

    /// Declares `main_output` as the next main output generation the way a session does:
    /// its folds are published first, then the spec carries the id and the
    /// commit is sent.
    fn declare(
        ed: &mut Editor,
        control: &OutputControl,
        main_output: Option<ChannelTopology>,
        device: ChannelLayout,
    ) -> Result<MainOutputId, tutti_graph::MainOutputError> {
        let id = ed.spec().main_output_id.next();
        control.publish_main_output(id, main_output, device)?;
        ed.spec_mut().main_output_id = id;
        ed.commit().expect("commits");
        Ok(id)
    }

    /// The frames of `samples` read as `width`-channel interleaved audio.
    fn frames_of(samples: &[f32], width: u16) -> impl Iterator<Item = &[f32]> {
        tutti_buffers::Interleaved::new(samples, ChannelLayout::from(width))
            .expect("whole frames")
            .iter()
    }

    /// One graph block as `process_segment` runs it, with `between` run after
    /// the walk and before `render`: the control thread's commit or publish
    /// landing in the gap.
    fn block_with(engine: &Engine, out_ch: usize, between: impl FnOnce()) -> Vec<f32> {
        block_of(engine, out_ch, 8, between)
    }

    /// [`block_with`] for a block of `want` frames (at most the bound).
    fn block_of(engine: &Engine, out_ch: usize, want: usize, between: impl FnOnce()) -> Vec<f32> {
        let mut g = engine.graph.borrow_mut();
        let (bound, _) = g.settle();
        assert!(bound >= want);
        let len = want;
        // Lent right after `settle`, as `process_segment` does: a publish in
        // the gap is not in this snapshot, and its plan is not installed.
        let mut reader = engine.folds.borrow_mut();
        let folds = reader.lend();
        let mut stages = engine.stage.borrow_mut();
        let stage = stages.lend();
        let mut tempos = engine.tempo.borrow_mut();
        let generation = g.transport.tempo_generation();
        let map = tempos.lend();
        let t = g.transport.begin_block(generation, &map);
        g.transport.advance(len, &t, &map);
        between();
        let mut out = vec![-9.0f32; len * out_ch];
        g.render(
            &mut out,
            Lent {
                folds: &folds,
                stage: &stage,
                tempo: &map,
            },
            out_ch,
            0,
            len,
            &t,
            &TransportChanges::NONE,
        );
        out
    }

    /// Every main output width that can be placed has a fold into every supported
    /// device width, and the fold's figures are the analytic ones: a 5.1 main output
    /// on a stereo device is the ITU fold, a mono main output on a 5.1 device is its
    /// centre, a 3-wide main output (no order) on a 5-wide device is its channels
    /// zero-filled. A narrowing with an unordered end (5 into 3, 8 into 7, 6
    /// into 5) has no fold, and renders a silent counted block rather than a
    /// truncation.
    #[test]
    fn every_main_output_width_places_into_any_device() {
        let folds = OutputFolds::up_to(DEFAULT_DEVICE_WIDTHS);
        for main_output in 1..=MAX_CHANNELS {
            for device in 1..=DEFAULT_DEVICE_WIDTHS {
                let ordered = |w: usize| [1, 2, 4, 6, 8, 12].contains(&w);
                let refused = main_output > device && !(ordered(main_output) && ordered(device));
                let Some(fold) = folds.get(MainOutputId::GENESIS, device, main_output) else {
                    assert!(refused, "{main_output} -> {device} has no fold");
                    continue;
                };
                assert!(!refused, "{main_output} -> {device} must have no fold");
                assert_eq!(usize::from(fold.src_layout().count()), main_output);
                assert_eq!(usize::from(fold.dst_layout().count()), device);
            }
        }
        for (main_output, device) in [(5, 3), (6, 5), (8, 7), (3, 1), (4, 3)] {
            assert!(
                folds
                    .get(MainOutputId::GENESIS, device, main_output)
                    .is_none(),
                "{main_output} -> {device}"
            );
            assert!(folds.has_device(device));
        }
        let first = |device, main_output| folds.get(MainOutputId::GENESIS, device, main_output);
        assert!(first(DEFAULT_DEVICE_WIDTHS + 1, 2).is_none());
        assert!(first(2, MAX_CHANNELS + 1).is_none());
        assert!(first(0, 2).is_none() && first(2, 0).is_none());
        assert!(
            folds.get(MainOutputId::GENESIS.next(), 2, 2).is_none(),
            "a main output never published has no fold"
        );

        let m = <f32 as Float>::MINUS_3_DB;
        let run = |main_output: usize, device: usize, frame: &[f32]| {
            let mut out = vec![9.0f32; device];
            folds
                .get(MainOutputId::GENESIS, device, main_output)
                .unwrap()
                .frame(frame, &mut out);
            out
        };
        assert_eq!(
            run(6, 2, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            [1.0 + m * 3.0 + m * 5.0, 2.0 + m * 3.0 + m * 6.0]
        );
        assert_eq!(run(1, 6, &[0.5]), [0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        assert_eq!(run(3, 5, &[1.0, 2.0, 3.0]), [1.0, 2.0, 3.0, 0.0, 0.0]);
    }

    /// A device wider than the default is added on the control side, and a
    /// buffer of a width nobody added is a silent counted block rather than a
    /// fold built on the audio thread.
    #[test]
    fn a_wider_device_is_added_and_an_unsupported_width_is_silent() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let wide = ChannelLayout::from(20u16);
        engine.support_output(wide).expect("a width");
        let mut out = vec![-9.0f32; 20 * 8];
        engine.process_segment(&mut InterleavedMut::new(&mut out, wide).unwrap());
        // Ramp(2) writes 21, 22: the front pair, the rest zero-filled.
        for frame in frames_of(&out, 20) {
            assert_eq!(&frame[..2], &[21.0, 22.0]);
            assert!(frame[2..].iter().all(|&s| s == 0.0));
        }
        assert_eq!(
            engine.support_output(ChannelLayout::EMPTY),
            Err(FoldError::Buffer(tutti_buffers::BufferError::ZeroWidth))
        );
        // Not added: silent, and counted, in every build.
        let mut out = vec![-9.0f32; 40 * 8];
        engine.process_segment(
            &mut InterleavedMut::new(&mut out, ChannelLayout::from(40u16)).unwrap(),
        );
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(engine.blocks().get().silenced, 1);
    }

    /// A main output the device has no place for (five channels into three: either
    /// width has no order, and two channels would be dropped) renders a silent
    /// block, counted, instead of its first three channels; into a wider device
    /// the same main output plays.
    #[test]
    fn a_main_output_the_device_cannot_take_is_silent_and_counted() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 5);
        let three = ChannelLayout::from(3u16);
        let mut out = vec![-9.0f32; 3 * 8];
        engine.process_segment(&mut InterleavedMut::new(&mut out, three).unwrap());
        assert!(out.iter().all(|&s| s == 0.0), "{out:?}");
        assert_eq!(engine.blocks().get().silenced, 1);
        let seven = ChannelLayout::from(7u16);
        let mut out = vec![-9.0f32; 7 * 8];
        engine.process_segment(&mut InterleavedMut::new(&mut out, seven).unwrap());
        // Ramp(5) writes 51..=55; the rest of the 7-wide device is silent.
        assert_eq!(&out[..7], &[51.0, 52.0, 53.0, 54.0, 55.0, 0.0, 0.0]);
        assert_eq!(engine.blocks().get().silenced, 1);
    }

    /// Renders one 8-frame segment of the committed graph into a device of `device`
    /// channels, after settling the first block (the commit installs on the
    /// block after it), and returns frame 0.
    fn first_frame(engine: &Engine, device: usize) -> Vec<f32> {
        let layout = ChannelLayout::try_from(device).expect("a test width is small");
        let mut out = vec![-9.0f32; device * 8];
        engine.process_segment(&mut InterleavedMut::new(&mut out, layout).unwrap());
        out[..device].to_vec()
    }

    /// [`first_frame`] after a routing change's crossfade has run out.
    fn first_frame_after_fade(engine: &Engine, device: usize) -> Vec<f32> {
        let layout = ChannelLayout::try_from(device).expect("a test width is small");
        let mut out = vec![0.0f32; device * (declick_frames(SampleRate::SR_48K).get() + 8)];
        engine.process_segment(&mut InterleavedMut::new(&mut out, layout).unwrap());
        first_frame(engine, device)
    }

    /// A main output of four independent buses, declared, stays by index on a 6-wide
    /// device: channels 2 and 3 land at 2 and 3. Undeclared, the same main output is
    /// quad, and its rears land on the side pair at 4 and 5.
    #[test]
    fn a_declared_unassigned_main_output_keeps_its_channels_by_index() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 4);
        let six = ChannelLayout::from(6u16);
        // The first block installs the plan (silent); the rows read the next.
        let _ = first_frame(&engine, 6);
        assert_eq!(first_frame(&engine, 6), [41.0, 42.0, 0.0, 0.0, 43.0, 44.0]);

        let control = engine.output_control();
        let buses = Some(ChannelTopology::unassigned(ChannelLayout::QUAD));
        declare(&mut ed, &control, buses, six).expect("buses place by index");
        assert_eq!(first_frame(&engine, 6), [41.0, 42.0, 43.0, 44.0, 0.0, 0.0]);

        declare(&mut ed, &control, None, six).expect("undeclared");
        assert_eq!(first_frame(&engine, 6), [41.0, 42.0, 0.0, 0.0, 43.0, 44.0]);
    }

    /// A declared 5.1 main output folds into stereo by the 5.1 rules (the ITU matrix:
    /// centre and each side at -3 dB).
    ///
    /// The declared 5.1 is the figure the bare width gives, so the last row
    /// declares the same channels with the front pair swapped to show the
    /// declaration is what is folded.
    #[test]
    fn a_declared_surround_main_output_folds_by_its_speakers() {
        use tutti_buffers::Speaker::*;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 6);
        let _ = first_frame(&engine, 2);
        let m = <f32 as Float>::MINUS_3_DB;
        let folded = [61.0 + m * 63.0 + m * 65.0, 62.0 + m * 63.0 + m * 66.0];
        assert_eq!(first_frame(&engine, 2), folded);
        let control = engine.output_control();
        let five_one = ChannelTopology::default_for(ChannelLayout::from(6u16));
        declare(&mut ed, &control, Some(five_one), ChannelLayout::STEREO).expect("5.1 into stereo");
        assert_eq!(first_frame(&engine, 2), folded);
        // The same six channels declared with the front pair swapped.
        let swapped = ChannelTopology::new([
            FrontRight,
            FrontLeft,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
        ])
        .unwrap();
        declare(&mut ed, &control, Some(swapped), ChannelLayout::STEREO).expect("places");
        assert_eq!(
            first_frame(&engine, 2),
            [62.0 + m * 63.0 + m * 65.0, 61.0 + m * 63.0 + m * 66.0]
        );
    }

    /// The fold follows the plan, not the publication history: main outputs published
    /// and never committed (a quad of buses, a 5.1, a 7.1) leave the plan
    /// rendering on the fold of the main output it carries, and the commit that
    /// carries a published main output's id is the block where that main output's fold
    /// takes over, with the plan.
    #[test]
    fn a_published_main_output_takes_over_with_the_plan_that_carries_its_id() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 4);
        let eight = ChannelLayout::from(8u16);
        let _ = first_frame(&engine, 8);
        let control = engine.output_control();
        let quad_on_eight = [41.0, 42.0, 0.0, 0.0, 0.0, 0.0, 43.0, 44.0];
        assert_eq!(first_frame(&engine, 8), quad_on_eight);
        let mut id = ed.spec().main_output_id;
        for main_output in [
            ChannelTopology::unassigned(ChannelLayout::QUAD),
            ChannelTopology::default_for(ChannelLayout::from(6u16)),
            ChannelTopology::default_for(eight),
        ] {
            id = id.next();
            control
                .publish_main_output(id, Some(main_output), eight)
                .unwrap();
            assert_eq!(first_frame(&engine, 8), quad_on_eight, "{id}");
        }
        // The plan that carries the by-index quad's id: its fold, at once.
        id = id.next();
        control
            .publish_main_output(
                id,
                Some(ChannelTopology::unassigned(ChannelLayout::QUAD)),
                eight,
            )
            .unwrap();
        ed.spec_mut().main_output_id = id;
        ed.commit().unwrap();
        assert_eq!(
            first_frame(&engine, 8),
            [41.0, 42.0, 43.0, 44.0, 0.0, 0.0, 0.0, 0.0]
        );
    }

    /// A main output change is bit-exact at the plan swap: in one engine block the
    /// plan of the old main output renders with the old main output's fold, and the next
    /// block, the first of the new plan, with the new main output's, with no block of
    /// the new plan folded by the old main output or the other way round.
    #[test]
    fn the_fold_swaps_at_the_block_the_plan_swaps() {
        use std::cell::RefCell;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        rewire(&mut ed, 4);
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("within limits");
        let control = engine.output_control();
        let six = ChannelLayout::from(6u16);
        let _ = block_with(&engine, 6, || {});
        let quad = [41.0, 42.0, 0.0, 0.0, 43.0, 44.0];
        let buses = [41.0, 42.0, 43.0, 44.0, 0.0, 0.0];
        let frames = |row: [f32; 6]| -> Vec<f32> { (0..8).flat_map(|_| row).collect() };
        assert_eq!(block_with(&engine, 6, || {}), frames(quad));
        // The commit lands after this block's `settle`: this block is the old
        // plan and the old fold, the next the new plan and the new fold.
        let got = block_with(&engine, 6, || {
            declare(
                &mut ed.borrow_mut(),
                &control,
                Some(ChannelTopology::unassigned(ChannelLayout::QUAD)),
                six,
            )
            .unwrap();
        });
        assert_eq!(got, frames(quad));
        assert_eq!(block_with(&engine, 6, || {}), frames(buses));
    }

    /// The first block of a main output committed right after its publish is
    /// rendered with its own fold, and nothing is counted unplaced.
    #[test]
    fn a_main_output_committed_after_its_publish_renders_its_first_block_folded() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 4);
        let six = ChannelLayout::from(6u16);
        let _ = first_frame(&engine, 6);
        let control = engine.output_control();
        let buses = Some(ChannelTopology::unassigned(ChannelLayout::QUAD));
        declare(&mut ed, &control, buses, six).unwrap();
        let before = engine.blocks().get().silenced;
        assert_eq!(first_frame(&engine, 6), [41.0, 42.0, 43.0, 44.0, 0.0, 0.0]);
        assert_eq!(
            engine.blocks().get().silenced,
            before,
            "the first block of the new main output is not silenced"
        );
    }

    /// The fold set is lent after the plan is installed, not before: with the
    /// control side publishing and committing a new main output generation while the
    /// audio side renders (on another thread, as in production) calls of many
    /// segments, no block is counted unplaced. A set lent at the callback's entry misses a main output
    /// published after it whose plan `settle` then installs, and that block is
    /// silent.
    #[test]
    fn a_main_output_published_while_blocks_render_is_never_unplaced() {
        use std::sync::atomic::AtomicBool;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 4);
        let control = engine.output_control();
        let six = ChannelLayout::from(6u16);
        let done = AtomicBool::new(false);
        let silenced = std::thread::scope(|scope| {
            let audio = scope.spawn(|| {
                // Many graph segments per call: a commit sent mid-call is installed by a
                // later segment's `settle`.
                let mut out = vec![0.0f32; 6 * 64 * 256];
                let mut blocks = 0u32;
                while !done.load(Ordering::Acquire) || blocks < 64 {
                    engine.process_segment(&mut InterleavedMut::new(&mut out, six).unwrap());
                    blocks += 1;
                }
                engine.blocks().get().silenced
            });
            for i in 0..2_000 {
                let main_output =
                    (i % 2 == 0).then(|| ChannelTopology::unassigned(ChannelLayout::QUAD));
                let id = ed.spec().main_output_id.next();
                match control.publish_main_output(id, main_output, six) {
                    Ok(()) => {
                        ed.spec_mut().main_output_id = id;
                        // The queue can be full while the callback lags.
                        while ed.commit().is_err() {
                            std::thread::yield_now();
                        }
                    }
                    // Too many main outputs await a plan: let a block render.
                    Err(tutti_graph::MainOutputError::TooManyPending { .. }) => {
                        std::thread::yield_now();
                    }
                    Err(e) => panic!("{e}"),
                }
                let _ = control.reclaim();
            }
            done.store(true, Ordering::Release);
            audio.join().unwrap()
        });
        assert_eq!(
            silenced, 0,
            "no block met a plan whose fold it did not hold"
        );
    }

    /// A main output id that is not past every published one is refused by name, at
    /// the call, and publishes nothing.
    #[test]
    fn a_main_output_id_not_past_the_published_ones_is_refused() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let control = engine.output_control();
        let stereo = ChannelLayout::STEREO;
        let first = MainOutputId::GENESIS.next();
        control.publish_main_output(first, None, stereo).unwrap();
        for stale in [MainOutputId::GENESIS, first] {
            assert_eq!(
                control.publish_main_output(stale, None, stereo),
                Err(tutti_graph::MainOutputError::NotPast {
                    id: stale,
                    last: first
                })
            );
        }
    }

    /// With no plan rendering (a stopped stream) the published main outputs are
    /// bounded: past `MAX_PENDING_MAIN_OUTPUTS` a publish is refused by name and the
    /// table stops growing, and one rendered block (which advances the main output in
    /// force) frees the room.
    #[test]
    fn main_outputs_published_while_nothing_renders_are_bounded() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let _ = first_frame(&engine, 2);
        let control = engine.output_control();
        let stereo = ChannelLayout::STEREO;
        let mut id = MainOutputId::GENESIS;
        for _ in 0..crate::MAX_PENDING_MAIN_OUTPUTS {
            id = id.next();
            control.publish_main_output(id, None, stereo).unwrap();
        }
        assert_eq!(
            control.publish_main_output(id.next(), None, stereo),
            Err(tutti_graph::MainOutputError::TooManyPending {
                limit: crate::MAX_PENDING_MAIN_OUTPUTS
            })
        );
        assert!(
            !control.has_fold(id.next(), 2, stereo),
            "nothing was published"
        );
        // The newest pending main output is committed and rendered: the older ones
        // are retired and the room is back.
        ed.spec_mut().main_output_id = id;
        ed.commit().unwrap();
        let _ = first_frame(&engine, 2);
        let _ = control.reclaim();
        control
            .publish_main_output(id.next(), None, stereo)
            .unwrap();
    }

    /// An idle `reclaim` (no main output to retire) neither clones nor republishes
    /// the fold table: the set the callback lends is the same allocation.
    #[test]
    fn an_idle_reclaim_leaves_the_published_set_alone() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let control = engine.output_control();
        let before = control.cell().current();
        for _ in 0..4 {
            let _ = control.reclaim();
        }
        assert!(Arc::ptr_eq(&before, &control.cell().current()));
    }

    /// An editor attached to an engine refuses a commit whose main output id no fold
    /// was published for, by name: the plan is not sent and the last one goes
    /// on sounding.
    #[test]
    fn a_commit_of_an_unpublished_main_output_is_refused() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let _ = first_frame(&engine, 2);
        let unheard = MainOutputId::GENESIS.next();
        ed.spec_mut().main_output_id = unheard;
        assert_eq!(
            ed.commit(),
            Err(CommitError::MainOutputNotPublished {
                main_output: unheard,
                outputs: 2
            })
        );
        assert_eq!(first_frame(&engine, 2), [21.0, 22.0]);
        engine
            .output_control()
            .publish_main_output(unheard, None, ChannelLayout::STEREO)
            .unwrap();
        ed.commit().expect("published, so it commits");
    }

    /// The monitor format folds the main output into what is heard, and the map places
    /// the heard channels on the device: a 5.1 main output heard as stereo on outputs 3
    /// and 4 of a six-output device is the ITU stereo fold on those two outputs
    /// and silence elsewhere, at the analytic figures.
    #[test]
    fn a_monitor_and_a_map_compose_into_the_one_fold() {
        use crate::{OutputMap, Routing};
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 6);
        let _ = first_frame(&engine, 6);
        let control = engine.output_control();
        let m = <f32 as Float>::MINUS_3_DB;
        assert_eq!(
            first_frame(&engine, 6),
            [61.0, 62.0, 63.0, 64.0, 65.0, 66.0],
            "no routing: the identity by speaker"
        );
        control
            .set_routing(Routing {
                monitor: Some(tutti_buffers::Bed::Stereo.topology()),
                map: OutputMap::channels([2, 3]).unwrap(),
                ..Routing::default()
            })
            .expect("the routing places the main outputs");
        let (l, r) = (61.0 + m * 63.0 + m * 65.0, 62.0 + m * 63.0 + m * 66.0);
        assert_eq!(
            first_frame_after_fade(&engine, 6),
            [0.0, 0.0, l, r, 0.0, 0.0]
        );
        // The same routing on a stereo device has no place for outputs 3 and 4:
        // no fold, so a commit or restart would name it.
        assert!(!control.has_fold(MainOutputId::GENESIS, 6, ChannelLayout::STEREO));
        // Taken back: by speaker again.
        control
            .set_routing(Routing::default())
            .expect("the routing places the main outputs");
        assert_eq!(
            first_frame_after_fade(&engine, 6),
            [61.0, 62.0, 63.0, 64.0, 65.0, 66.0]
        );
    }

    /// **A routing that cannot place the main output on the device in use is refused
    /// and installs nothing.** A map sending a heard channel to output 3 of a
    /// device of two names that channel and that device, and the routing in
    /// force still folds the next block.
    #[test]
    fn a_routing_that_cannot_place_the_main_output_on_the_device_in_use_is_refused() {
        use crate::{OutputMap, Routing, RoutingError};
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let control = engine.output_control();
        let past = || Routing {
            map: OutputMap::channels([0, 2]).unwrap(),
            ..Routing::default()
        };
        let before = first_frame(&engine, 2);
        assert_eq!(
            control.set_routing(past()),
            Err(RoutingError::MapPast {
                output: 2,
                device: ChannelLayout::STEREO
            })
        );
        assert_eq!(
            control.routing(),
            Routing::default(),
            "nothing is installed"
        );
        assert_eq!(first_frame(&engine, 2), before, "the block folds as before");
    }

    /// **A routing is judged against the device the stream renders into now.**
    /// After a block at 2 channels, stating the 8-channel device moves the
    /// device in use, so a map to output 5 is taken instead of refused against
    /// the 2-channel device.
    #[test]
    fn a_device_statement_forgets_the_device_the_last_block_rendered_into() {
        use crate::{OutputMap, Routing};
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let _ = first_frame(&engine, 2);
        let control = engine.output_control();
        let to_five = Routing {
            map: OutputMap::channels([0, 5]).unwrap(),
            ..Routing::default()
        };
        assert!(control.set_routing(to_five.clone()).is_err(), "device 2");
        control
            .state_device(ChannelLayout::from(8u16), None)
            .unwrap();
        assert_eq!(control.set_routing(to_five), Ok(()));
    }

    /// Before a block has rendered there is no device in use, so a routing the
    /// narrowest device could not take is installed.
    #[test]
    fn a_routing_is_taken_before_any_block_has_rendered() {
        use crate::{OutputMap, Routing};
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let routing = Routing {
            map: OutputMap::channels([0, 5]).unwrap(),
            ..Routing::default()
        };
        let control = engine.output_control();
        assert_eq!(control.set_routing(routing.clone()), Ok(()));
        assert_eq!(control.routing(), routing);
    }

    /// The monitor fold takes the policy's levels: the centre and the surrounds
    /// at their own gains instead of -3 dB.
    #[test]
    fn the_monitor_fold_takes_the_levels() {
        use crate::Routing;
        use tutti_buffers::FoldLevels;
        use tutti_units::Amplitude;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 6);
        let _ = first_frame(&engine, 2);
        engine
            .output_control()
            .set_routing(Routing {
                monitor: Some(tutti_buffers::Bed::Stereo.topology()),
                levels: FoldLevels {
                    centre: Amplitude::new(0.5),
                    surround: Amplitude::new(0.25),
                    ..FoldLevels::default()
                },
                ..Routing::default()
            })
            .expect("the routing places the main outputs");
        // Channels 61..66: L = 61 + 0.5 * 63 + 0.25 * 65, R likewise.
        assert_eq!(
            first_frame_after_fade(&engine, 2),
            [
                61.0 + 0.5 * 63.0 + 0.25 * 65.0,
                62.0 + 0.5 * 63.0 + 0.25 * 66.0
            ]
        );
    }

    /// A gain change ramps over the declick length, linearly, across segments,
    /// and swaps no fold: the published fold set is the same allocation after
    /// it. Frame `k` of the ramp is `from + (to - from) * (k + 1) / n`, and it
    /// ends exactly at the target.
    #[test]
    fn an_output_gain_ramps_over_the_declick_and_swaps_no_fold() {
        use crate::{OutputLevels, OutputStrip};
        use tutti_units::Amplitude;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let _ = first_frame(&engine, 2);
        let control = engine.output_control();
        let folds = control.cell().current();
        let mut levels = OutputLevels::new();
        levels
            .set_strip(
                0,
                OutputStrip {
                    gain: Amplitude::new(0.5),
                    ..OutputStrip::default()
                },
            )
            .unwrap();
        control.set_levels(&levels);
        assert!(
            Arc::ptr_eq(&folds, &control.cell().current()),
            "no fold was swapped"
        );
        let n = declick_frames(SampleRate::SR_48K).get();
        let frames = n + 64;
        let mut out = vec![0.0f32; frames * 2];
        engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        for k in 0..frames {
            // The ramp renders in `f32` as `from + (to - from) * (k + 1) / n`: one
            // rounding each for the ratio, the product and the sum, plus the ends'
            // narrowing, at most 4 ulp of the value (the gain stays within 0.5..=1,
            // so the value is the scale).
            let gain = if k + 1 >= n {
                0.5
            } else {
                1.0 + (0.5 - 1.0) * ((k + 1) as f64 / n as f64)
            };
            let want = 21.0 * gain;
            assert!(
                (f64::from(out[2 * k]) - want).abs() <= 4.0 * f64::from(f32::EPSILON) * want.abs(),
                "left, frame {k}: {} against {want}",
                out[2 * k]
            );
            assert_eq!(out[2 * k + 1], 22.0, "right is untouched, frame {k}");
        }
        // Muted, it ramps down from where it stands, and the dim scales the rest.
        levels
            .set_strip(
                0,
                OutputStrip {
                    mute: true,
                    ..OutputStrip::default()
                },
            )
            .unwrap();
        control.set_levels(&levels.with_dim(Some(Amplitude::new(0.5))).unwrap());
        let mut out = vec![0.0f32; frames * 2];
        engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        assert_eq!(out[2 * (frames - 1)], 0.0);
        assert_eq!(out[2 * (frames - 1) + 1], 22.0 * 0.5);
        assert!(out[0] > 0.0 && out[0] < 21.0 * 0.5, "mid-ramp: {}", out[0]);
    }

    /// A main output with no fold into the device (five buses into three) is
    /// reported by `has_fold` before any block runs, so the session can
    /// refuse the commit that would make it; a width a declared main output does not
    /// have is not one it renders.
    #[test]
    fn has_fold_names_the_pairs_a_commit_would_make() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let control = engine.output_control();
        let three = ChannelLayout::from(3u16);
        let genesis = MainOutputId::GENESIS;
        assert!(control.has_fold(genesis, 3, three));
        assert!(
            control.has_fold(genesis, 2, three),
            "stereo into 3: zero-filled"
        );
        assert!(!control.has_fold(genesis, 5, three), "5 into 3 drops two");
        let id = genesis.next();
        control
            .publish_main_output(
                id,
                Some(ChannelTopology::unassigned(ChannelLayout::from(2u16))),
                three,
            )
            .unwrap();
        assert!(control.has_fold(id, 2, three));
        assert!(
            !control.has_fold(id, 3, three),
            "the main output is declared 2 wide"
        );
        assert!(control.has_width(id, 2) && !control.has_width(id, 3));
        assert!(!control.has_fold(id.next(), 2, three), "not published");
    }

    /// A device whose wiring is stated is folded into by speaker, and is
    /// reported `Stated`; the width's default layout is `Default`. A stereo main output into
    /// a device stated right then left lands its left on the device's second
    /// channel, and taking the statement back is the identity again.
    #[test]
    fn a_stated_device_is_folded_into_by_its_speakers() {
        use tutti_buffers::Speaker::*;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let control = engine.output_control();
        let _ = first_frame(&engine, 2);
        // Default stereo; a 2-wide main output passes through.
        let stereo = ChannelLayout::STEREO;
        assert_eq!(
            control.device(stereo).map(|d| d.placement),
            Some(Placement::Default)
        );
        assert_eq!(first_frame(&engine, 2), [21.0, 22.0]);
        // A 2-wide device stated as a left-right swap of the front pair: the
        // main output's left lands on the device's channel 1.
        let swapped = ChannelTopology::new([FrontRight, FrontLeft]).unwrap();
        control.state_device(stereo, Some(swapped.clone())).unwrap();
        let placed = control.device(stereo).unwrap();
        assert_eq!(placed.placement, Placement::Stated);
        assert_eq!(placed.topology, swapped);
        assert_eq!(first_frame_after_fade(&engine, 2), [22.0, 21.0]);
        // Taken back: the width's order.
        control.state_device(stereo, None).unwrap();
        assert_eq!(
            control.device(stereo).map(|d| d.placement),
            Some(Placement::Default)
        );
        assert_eq!(first_frame_after_fade(&engine, 2), [21.0, 22.0]);
        // A statement of another width than the device is refused.
        assert!(matches!(
            control.state_device(ChannelLayout::from(6u16), Some(swapped)),
            Err(FoldError::Buffer(
                tutti_buffers::BufferError::WidthMismatch { .. }
            ))
        ));
    }

    /// A main output generation's folds are published until a plan of a later main output
    /// has rendered: `reclaim` keeps the older main output while its plan is the one
    /// rendering, drops it after the new plan's first block, and frees what the
    /// replaced set held once no block holds it.
    #[test]
    fn the_old_main_outputs_folds_go_at_the_first_reclaim_after_the_new_plan_renders() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let control = engine.output_control();
        let stereo = ChannelLayout::STEREO;
        let _ = first_frame(&engine, 2);
        let old = ed.spec().main_output_id;
        let new = declare(&mut ed, &control, None, stereo).unwrap();
        // Committed and published, not yet rendered: both main outputs are held.
        let _ = control.reclaim();
        assert!(control.has_fold(old, 2, stereo) && control.has_fold(new, 2, stereo));
        // The new plan's first block.
        let _ = first_frame(&engine, 2);
        let _ = control.reclaim();
        assert!(
            !control.has_fold(old, 2, stereo),
            "the old main output is gone"
        );
        assert!(control.has_fold(new, 2, stereo));
        assert_eq!(control.reclaim(), 0, "nothing is left pinned");
    }

    /// One channel of constant 1.0.
    struct Dc;

    impl tutti_graph::Node for Dc {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
                .tail(tutti_units::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            io.output(0).fill(1.0);
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// Left: the tempo of the map the node is lent; right: the tempo of the
    /// block's transport.
    struct TempoProbe;

    impl tutti_graph::Node for TempoProbe {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::source_unassigned(ChannelLayout::STEREO)
                .tail(tutti_units::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            cx: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            let lent = cx.tempo.map_or(-1.0, |m| {
                m.tempo_at(tutti_units::Beat::new(0.0)).get() as f32
            });
            io.output(0).fill(lent);
            io.output(1).fill(cx.env.transport.tempo.get() as f32);
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// **The map a node is lent is the map its block's transport came from.**
    /// A tempo published between the block's walk and its render (90 after 120)
    /// reaches neither: the block's nodes read 120 through `Cx::tempo` and in
    /// `Env::transport`, and the next block reads 90 through both.
    #[test]
    fn a_tempo_published_mid_block_reaches_the_next_block_through_both_the_map_and_the_transport() {
        use tutti_graph::{NodeKey, OutputPort, PortSource};
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(8)),
        ));
        ed.insert(NodeKey(1), "probe", tutti_graph::Unforkable(TempoProbe));
        ed.spec_mut()
            .topology
            .set_outputs((0..2).map(|c| PortSource::Node(OutputPort::new(NodeKey(1), c))));
        ed.commit().expect("commits");
        let engine = Engine::new(&transport, &mut ed, exec).expect("within limits");
        transport.settings().set_tempo(tutti_units::Bpm::of(120.0));
        let _ = block_with(&engine, 2, || {});
        let first = block_with(&engine, 2, || {
            transport.settings().set_tempo(tutti_units::Bpm::of(90.0))
        });
        assert_eq!(first, [120.0, 120.0].repeat(8), "the block's own map");
        let next = block_with(&engine, 2, || {});
        assert_eq!(next, [90.0, 90.0].repeat(8), "the published map, next");
    }

    /// Feeds the constant at key 1 to channel `channel` of a `width`-wide
    /// main output, the others silent, in the editor's spec.
    fn feed(ed: &mut Editor, width: usize, channel: usize) {
        use tutti_graph::{NodeKey, OutputPort, PortSource};
        let dc = PortSource::Node(OutputPort::new(NodeKey(1), 0));
        ed.spec_mut()
            .topology
            .set_outputs((0..width).map(|c| if c == channel { dc } else { PortSource::Zero }));
    }

    /// The smootherstep `s(x) = x³ (6x² − 15x + 10)` the equal-amplitude
    /// gains are built on: `(1 − s, s)` at `x = (k + 1) / (n + 1)`.
    fn equal_amplitude(k: usize, n: usize) -> (f32, f32) {
        let x = (k + 1) as f64 / (n + 1) as f64;
        let s = x * x * x * (6.0 * x * x - 15.0 * x + 10.0);
        ((1.0 - s) as f32, s as f32)
    }

    /// An engine of the constant at key 1 into the left of a stereo main output, the
    /// main output fade set to `fade` frames of equal amplitude, and the plan
    /// rendered once.
    fn crossfading_engine(fade: usize) -> (Engine, Editor) {
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(8)),
        ));
        ed.insert(tutti_graph::NodeKey(1), "dc", tutti_graph::Unforkable(Dc));
        feed(&mut ed, 2, 0);
        ed.commit().expect("commits");
        let engine =
            Engine::new(&Transport::new(SampleRate::SR_48K), &mut ed, exec).expect("within limits");
        ed.set_main_output_fade(Some(tutti_graph::Fade::new(Samples::new(fade))));
        assert_eq!(block_with(&engine, 2, || {}), [1.0, 0.0].repeat(8));
        (engine, ed)
    }

    /// Declares a main output of `width` channels (`None` the engine's order) with
    /// the constant on its channel `channel`, and commits.
    fn move_main_output(
        ed: &mut Editor,
        control: &OutputControl,
        main_output: Option<ChannelTopology>,
        width: usize,
        channel: usize,
    ) {
        let id = ed.spec().main_output_id.next();
        control
            .publish_main_output(id, main_output, ChannelLayout::STEREO)
            .expect("places");
        ed.spec_mut().main_output_id = id;
        feed(ed, width, channel);
        ed.commit().expect("commits");
    }

    /// An engine of a stereo `Ramp` (channels 21 and 22) into a four-output
    /// device, routed by `map` and rendered past the routing's fade.
    fn routed_engine(map: [usize; 2]) -> Engine {
        use crate::{OutputMap, Routing};
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let _ = first_frame(&engine, 4);
        engine
            .output_control()
            .set_routing(Routing {
                map: OutputMap::channels(map).unwrap(),
                ..Routing::default()
            })
            .expect("the routing places the main outputs");
        let _ = first_frame_after_fade(&engine, 4);
        engine
    }

    /// Four-output frames of `frames` frames.
    fn render_four(engine: &Engine, frames: usize) -> Vec<f32> {
        let mut out = vec![-9.0f32; frames * 4];
        engine.process_segment(
            &mut InterleavedMut::new(&mut out, ChannelLayout::from(4u16)).unwrap(),
        );
        out
    }

    fn move_map(engine: &Engine, map: [usize; 2]) {
        use crate::{OutputMap, Routing};
        engine
            .output_control()
            .set_routing(Routing {
                map: OutputMap::channels(map).unwrap(),
                ..Routing::default()
            })
            .expect("the routing places the main outputs");
    }

    /// A map change from outputs 0 and 1 to 2 and 3 ramps: frame `k` of the
    /// declick-long fade has the old outputs at the fade law's old-side gain
    /// and the new at its new-side gain (from a channel of 21 and one of 22),
    /// across block boundaries, and from the fade's length on only the new
    /// outputs sound, exactly.
    #[test]
    fn a_routing_change_ramps_the_old_outputs_out_and_the_new_in_by_the_fade_law() {
        let engine = routed_engine([0, 1]);
        move_map(&engine, [2, 3]);
        let n = declick_frames(SampleRate::SR_48K).get();
        assert!(n > 64, "the fade spans blocks");
        let out = render_four(&engine, n + 70);
        for (k, frame) in frames_of(&out, 4).enumerate() {
            let (go, gn) = if k < n {
                FadeLaw::EqualAmplitude.gains(Samples::new(k), Samples::new(n))
            } else {
                (Amplitude::SILENT, Amplitude::UNITY)
            };
            let (go, gn) = (go.get() as f32, gn.get() as f32);
            assert_eq!(
                *frame,
                [21.0 * go, 22.0 * go, 21.0 * gn, 22.0 * gn],
                "frame {k}"
            );
        }
    }

    /// A routing change made while the last one still fades cuts: the next
    /// block is the newest routing alone, with no third table held.
    #[test]
    fn a_routing_change_during_a_routing_fade_cuts_to_the_newest() {
        let engine = routed_engine([0, 1]);
        move_map(&engine, [2, 3]);
        let _ = render_four(&engine, 64);
        move_map(&engine, [1, 0]);
        let out = render_four(&engine, 16);
        for frame in frames_of(&out, 4) {
            assert_eq!(*frame, [22.0, 21.0, 0.0, 0.0]);
        }
        let held = engine.output_control().cell().current();
        assert!(
            held.previous().is_none(),
            "a cut keeps nothing to fade from"
        );
    }

    /// A routing change during a main output fade cuts: the next block folds both
    /// main outputs under the new routing from its first frame (the old main output is the
    /// constant on the left, the new on the right, heard swapped), and the
    /// routing does not fade in after the main output fade ends.
    #[test]
    fn a_routing_change_during_a_main_output_fade_cuts() {
        use crate::{OutputMap, Routing};
        let (engine, mut ed) = crossfading_engine(20);
        let control = engine.output_control();
        move_main_output(&mut ed, &control, None, 2, 1);
        let mut heard = block_with(&engine, 2, || {});
        control
            .set_routing(Routing {
                map: OutputMap::channels([1, 0]).unwrap(),
                ..Routing::default()
            })
            .expect("the routing places the main outputs");
        heard.extend((0..2).flat_map(|_| block_with(&engine, 2, || {})));
        for (k, frame) in frames_of(&heard, 2).enumerate() {
            let (out, new) = if k < 20 {
                FadeLaw::EqualAmplitude.gains(Samples::new(k), Samples::new(20))
            } else {
                (Amplitude::SILENT, Amplitude::UNITY)
            };
            let (out, new) = (out.get() as f32, new.get() as f32);
            // Heard left goes to output 1 and heard right to output 0, from
            // the block after the change.
            let want = if k < 8 { [out, new] } else { [new, out] };
            assert!(
                (frame[0] - want[0]).abs() < 1e-6 && (frame[1] - want[1]).abs() < 1e-6,
                "frame {k}: {frame:?} against {want:?}"
            );
        }
        let _ = control.reclaim();
        assert!(
            control.cell().current().previous().is_none(),
            "the cut table is not kept"
        );
    }

    /// The table a routing change replaced is held while the fade runs, goes
    /// at the first reclaim after the audio thread reports the fade ended, and
    /// is freed (nothing keeps it) once no block lends the table that held it.
    #[test]
    fn the_replaced_routing_table_is_reclaimed_after_the_fade() {
        let engine = routed_engine([0, 1]);
        let control = engine.output_control();
        move_map(&engine, [2, 3]);
        let previous = {
            let table = control.cell().current();
            let held = table.previous().expect("the replaced table is held");
            assert_eq!(Arc::strong_count(held), 1, "the control side's alone");
            Arc::downgrade(held)
        };
        let n = declick_frames(SampleRate::SR_48K).get();
        let _ = render_four(&engine, n / 2);
        let _ = control.reclaim();
        assert!(
            control.cell().current().previous().is_some(),
            "held while the fade runs"
        );
        let _ = render_four(&engine, n);
        let _ = control.reclaim();
        assert!(control.cell().current().previous().is_none());
        let _ = render_four(&engine, 8);
        assert_eq!(control.reclaim(), 0, "nothing is left pinned");
        assert!(previous.upgrade().is_none(), "the replaced table is freed");
    }

    /// A routing change's replaced table is reclaimed even when no fold plays:
    /// a block with no plan (width 0) and one whose main output has no placed fold
    /// both end the fade, so `previous` is not held while the stream is silent.
    #[test]
    fn the_replaced_routing_table_is_reclaimed_while_no_fold_plays() {
        use crate::{OutputMap, Routing};
        let change = || Routing {
            map: OutputMap::channels([1, 0]).unwrap(),
            ..Routing::default()
        };
        // No plan: width 0.
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let control = engine.output_control();
        let _ = first_frame(&engine, 2);
        control
            .set_routing(change())
            .expect("the routing places the main outputs");
        assert!(control.cell().current().previous().is_some());
        let _ = first_frame(&engine, 2);
        let _ = control.reclaim();
        assert!(control.cell().current().previous().is_none(), "width 0");
        // Five buses into three outputs have no fold, and a routing that
        // cannot place them is refused; a device statement fades all the same.
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 5);
        let control = engine.output_control();
        let _ = first_frame(&engine, 3);
        assert!(!control.has_fold(MainOutputId::GENESIS, 5, ChannelLayout::from(3u16)));
        assert!(control.set_routing(change()).is_err());
        let swapped = ChannelTopology::new([
            tutti_buffers::Speaker::FrontRight,
            tutti_buffers::Speaker::FrontLeft,
            tutti_buffers::Speaker::FrontCenter,
        ])
        .unwrap();
        control
            .state_device(ChannelLayout::from(3u16), Some(swapped))
            .unwrap();
        assert!(!control.has_fold(MainOutputId::GENESIS, 5, ChannelLayout::from(3u16)));
        assert!(control.cell().current().previous().is_some());
        let _ = first_frame(&engine, 3);
        let _ = control.reclaim();
        assert!(control.cell().current().previous().is_none(), "no fold");
    }

    /// A publish that is not a routing change (a device added mid-fade) neither
    /// restarts nor extends the fade: the frames follow the law from where they
    /// were and end at the declick length.
    #[test]
    fn an_unrelated_publish_mid_fade_leaves_the_fade_where_it_was() {
        let engine = routed_engine([0, 1]);
        move_map(&engine, [2, 3]);
        let n = declick_frames(SampleRate::SR_48K).get();
        let mut out = render_four(&engine, 100);
        engine
            .support_output(ChannelLayout::from(20u16))
            .expect("a width");
        out.extend(render_four(&engine, n + 70 - 100));
        for (k, frame) in frames_of(&out, 4).enumerate() {
            let (go, gn) = if k < n {
                FadeLaw::EqualAmplitude.gains(Samples::new(k), Samples::new(n))
            } else {
                (Amplitude::SILENT, Amplitude::UNITY)
            };
            let (go, gn) = (go.get() as f32, gn.get() as f32);
            assert_eq!(
                *frame,
                [21.0 * go, 22.0 * go, 21.0 * gn, 22.0 * gn],
                "frame {k}"
            );
        }
    }

    /// A device statement is a routing change as well: swapping the stereo
    /// device's front pair fades, so the first frame after it is still the
    /// old order's and the swap is whole at the fade's length.
    #[test]
    fn a_device_statement_ramps_like_a_routing_change() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 2);
        let _ = first_frame(&engine, 2);
        let swapped = ChannelTopology::new([
            tutti_buffers::Speaker::FrontRight,
            tutti_buffers::Speaker::FrontLeft,
        ])
        .unwrap();
        engine
            .output_control()
            .state_device(ChannelLayout::STEREO, Some(swapped))
            .unwrap();
        let n = declick_frames(SampleRate::SR_48K).get();
        let mut out = vec![0.0f32; (n + 8) * 2];
        engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        for (k, frame) in frames_of(&out, 2).enumerate() {
            let (go, gn) = if k < n {
                FadeLaw::EqualAmplitude.gains(Samples::new(k), Samples::new(n))
            } else {
                (Amplitude::SILENT, Amplitude::UNITY)
            };
            let (go, gn) = (go.get() as f32, gn.get() as f32);
            assert_eq!(
                *frame,
                [21.0 * go + 22.0 * gn, 22.0 * go + 21.0 * gn],
                "frame {k}"
            );
        }
    }

    /// A main output change crossfades: the old main output's lanes ramp out by the
    /// equal-amplitude gains while the new main output's ramp in, frame by frame
    /// over four 8-frame blocks, each main output folded by its own fold. The old
    /// main output is the constant on the left, the new the same constant on the
    /// right, so each frame of the left is the outgoing gain and of the right
    /// the incoming one; past the fade's 20 frames the right is the whole mix.
    #[test]
    fn a_main_output_change_crossfades_the_old_main_output_out_and_the_new_in() {
        let (engine, mut ed) = crossfading_engine(20);
        let control = engine.output_control();
        move_main_output(&mut ed, &control, None, 2, 1);
        let heard: Vec<f32> = (0..4).flat_map(|_| block_with(&engine, 2, || {})).collect();
        for (k, frame) in frames_of(&heard, 2).enumerate() {
            let (out, new) = if k < 20 {
                equal_amplitude(k, 20)
            } else {
                (0.0, 1.0)
            };
            assert!(
                (frame[0] - out).abs() < 1e-6 && (frame[1] - new).abs() < 1e-6,
                "frame {k}: {frame:?} against ({out}, {new})"
            );
        }
    }

    /// The two main outputs are folded each by its own fold: a stereo main output fading
    /// out to a 5.1 main output whose constant is on the centre, into a stereo
    /// device. The centre folds to both sides at -3 dB (BS.775), so the left
    /// is `out + new / sqrt(2)` and the right `new / sqrt(2)`.
    #[test]
    fn each_main_output_is_folded_by_its_own_fold_through_the_crossfade() {
        let (engine, mut ed) = crossfading_engine(20);
        let control = engine.output_control();
        move_main_output(&mut ed, &control, Some(Bed::Surround51.topology()), 6, 2);
        let heard: Vec<f32> = (0..3).flat_map(|_| block_with(&engine, 2, || {})).collect();
        let centre = std::f32::consts::FRAC_1_SQRT_2;
        for (k, frame) in frames_of(&heard, 2).enumerate().take(24) {
            let (out, new) = if k < 20 {
                equal_amplitude(k, 20)
            } else {
                (0.0, 1.0)
            };
            let (left, right) = (out + new * centre, new * centre);
            assert!(
                (frame[0] - left).abs() < 1e-6 && (frame[1] - right).abs() < 1e-6,
                "frame {k}: {frame:?} against ({left}, {right})"
            );
        }
    }

    /// A main output change with no main output fade is a cut: the first frame of the block
    /// the commit lands on is the new main output alone.
    #[test]
    fn a_main_output_change_without_a_fade_cuts_at_the_block() {
        let (engine, mut ed) = crossfading_engine(20);
        ed.set_main_output_fade(None);
        let control = engine.output_control();
        move_main_output(&mut ed, &control, None, 2, 1);
        assert_eq!(block_with(&engine, 2, || {}), [0.0, 1.0].repeat(8));
        assert!(
            ed.base().is_some_and(|p| p.main_output_bank().is_none()),
            "the plan carries no old main output"
        );
    }

    /// The old main output's folds stay published while the fade runs and go at the
    /// first reclaim after the block that ends it, not after the commit's
    /// first block; the plan is then compiled again without the old channels.
    #[test]
    fn the_old_main_outputs_folds_go_at_the_first_reclaim_after_the_crossfade_ends() {
        let (engine, mut ed) = crossfading_engine(20);
        let control = engine.output_control();
        let stereo = ChannelLayout::STEREO;
        let old = ed.spec().main_output_id;
        move_main_output(&mut ed, &control, None, 2, 1);
        let new = ed.spec().main_output_id;
        // Frames 0..8, 8..16 and 16..24: the third ends the 20-frame fade.
        for block in 0..2 {
            let _ = block_with(&engine, 2, || {});
            let _ = control.reclaim();
            assert!(
                control.has_fold(old, 2, stereo),
                "the old main output is folded until the fade ends (block {block})"
            );
        }
        let _ = block_with(&engine, 2, || {});
        let _ = control.reclaim();
        assert!(
            !control.has_fold(old, 2, stereo),
            "the old main output is gone"
        );
        assert!(control.has_fold(new, 2, stereo));
        assert_eq!(control.reclaim(), 0, "nothing is left pinned");
        // The editor learns the fade is over and compiles the old channels away.
        let _ = ed.collect();
        assert!(ed.finish_fades(), "the fade's end is reported once");
        assert!(ed.base().is_some_and(|p| p.main_output_bank().is_some()));
        ed.commit().expect("commits");
        assert!(ed.base().is_some_and(|p| p.main_output_bank().is_none()));
        assert_eq!(block_with(&engine, 2, || {}), [0.0, 1.0].repeat(8));
        assert!(!ed.finish_fades(), "and once only");
    }

    /// A commit that lands while the fade runs, and leaves the main output alone,
    /// continues it: the rewire to both channels brings the right in by the
    /// fade's gains from frame 8 on, not at once, while the left stays the sum
    /// of the old main output going out and the new coming in, which is one.
    #[test]
    fn a_commit_that_lands_mid_fade_continues_it() {
        let (engine, mut ed) = crossfading_engine(20);
        let control = engine.output_control();
        move_main_output(&mut ed, &control, None, 2, 1);
        let mut heard: Vec<f32> = block_with(&engine, 2, || {});
        let dc =
            tutti_graph::PortSource::Node(tutti_graph::OutputPort::new(tutti_graph::NodeKey(1), 0));
        ed.spec_mut().topology.set_outputs([dc, dc]);
        ed.commit().expect("commits");
        heard.extend((0..3).flat_map(|_| block_with(&engine, 2, || {})));
        for (k, frame) in frames_of(&heard, 2).enumerate() {
            let (out, new) = if k < 20 {
                equal_amplitude(k, 20)
            } else {
                (0.0, 1.0)
            };
            // The old main output is the constant on the left; the new main output, from
            // frame 8, the constant on both sides.
            let (left, right) = if k < 8 { (out, new) } else { (out + new, new) };
            assert!(
                (frame[0] - left).abs() < 1e-6 && (frame[1] - right).abs() < 1e-6,
                "frame {k}: {frame:?} against ({left}, {right})"
            );
        }
    }

    /// Counts 1, 2, 3 ... one per frame, on one channel.
    struct Count(f32);

    impl tutti_graph::Node for Count {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
                .tail(tutti_units::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            for x in io.output(0) {
                self.0 += 1.0;
                *x = self.0;
            }
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// Silence that reports 16 frames of latency, so the graph delays every
    /// other output by 16.
    struct Late;

    impl tutti_graph::Node for Late {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
                .latency(tutti_units::Latency::new(Samples::new(16)))
                .tail(tutti_units::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            io.output(0).fill(0.0);
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// A compensated old main output fades out with no gap: the counter on the left
    /// is delayed by 16 frames to the output time, and its history crosses
    /// into the old main output's channel of the bank, so frame `t` of the fade is
    /// the counter's value 16 frames before, times the outgoing gain, with no
    /// dip to silence. The new main output, which has the same source on its right,
    /// starts from the same history, so it has no gap either.
    #[test]
    fn a_compensated_old_main_output_crossfades_out_with_its_history() {
        use tutti_graph::{NodeKey, OutputPort, PortSource};
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(8)),
        ));
        ed.insert(NodeKey(1), "count", tutti_graph::Unforkable(Count(0.0)));
        ed.insert(NodeKey(2), "late", tutti_graph::Unforkable(Late));
        let count = PortSource::Node(OutputPort::new(NodeKey(1), 0));
        let late = PortSource::Node(OutputPort::new(NodeKey(2), 0));
        ed.spec_mut().topology.set_outputs([count, late]);
        ed.commit().expect("commits");
        let engine =
            Engine::new(&Transport::new(SampleRate::SR_48K), &mut ed, exec).expect("within limits");
        ed.set_main_output_fade(Some(tutti_graph::Fade::new(Samples::new(20))));
        let control = engine.output_control();
        // 16 frames of the old main output, whose left is the counter 16 behind.
        for _ in 0..2 {
            let _ = block_with(&engine, 2, || {});
        }
        let id = ed.spec().main_output_id.next();
        control
            .publish_main_output(id, None, ChannelLayout::STEREO)
            .unwrap();
        ed.spec_mut().main_output_id = id;
        ed.spec_mut().topology.set_outputs([late, count]);
        ed.commit().expect("commits");
        let heard: Vec<f32> = (0..3).flat_map(|_| block_with(&engine, 2, || {})).collect();
        // The counter's value at absolute frame `t` is `t + 1`; delayed by 16
        // it is `t - 15` from frame 16 on.
        let delayed = |t: usize| (t as f32 - 15.0).max(0.0);
        for (k, frame) in frames_of(&heard, 2).enumerate().take(24) {
            let t = 16 + k;
            let (out, new) = if k < 20 {
                equal_amplitude(k, 20)
            } else {
                (0.0, 1.0)
            };
            let left = out * delayed(t);
            // The counter's history crosses to the right too: the source's
            // delayed signal does not depend on the channel it feeds.
            let right = new * delayed(t);
            assert!(
                (frame[0] - left).abs() < 1e-3 && (frame[1] - right).abs() < 1e-3,
                "frame {k}: {frame:?} against ({left}, {right})"
            );
        }
    }

    /// An old main output's latent source the new main output lacks does not move the new
    /// main output: the old main output has the counter and a 16-frame latent source, the
    /// new main output the counter alone on the left. The new main output's counter is
    /// undelayed from its first frame, through the fade and after the commit
    /// that compiles the old channels away, and the plan's reported latency is
    /// the new main output's (none), not the old main output's 16.
    #[test]
    fn a_latent_source_the_new_main_output_lacks_does_not_move_the_new_main_output() {
        use tutti_graph::{NodeKey, OutputPort, PortSource};
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(8)),
        ));
        ed.insert(NodeKey(1), "count", tutti_graph::Unforkable(Count(0.0)));
        ed.insert(NodeKey(2), "late", tutti_graph::Unforkable(Late));
        let count = PortSource::Node(OutputPort::new(NodeKey(1), 0));
        let late = PortSource::Node(OutputPort::new(NodeKey(2), 0));
        ed.spec_mut().topology.set_outputs([count, late]);
        ed.commit().expect("commits");
        let engine =
            Engine::new(&Transport::new(SampleRate::SR_48K), &mut ed, exec).expect("within limits");
        ed.set_main_output_fade(Some(tutti_graph::Fade::new(Samples::new(20))));
        let control = engine.output_control();
        for _ in 0..2 {
            let _ = block_with(&engine, 2, || {});
        }
        assert_eq!(
            ed.base().map(|p| p.total_latency().samples()),
            Some(Samples::new(16))
        );
        let id = ed.spec().main_output_id.next();
        control
            .publish_main_output(id, None, ChannelLayout::STEREO)
            .unwrap();
        ed.spec_mut().main_output_id = id;
        ed.spec_mut()
            .topology
            .set_outputs([count, PortSource::Zero]);
        ed.commit().expect("commits");
        assert_eq!(
            ed.base().map(|p| p.total_latency().samples()),
            Some(Samples::new(0)),
            "the reported latency is the new main output's"
        );
        let mut heard: Vec<f32> = (0..3).flat_map(|_| block_with(&engine, 2, || {})).collect();
        // The fade is over: compile the old channels away and keep rendering.
        let _ = ed.collect();
        assert!(ed.finish_fades());
        ed.commit().expect("commits");
        heard.extend((0..2).flat_map(|_| block_with(&engine, 2, || {})));
        // The counter at absolute frame `t` is `t + 1`; the old main output's copy is
        // 16 behind from frame 16 on.
        for (k, frame) in frames_of(&heard, 2).enumerate() {
            let t = 16 + k;
            let (out, new) = if k < 20 {
                equal_amplitude(k, 20)
            } else {
                (0.0, 1.0)
            };
            let left = out * (t as f32 - 15.0) + new * (t as f32 + 1.0);
            assert!(
                (frame[0] - left).abs() < 1e-3 && frame[1].abs() < 1e-6,
                "frame {k}: {frame:?} against ({left}, 0)"
            );
        }
    }

    /// A fold set a block holds when it is replaced stays pinned until the
    /// block ends, and `OutputControl::reclaim` frees it afterwards.
    #[test]
    fn a_replaced_fold_set_is_reclaimed_after_the_block() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        let control = engine.output_control();
        let loan_holder = &engine.folds;
        let mut reader = loan_holder.borrow_mut();
        let loan = reader.lend();
        control
            .publish_main_output(
                ed.spec().main_output_id.next(),
                Some(ChannelTopology::unassigned(ChannelLayout::QUAD)),
                ChannelLayout::from(6u16),
            )
            .unwrap();
        assert_eq!(control.reclaim(), 1, "the set the block holds is pinned");
        drop(loan);
        assert_eq!(control.reclaim(), 0);
    }

    /// A declaration the device cannot take is refused and publishes nothing:
    /// four buses into a stereo device would drop two, and a channel no
    /// speaker names has no place anywhere.
    #[test]
    fn an_unplaceable_declaration_is_refused() {
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let engine = Engine::new(&transport, &mut ed, exec).expect("empty graph");
        rewire(&mut ed, 4);
        let _ = first_frame(&engine, 2);
        let before = first_frame(&engine, 2);
        let control = engine.output_control();
        let next = ed.spec().main_output_id.next();
        assert_eq!(
            control.publish_main_output(
                next,
                Some(ChannelTopology::unassigned(ChannelLayout::QUAD)),
                ChannelLayout::STEREO
            ),
            Err(tutti_graph::MainOutputError::Fold(FoldError::Unplaced {
                channel: 2,
                speaker: tutti_buffers::Speaker::Unassigned
            }))
        );
        let lfe2 = ChannelTopology::new([
            tutti_buffers::Speaker::FrontLeft,
            tutti_buffers::Speaker::FrontRight,
            tutti_buffers::Speaker::Unknown(9),
            tutti_buffers::Speaker::FrontCenter,
        ])
        .unwrap();
        assert!(matches!(
            control.publish_main_output(next, Some(lfe2), ChannelLayout::STEREO),
            Err(tutti_graph::MainOutputError::Fold(
                FoldError::Unnamed { .. }
            ))
        ));
        assert!(!control.has_fold(next, 4, ChannelLayout::STEREO));
        assert_eq!(first_frame(&engine, 2), before);
    }

    /// A commit that lands between `settle` and `render` waits for the next
    /// block: this block renders the plan `settle` left, at its width, and
    /// the next one the new plan. Deterministic: the gap is staged, not
    /// timed. From no plan, then widening.
    #[test]
    fn a_commit_between_settle_and_render_waits_for_the_next_block() {
        use std::cell::RefCell;
        let transport = Transport::new(SampleRate::SR_48K);
        let (ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("empty graph");
        let want = |w: usize| -> Vec<f32> {
            (0..8)
                .flat_map(|_| (0..w).map(|c| (10 * w + c + 1) as f32))
                .collect()
        };
        // No plan at `settle`: this block is silent.
        let got = block_with(&engine, 2, || rewire(&mut ed.borrow_mut(), 2));
        assert_eq!(got, vec![0.0; 16]);
        // The next one has it.
        assert_eq!(block_with(&engine, 2, || {}), want(2));
        // Widening to 4 in the gap: this block is still 2 wide.
        assert_eq!(
            block_with(&engine, 2, || rewire(&mut ed.borrow_mut(), 4)),
            want(2)
        );
        assert_eq!(block_with(&engine, 4, || {}), want(4));
    }

    /// A narrowing commit in the gap: this block folds the wide plan whole,
    /// the next the narrow one, with no stale channel in it.
    #[test]
    fn a_narrowing_commit_between_settle_and_render_waits_for_the_next_block() {
        use std::cell::RefCell;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        rewire(&mut ed, 4);
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("within limits");
        let wide: Vec<f32> = (0..8).flat_map(|_| [41.0, 42.0, 43.0, 44.0]).collect();
        let narrow: Vec<f32> = (0..8).flat_map(|_| [21.0, 22.0]).collect();
        assert_eq!(block_with(&engine, 4, || {}), wide);
        let got = block_with(&engine, 4, || rewire(&mut ed.borrow_mut(), 2));
        assert_eq!(got, wide);
        assert_eq!(block_with(&engine, 2, || {}), narrow);
    }

    /// A re-prepare's resume landing in the gap changes neither the block
    /// bound nor the rate under the block: this block (settled at 512)
    /// renders at 512, and the next is bound at 256.
    #[test]
    fn a_resume_between_settle_and_render_waits_for_the_next_block() {
        use std::cell::RefCell;
        let transport = Transport::new(SampleRate::SR_48K);
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(512)),
        ));
        rewire(&mut ed, 2);
        let ed = RefCell::new(ed);
        let engine = Engine::new(&transport, &mut ed.borrow_mut(), exec).expect("within limits");
        ed.borrow_mut()
            .reprepare(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(256)),
            ))
            .expect("shrinks");
        // The suspend commit lands in this block's `settle`; silent.
        let _ = block_of(&engine, 2, 512, || {});
        // The resume is sent after `settle` bound the block at 512.
        let _ = block_of(&engine, 2, 512, || drop(ed.borrow_mut().collect()));
        let max = |e: &Engine| e.graph.borrow_mut().exec.prepare().max_block().get();
        assert_eq!(max(&engine), 512, "not yet applied");
        let got = block_of(&engine, 2, 256, || {});
        assert_eq!(max(&engine), 256);
        let want: Vec<f32> = (0..256).flat_map(|_| [21.0, 22.0]).collect();
        assert_eq!(got, want);
    }
}
