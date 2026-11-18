//! The render thread.
//!
//! The server runs two threads once the host has connected its audio and
//! control connections:
//!
//! - **The main thread** keeps the plugin's `Controller` and its editor, and
//!   serves the control connection and the idle tick ([`crate::main_loop`]).
//!   An editor's paint runs there, and may take as long as it takes.
//! - **The render thread** (this module) owns the plugin's `Processor` and the
//!   audio connection. It reads `ProcessAudio` and `Reset`, renders, and
//!   answers. **Nothing on that path is shared with the main thread but the
//!   queues below**: it takes no lock the main thread holds, so a paint can
//!   never delay a block.
//!
//! What crosses between them:
//!
//! - **The Processor** travels through `tutti_rt`'s handoff: the main thread lends it at an
//!   activation and recalls it before a
//!   deactivation, and this thread trades it at its next poll (between blocks,
//!   and every [`RENDER_WAIT`] while the host is quiet, so a recall is never
//!   left waiting for a block).
//! - **Commands** ([`ToRender`]) travel one way, main to render, through a
//!   `tutti_rt` ring: the slab with the scratch sized for it, the plugin's
//!   parameter output, the figures a block reads ([`RenderConfig`]) and the seat the Processor
//!   arrives in. A command carries a finished value: the main thread builds
//!   the scratch, the parameter output and the slab's room, and the render
//!   thread only swaps them in. What the render thread has finished with goes
//!   back through a second queue ([`Back`]) to be dropped on the main thread:
//!   a plugin's objects are freed on the render thread only when that queue
//!   refuses them, which only an abandoned session reaches.
//! - **Losses** ([`Losses`]) are `tutti_rt` tallies the render thread adds to
//!   and the main thread reads and logs ([`RenderLink::report_losses`]): the
//!   render thread never calls `tracing`.
//!
//! Commands are applied before the request that follows them is served, so a
//! command the main thread pushed before it answered the host is in effect for
//! every block the host sends after that answer.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tutti_rt::{StopToken, ThreadHandle};

use std::borrow::Cow;
use tutti_plugin_ipc::{BridgeMessage, HostMessage, SlabWriter};
use tutti_plugin_types::Samples;
use tutti_plugin_types::{
    BlockStatus, BridgeError, BridgeResult, Features, MidiEventVec, PluginAudio, PluginError,
    PluginResult, ProcessContext, ProcessOutput, Processor, SampleFormat,
};
use tutti_rt::{
    Overflow, PushError, Returned, RingReader, RingWriter, Rt, Tally, TallyWriter, ring, tally,
};

use crate::audio_pipeline::{AudioBlock, AudioPipeline, AudioScratch, Clock, ProcessExtras};
use crate::served::Render;
use crate::transport::Transport;

/// How long the render thread waits for a request before it looks at its
/// commands and its seat again: the longest a recall of the Processor waits
/// for a host that sends no blocks. A block's request ends the wait at once.
pub(crate) const RENDER_WAIT: Duration = Duration::from_millis(5);

/// How many commands may be queued to the render thread. A load is a handful;
/// the queue is sized so the handshake, which runs before the thread does,
/// never fills it.
const COMMANDS: usize = 64;

/// How long the main thread waits for room in a full queue before it gives up
/// on a command (a render thread that has stopped taking them).
const SEND_PATIENCE: Duration = Duration::from_secs(5);

/// What a block reads of the plugin and the session, published by the main
/// thread whenever it changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RenderConfig {
    /// The rate and sample format blocks are rendered in.
    pub clock: Clock,
    /// The longest block the plugin was activated for, 0 with none loaded.
    pub max_block: usize,
    /// Whether the plugin refused the rate the host asked for: it then renders
    /// silence and no block reaches it.
    pub rate_refused: bool,
    /// What the plugin consumes, as it reports now.
    pub features: Features,
    /// The channels the plugin takes now, over all its buses, 0 with none
    /// loaded. A block renders only into a slab laid out the same.
    pub inputs: usize,
    /// The channels the plugin gives now, over all its buses, 0 with none
    /// loaded. A block renders only into a slab laid out the same.
    pub outputs: usize,
}

impl RenderConfig {
    /// Nothing loaded.
    pub(crate) fn new() -> Self {
        Self {
            clock: Clock::default(),
            max_block: 0,
            rate_refused: false,
            features: Features::empty(),
            inputs: 0,
            outputs: 0,
        }
    }
}

/// The render thread's end of an active plugin: where its Processor arrives
/// and how a block is rendered with it.
///
/// One per loaded plugin, built with the plugin (it names the format's
/// Processor, which the session's type-erased plugin does not).
pub(crate) trait RenderEnd: Render + Send {
    /// Between blocks: trades with the main thread (takes a Processor lent to
    /// it, returns one that was recalled).
    fn poll(&mut self);
    /// Forgets the plugin's signal history, between blocks. A plugin whose
    /// Processor is not here has nothing to forget.
    fn reset(&mut self);
    /// What the plugin consumes, as it reports now.
    fn set_features(&mut self, features: Features);
    /// Whether the Processor is here: a block rendered now reaches the plugin
    /// (and drains the writes queued to it).
    fn has_processor(&mut self) -> bool;
}

/// [`RenderEnd`] over the format's Processor, in a handoff.
pub(crate) struct End<P: Processor> {
    rt: Rt<P>,
    features: Features,
}

impl<P: Processor> End<P> {
    pub(crate) fn new(rt: Rt<P>, features: Features) -> Self {
        Self { rt, features }
    }
}

impl<P: Processor + 'static> Render for End<P> {
    fn features(&self) -> Features {
        self.features
    }

    fn render<'a>(
        &mut self,
        buffer: PluginAudio<'a>,
        ctx: &'a ProcessContext<'a>,
        out: &'a mut ProcessOutput,
    ) -> PluginResult<()> {
        // A Processor that is not here (recalled for a rate change, or
        // refused): the block is silence and the plugin is not touched.
        let Some(processor) = self.rt.held() else {
            out.clear();
            let mut buffer = buffer;
            buffer.clear_outputs();
            return Ok(());
        };
        buffer.process(processor, ctx, out)
    }
}

impl<P: Processor + 'static> RenderEnd for End<P> {
    fn poll(&mut self) {
        let _ = self.rt.poll(Processor::stop);
    }

    fn reset(&mut self) {
        if let Some(processor) = self.rt.held() {
            processor.reset();
        }
    }

    fn set_features(&mut self, features: Features) {
        self.features = features;
    }

    fn has_processor(&mut self) -> bool {
        self.rt.held().is_some()
    }
}

/// What the main thread asks of the render thread. Every value a command
/// carries is finished: the render thread swaps it in and hands the value it
/// replaces back ([`Back`]).
#[allow(clippy::large_enum_variant)] // sent a few times per session, never per block
pub(crate) enum ToRender {
    /// The shared-memory slab the host set up, with the scratch sized to it.
    Slab {
        /// The slab blocks are read from and published into.
        slab: SlabWriter,
        /// Scratch for the slab's widest direction at its per-channel room, in
        /// the sample format blocks are rendered in.
        scratch: Box<AudioScratch>,
    },
    /// The slab is gone (an unload).
    DropSlab,
    /// The sample format changed: scratch of the slab's shape in the new one.
    Scratch(Box<AudioScratch>),
    /// The plugin's parameter output, held in fixed storage: a queue per
    /// parameter, each of room for a block's points.
    Output(Box<ProcessOutput>),
    /// New figures for the blocks that follow.
    Config(RenderConfig),
    /// The plugin's end: its Processor arrives in it.
    Seat(Box<dyn RenderEnd>),
    /// A write was queued to the Processor before this: the next block that
    /// reaches the plugin applies it (the Processor drains its edit ring at the
    /// head of a block, after this command was taken), and is answered with
    /// [`Back::Ack`]. See [`RenderLink::barrier`].
    Barrier(u64),
    /// Stop rendering with the seat and give it back. The Processor has been
    /// recalled by now.
    Unseat,
}

/// What the render thread hands back to be dropped on the main thread.
// The fields are never read: a value is handed back only to be dropped by the
// thread that may free it.
#[allow(dead_code)] // but `Failure`, which `RenderLink::reap` logs
pub(crate) enum Back {
    /// The seat of a plugin that is gone.
    Seat(Box<dyn RenderEnd>),
    /// The slab.
    Slab(SlabWriter),
    /// The scratch a new one replaced.
    Scratch(Box<AudioScratch>),
    /// Why the plugin failed a block: logged, and freed, by the main thread.
    Failure(PluginError),
    /// The parameter output a new one replaced.
    Output(Box<ProcessOutput>),
    /// A block that reached the plugin after the [`ToRender::Barrier`] of this
    /// number was taken.
    Ack(u64),
}

/// What the render thread lost, counted since the session began.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Losses {
    /// Blocks refused for their length (empty, or past the activated length or
    /// the slab's room), for a slab laid out unlike the plugin's ports, or for
    /// a rate the plugin refused. Each reads back as silence.
    pub refused_blocks: u64,
    /// Blocks the plugin failed. Each publishes nothing and reads as silence.
    pub failed_blocks: u64,
    /// MIDI events a plugin emitted past the reply's cap.
    pub midi_dropped: u64,
    /// Hand-backs the main thread's queue refused, so the render thread
    /// dropped them itself (an abandoned session).
    pub back_refused: u64,
}

impl Losses {
    /// Whether some count is now at least twice what `logged` held (or was
    /// zero and is not): the rule that logs a plugin failing every block a
    /// handful of times instead of on every tick.
    fn worsened_since(self, logged: Self) -> bool {
        let grew = |now: u64, then: u64| now > 0 && (then == 0 || now >= then.saturating_mul(2));
        grew(self.refused_blocks, logged.refused_blocks)
            || grew(self.failed_blocks, logged.failed_blocks)
            || grew(self.midi_dropped, logged.midi_dropped)
            || grew(self.back_refused, logged.back_refused)
    }
}

/// The reading ends of the render thread's loss tallies, held by the main
/// thread.
struct LossReaders {
    refused_blocks: Tally<u64>,
    failed_blocks: Tally<u64>,
    midi_dropped: Tally<u64>,
    back_refused: Tally<u64>,
}

/// The writing ends of the loss tallies, held by the render side.
struct LossWriters {
    refused_blocks: TallyWriter<u64>,
    midi_dropped: TallyWriter<u64>,
    back_refused: TallyWriter<u64>,
}

/// The main thread's end of the render thread's queues.
pub(crate) struct RenderLink {
    to: RingWriter<ToRender>,
    back: RingReader<Back>,
    losses: LossReaders,
    /// What [`report_losses`](Self::report_losses) last logged.
    logged: Losses,
    /// The format blocks are rendered in, which scratch is built for.
    format: SampleFormat,
    /// The widest direction of the slab, and its per-channel room, while one
    /// is attached: the shape scratch is built to.
    slab_shape: Option<SlabShape>,
    /// The parameter queues and the points each holds that the plugin's output
    /// was last built for.
    params: (usize, usize),
    /// The number of the last barrier sent, and of the last one acknowledged.
    barrier_sent: u64,
    barrier_acked: u64,
}

/// A slab's widest direction and per-channel room, kept to rebuild scratch.
#[derive(Clone, Copy)]
struct SlabShape {
    channels: usize,
    frames: Samples,
}

impl RenderLink {
    /// Attaches `slab`: the render thread takes it with scratch sized to its
    /// layout, in the format blocks are rendered in.
    pub(crate) fn attach_slab(&mut self, slab: SlabWriter) {
        let layout = slab.layout_ref();
        self.slab_shape = Some(SlabShape {
            channels: layout.input_channels().max(layout.output_channels()),
            frames: Samples(layout.samples_per_channel),
        });
        let scratch = Box::new(AudioScratch::for_layout(self.format, layout));
        self.send(ToRender::Slab { slab, scratch });
    }

    /// Detaches the slab (an unload).
    pub(crate) fn detach_slab(&mut self) {
        self.slab_shape = None;
        self.send(ToRender::DropSlab);
    }

    /// Renders in `format` from the next block on: scratch of the slab's shape
    /// is rebuilt when it changed, and later slabs are sized in it.
    pub(crate) fn set_format(&mut self, format: SampleFormat) {
        if format == self.format {
            return;
        }
        self.format = format;
        if let Some(SlabShape { channels, frames }) = self.slab_shape {
            self.send(ToRender::Scratch(Box::new(AudioScratch::new(
                format, channels, frames,
            ))));
        }
    }

    /// Holds the plugin's parameter output in fixed storage: a queue per
    /// parameter, each with room for `points` points.
    pub(crate) fn reserve_params(&mut self, queues: usize, points: usize) {
        self.params = (queues, points);
        let mut output = ProcessOutput::default();
        output.reserve_params(queues, points);
        self.send(ToRender::Output(Box::new(output)));
    }

    /// Re-reserves the plugin's parameter output for `queues` parameters, at
    /// the room it was reserved with, when it holds fewer queues: a plugin
    /// whose parameter list grew after load.
    pub(crate) fn fit_params(&mut self, queues: usize) {
        let (held, points) = self.params;
        if queues > held {
            self.reserve_params(queues, points);
        }
    }

    /// What the render thread has lost so far.
    pub(crate) fn losses(&self) -> Losses {
        Losses {
            refused_blocks: self.losses.refused_blocks.get(),
            failed_blocks: self.losses.failed_blocks.get(),
            midi_dropped: self.losses.midi_dropped.get(),
            back_refused: self.losses.back_refused.get(),
        }
    }

    /// Logs what the render thread has lost, when a count has first become
    /// nonzero or doubled since it was last logged ([`Losses::worsened_since`]):
    /// the render thread counts and never logs.
    pub(crate) fn report_losses(&mut self) {
        let now = self.losses();
        if now.worsened_since(self.logged) {
            self.logged = now;
            tracing::warn!(
                "plugin-server: the render thread lost {} block(s) refused, {} block(s) the plugin \
                 failed, {} MIDI event(s) past the cap, {} hand-back(s) the main thread did not take",
                now.refused_blocks,
                now.failed_blocks,
                now.midi_dropped,
                now.back_refused,
            );
        }
    }

    /// Queues `command`, waiting a little for room if the queue is full.
    /// A command the render thread does not take within [`SEND_PATIENCE`] is
    /// dropped and logged: the render thread has stopped.
    pub(crate) fn send(&mut self, command: ToRender) {
        self.reap();
        let mut command = command;
        let giving_up = Instant::now() + SEND_PATIENCE;
        loop {
            match self.to.push(command) {
                Ok(()) => return,
                Err(Returned {
                    error: PushError::Disconnected,
                    ..
                }) => {
                    tracing::error!("plugin-server: the render thread is gone");
                    return;
                }
                Err(Returned { value, .. }) => command = value,
            }
            if Instant::now() >= giving_up {
                tracing::error!("plugin-server: the render thread takes no commands");
                return;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Drops what the render thread handed back, noting the barriers it
    /// acknowledged.
    pub(crate) fn reap(&mut self) {
        while let Some(back) = self.back.pop() {
            match back {
                Back::Ack(id) => self.barrier_acked = self.barrier_acked.max(id),
                Back::Failure(error) => tracing::warn!("the plugin failed a block: {error}"),
                _ => {}
            }
        }
    }

    /// Asks to be told when a block that reached the plugin has run **after**
    /// the writes queued to its Processor so far; the number to
    /// [`wait_applied`](Self::wait_applied) for.
    ///
    /// The order is the queue's. The writes were pushed (release) before this
    /// command was, the render thread takes the command (acquire) before it
    /// serves the next block, and the Processor drains its edit ring at the head
    /// of that block, so the block sees every write that preceded the barrier.
    /// The acknowledgement comes back through [`Back`] after that block: no
    /// block that began before the command was taken can answer it. No atomic
    /// of its own: the two queues carry the order.
    pub(crate) fn barrier(&mut self) -> u64 {
        self.barrier_sent += 1;
        let id = self.barrier_sent;
        self.send(ToRender::Barrier(id));
        id
    }

    /// Whether the barrier `id` has been acknowledged, waiting up to `within`
    /// for the next block to do it. `false` when no block reached the plugin
    /// in that time (the host is quiet).
    pub(crate) fn wait_applied(&mut self, id: u64, within: Duration) -> bool {
        let giving_up = Instant::now() + within;
        loop {
            self.reap();
            if self.barrier_acked >= id {
                return true;
            }
            if Instant::now() >= giving_up {
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

/// The render thread's state: what a block is rendered with.
pub(crate) struct RenderSide {
    commands: RingReader<ToRender>,
    back: RingWriter<Back>,
    slab: Option<SlabWriter>,
    pipeline: AudioPipeline,
    config: RenderConfig,
    seat: Option<Box<dyn RenderEnd>>,
    /// The barrier the next block that reaches the plugin acknowledges.
    barrier: Option<u64>,
    losses: LossWriters,
    /// Blocks the plugin has failed: the cause of the first, and of each
    /// that makes a power of two, goes home ([`Back::Failure`]).
    failures: u64,
}

/// A session's two halves: the control side the main thread drives, and the
/// render side, which a thread runs ([`RenderThread::spawn`]) once the host's
/// audio connection is there.
pub(crate) fn link() -> (RenderLink, RenderSide) {
    let (to, commands) = ring::<ToRender>(COMMANDS, Overflow::Refuse);
    let (back_tx, back) = ring::<Back>(COMMANDS, Overflow::Refuse);
    let config = RenderConfig::new();
    let (refused_blocks, failed_blocks, midi_dropped, back_refused) =
        (tally(), tally(), tally(), tally());
    let writers = LossWriters {
        refused_blocks: refused_blocks.writer(),
        midi_dropped: midi_dropped.writer(),
        back_refused: back_refused.writer(),
    };
    let pipeline = AudioPipeline::new(failed_blocks.writer());
    (
        RenderLink {
            to,
            back,
            losses: LossReaders {
                refused_blocks,
                failed_blocks,
                midi_dropped,
                back_refused,
            },
            logged: Losses::default(),
            format: config.clock.format,
            slab_shape: None,
            params: (0, 0),
            barrier_sent: 0,
            barrier_acked: 0,
        },
        RenderSide {
            commands,
            back: back_tx,
            slab: None,
            pipeline,
            config,
            seat: None,
            barrier: None,
            losses: writers,
            failures: 0,
        },
    )
}

impl RenderSide {
    /// Applies what the main thread queued. Allocates and frees nothing: a
    /// command carries finished values, and what a swap replaces goes back to
    /// the main thread.
    pub(crate) fn drain(&mut self) {
        while let Some(command) = self.commands.pop() {
            match command {
                ToRender::Slab { slab, scratch } => {
                    let old_scratch = self.pipeline.install_scratch(scratch);
                    self.give_back(Back::Scratch(old_scratch));
                    if let Some(old) = self.slab.replace(slab) {
                        self.give_back(Back::Slab(old));
                    }
                }
                ToRender::DropSlab => {
                    if let Some(old) = self.slab.take() {
                        self.give_back(Back::Slab(old));
                    }
                }
                ToRender::Scratch(scratch) => {
                    let old = self.pipeline.install_scratch(scratch);
                    self.give_back(Back::Scratch(old));
                }
                ToRender::Output(output) => {
                    let old = self.pipeline.install_output(output);
                    self.give_back(Back::Output(old));
                }
                ToRender::Config(config) => {
                    self.config = config;
                    if let Some(seat) = self.seat.as_mut() {
                        seat.set_features(config.features);
                    }
                }
                ToRender::Seat(mut seat) => {
                    seat.set_features(self.config.features);
                    if let Some(old) = self.seat.replace(seat) {
                        self.give_back(Back::Seat(old));
                    }
                }
                ToRender::Barrier(id) => self.barrier = Some(id),
                ToRender::Unseat => {
                    if let Some(old) = self.seat.take() {
                        self.give_back(Back::Seat(old));
                    }
                }
            }
        }
    }

    /// Hands something back to the main thread. A full queue, or one whose
    /// main-thread end is gone, drops it here and counts it, which only an
    /// abandoned session reaches: the main thread reaps on every command it
    /// sends.
    fn give_back(&mut self, back: Back) {
        if self.back.push(back).is_err() {
            self.losses.back_refused.add(1);
        }
    }

    /// Between blocks: trades the Processor with the main thread.
    pub(crate) fn poll_seat(&mut self) {
        if let Some(seat) = self.seat.as_mut() {
            seat.poll();
        }
    }

    /// One pass of the thread: applies the commands, trades the Processor,
    /// waits up to `wait` for a request on `io` and serves it. With no `io`
    /// (the host's audio connection has gone) it only waits.
    ///
    /// # Errors
    ///
    /// The transport fails (the host went away, or sent a frame that does not
    /// decode), or the host sent a message that is not the audio connection's.
    pub(crate) fn step(
        &mut self,
        io: Option<&mut Box<dyn Transport>>,
        wait: Duration,
    ) -> BridgeResult<()> {
        self.drain();
        self.poll_seat();
        let Some(io) = io else {
            std::thread::sleep(wait);
            return Ok(());
        };
        if !io.wait_readable(wait)? {
            return Ok(());
        }
        let msg = io.recv()?;
        // A command pushed before the answer that let the host send this
        // request is in effect for it.
        self.drain();
        self.poll_seat();
        if let Some(reply) = self.handle(msg)? {
            io.send(&reply)?;
        }
        Ok(())
    }

    /// Serves one message of the audio connection: `ProcessAudio` (answered)
    /// and `Reset` (not).
    ///
    /// # Errors
    ///
    /// The message is not one the audio connection carries: a peer that
    /// confuses its two connections.
    pub(crate) fn handle(&mut self, msg: HostMessage) -> BridgeResult<Option<BridgeMessage>> {
        match msg {
            HostMessage::ProcessAudio(data) => {
                let midi = &data.midi_events;
                let extras = ProcessExtras {
                    param_changes: &data.param_changes,
                    note_expression: &data.note_expression,
                    chords: &data.chords,
                    scales: &data.scales,
                    expr_texts: &data.expr_texts,
                    expr_ints: &data.expr_ints,
                    transport: &data.transport,
                    transport_changes: &data.transport_changes,
                };
                self.handle_process(AudioBlock {
                    seq: data.seq,
                    num_samples: data.num_samples,
                    midi,
                    extras: Some(extras),
                })
                .map(Some)
            }
            // The plugin forgets its signal history,
            // in order with the blocks on this connection, so a reset is never
            // reordered against a block. The pipeline is left alone, and the
            // host depends on that: clearing it on a seek is entirely the
            // host's job (it drops its in-flight block and never rewinds its
            // sequence, so a pre-seek block published late is structurally
            // unmatchable), and anything done to it here would be a second
            // mechanism that could disagree with the first.
            HostMessage::Reset => {
                if let Some(seat) = self.seat.as_mut() {
                    seat.reset();
                }
                Ok(None)
            }
            // The control connection's messages: a peer that confuses its two
            // connections. Named, not formatted: the render thread builds no
            // text.
            _ => Err(BridgeError::UnexpectedMessage {
                expected: "a message the audio connection carries",
                got: Cow::Borrowed("a control connection message"),
            }),
        }
    }

    /// Whether a block of `frames` may reach the plugin: not empty, within the
    /// length it was activated for, within the slab's per-channel room (the
    /// scratch's size), and into a slab laid out as the plugin's ports are now.
    /// The length is the peer's to choose, so it is checked here, once, for
    /// every format: unchecked, a long one sized the scratch to it on the audio
    /// thread, and the formats disagreed past the activated length (CLAP
    /// refused it, VST2 split it, VST3 passed it to the plugin). A plugin that changed its ports before the host set up a slab
    /// for them is read and written as the other layout otherwise. A refused
    /// block is counted and reads back as silence.
    fn block_fits(&mut self, frames: usize) -> bool {
        let (room, matches) = self.slab.as_ref().map_or((0, false), |slab| {
            let layout = slab.layout_ref();
            (
                layout.samples_per_channel,
                layout.input_channels() == self.config.inputs
                    && layout.output_channels() == self.config.outputs,
            )
        });
        let fits = matches
            && frames > 0
            && frames <= self.config.max_block
            && frames <= room
            && self
                .pipeline
                .fits(self.config.inputs.max(self.config.outputs), frames);
        if !fits {
            self.losses.refused_blocks.add(1);
        }
        fits
    }

    fn handle_process(&mut self, block: AudioBlock<'_>) -> BridgeResult<BridgeMessage> {
        // Read before `block` is moved into the pipeline: every reply below
        // echoes it.
        let seq = block.seq;
        let silent = || BridgeMessage::AudioProcessed {
            latency_us: 0,
            seq,
            midi_out: MidiEventVec::new(),
            status: BlockStatus::Continue,
            // Nothing is published on these paths.
            refused: true,
        };

        // NOTE: both early returns below reply WITHOUT publishing to the slab,
        // and that is deliberate. The host's read is gated on the output
        // sequence, so an unpublished block reads back as silence; there is no
        // separate "this block failed" signal to send, and none is wanted.
        // Publishing something here to "answer properly" would be actively
        // wrong: it would hand the host a slot full of whatever the previous
        // block left, marked valid.
        if self.seat.is_none() {
            // No plugin: nothing is published, and the reply is the refusal.
            return Ok(silent());
        }
        // A plugin that refused the rate renders silence: no block reaches it
        // and nothing is published (the host reads an unpublished block as
        // silence). Its MIDI is dropped; the host cut any open SysEx when it
        // re-prepared. A block of a length the plugin was not activated for,
        // or the slab cannot hold, is refused the same way (`block_fits`).
        if self.config.rate_refused {
            self.losses.refused_blocks.add(1);
            return Ok(silent());
        }
        if !self.block_fits(block.num_samples) {
            return Ok(silent());
        }
        // Neither the no-slab return just below nor a block the plugin's host
        // refuses before staging its MIDI (CLAP's `BlockTooLarge`) can drop
        // MIDI past a SysEx open in the reassembler, leaving a gap, from
        // tutti's own host: it sets the slab up before its bridge sends any
        // block, and its chunk never exceeds `max_buffer_size`, the
        // `block_size` the plugin was loaded with, which nothing here changes.
        // A peer that broke either would splice.
        let Self {
            slab,
            pipeline,
            seat,
            barrier,
            back,
            losses,
            failures,
            ..
        } = self;
        let (Some(seat), Some(slab)) = (seat.as_mut(), slab.as_mut()) else {
            // The process path only ever ran under the full `plugin + slab`
            // pair. Reply so the host's queue stays in step; no publish, so it
            // hears silence.
            return Ok(silent());
        };

        let reaches = seat.has_processor();
        let mut output = pipeline.process(seat.as_mut(), slab, block)?;
        if let Some(error) = output.failure.take() {
            *failures += 1;
            // A plugin failing every block is logged on a doubling schedule,
            // not on every block, and the rest are dropped here.
            if failures.is_power_of_two() && back.push(Back::Failure(error)).is_err() {
                losses.back_refused.add(1);
            }
        }
        if reaches && let Some(id) = barrier.take() {
            // This block ran the Processor after the barrier was taken.
            if back.push(Back::Ack(id)).is_err() {
                losses.back_refused.add(1);
            }
        }
        Ok(BridgeMessage::AudioProcessed {
            latency_us: output.latency_us,
            seq,
            // Borrowed from the pipeline's reusable buffer rather than moved
            // out of the result: moving it would take the buffer's heap
            // capacity with it and put the reallocation back on the audio
            // thread. See [`AudioOutput`](crate::audio_pipeline::AudioOutput).
            midi_out: {
                losses.midi_dropped.add(pipeline.midi_dropped() as u64);
                pipeline.midi_out().clone()
            },
            status: pipeline.status(),
            refused: output.refused,
        })
    }

    /// The pipeline, for the tests that read what it holds.
    #[cfg(test)]
    pub(crate) fn pipeline(&self) -> &AudioPipeline {
        &self.pipeline
    }

    /// Whether a slab is attached.
    #[cfg(test)]
    pub(crate) fn has_slab(&self) -> bool {
        self.slab.is_some()
    }
}

/// The thread that renders: owns the [`RenderSide`] and the host's audio
/// connection until it is stopped.
pub(crate) struct RenderThread {
    handle: Option<ThreadHandle<RenderSide>>,
    /// Why the audio connection ended, if it did: the thread then only trades
    /// the Processor, until it is stopped.
    ended: Arc<OnceLock<Arc<BridgeError>>>,
}

impl RenderThread {
    /// Starts the thread over `audio`, the host's audio connection.
    ///
    /// The thread asks the OS for the highest priority it grants
    /// ([`crate::server::request_audio_priority`]): it is the one that renders.
    pub(crate) fn spawn(side: RenderSide, audio: Box<dyn Transport>) -> std::io::Result<Self> {
        let ended = Arc::new(OnceLock::new());
        let handle = tutti_rt::spawn_named("plugin-render", None, {
            let ended = Arc::clone(&ended);
            move |stop: &StopToken| {
                crate::server::request_audio_priority("the render thread");
                run(side, audio, stop, &ended)
            }
        })?;
        Ok(Self {
            handle: Some(handle),
            ended,
        })
    }

    /// Why the audio connection ended, once it has.
    pub(crate) fn ended(&self) -> Option<&Arc<BridgeError>> {
        self.ended.get()
    }

    /// Stops the thread and takes its state back.
    ///
    /// # Panics
    ///
    /// If the render thread panicked (a plugin that panics is caught at the
    /// lifecycle; anything else here is a bug).
    pub(crate) fn stop(mut self) -> RenderSide {
        let handle = self.handle.take().expect("stopped once");
        handle.token().stop();
        handle.join().expect("the render thread does not panic")
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        // The handle's drop stops the thread and joins it.
        drop(self.handle.take());
    }
}

/// The thread's loop: serves the audio connection, and after it ends goes on
/// trading the Processor until `stop`.
fn run(
    mut side: RenderSide,
    audio: Box<dyn Transport>,
    stop: &StopToken,
    ended: &OnceLock<Arc<BridgeError>>,
) -> RenderSide {
    let mut audio = Some(audio);
    while !stop.is_stopped() {
        if let Err(error) = side.step(audio.as_mut(), RENDER_WAIT) {
            // The connection is gone; the Processor may still be recalled, so
            // the thread keeps trading it until the main thread has it back.
            audio = None;
            let _ = ended.set(Arc::new(error));
        }
    }
    side
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::AUDIO_REPLY_BYTES;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tutti_plugin_ipc::{AudioSlab, FrameBuf, ProcessAudioData, SlabLayout, write_frame};
    use tutti_plugin_types::{
        ChannelLayout, MIDI_STACK_CAPACITY, NoteExpressionIntValue, SampleRate, TransportChange,
        TransportInfo, WireMidi,
    };
    use tutti_ump::Ump;

    /// A slab of `samples` frames, stereo each way, and the host's end that
    /// keeps its memory alive.
    fn slab(name: &str, samples: usize) -> (impl Sized, SlabWriter) {
        let layout = SlabLayout::new(
            samples,
            SampleFormat::Float32,
            vec![ChannelLayout::STEREO],
            vec![ChannelLayout::STEREO],
        );
        let name = format!("tutti_render_{name}_{}", std::process::id());
        let host = AudioSlab::create(name.clone(), layout.clone()).expect("slab");
        (host, AudioSlab::open(name, layout).expect("slab"))
    }

    /// **A drain never allocates or frees**, whatever it applies: a second slab
    /// of another size over the first, a parameter list that grew, and a
    /// sample format that changed. The control side builds every value a
    /// command carries, and the values the render side replaces go back to it.
    #[test]
    fn a_drain_that_replaces_the_slab_refits_the_params_and_changes_format_never_allocates() {
        let (mut link, mut side) = link();
        let (_first_host, first) = slab("drain_first", 256);
        link.attach_slab(first);
        link.reserve_params(4, 64);
        side.drain();

        let (_second_host, second) = slab("drain_second", 4096);
        link.attach_slab(second);
        link.fit_params(300);
        link.set_format(SampleFormat::Float64);
        assert_no_alloc::assert_no_alloc(|| side.drain());
        assert!(side.has_slab());

        // What the drain replaced is home, to be freed here.
        link.reap();
        assert_eq!(link.losses(), Losses::default());
    }

    /// **A parameter list that grows after load gets its output reserved
    /// again, at the load's room; one that shrinks keeps it.** An output
    /// reserved for 4 parameters at a room of 10, fitted to 8, holds 8 queues;
    /// fitted to 2, it still does.
    #[test]
    fn a_grown_parameter_list_is_reserved_again_at_the_loads_room() {
        let (mut link, mut side) = link();
        link.reserve_params(4, 10);
        side.drain();
        assert_eq!(side.pipeline().param_queues(), 4);
        link.fit_params(8);
        side.drain();
        assert_eq!(side.pipeline().param_queues(), 8);
        link.fit_params(2);
        side.drain();
        assert_eq!(side.pipeline().param_queues(), 8);
    }

    /// A subscriber that counts the events it is told of.
    struct CountEvents(Arc<AtomicUsize>);

    impl tracing::Subscriber for CountEvents {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A plugin that fails every block, or floods its MIDI output.
    struct Faulty {
        fail: bool,
    }

    impl Render for Faulty {
        fn features(&self) -> Features {
            Features::empty()
        }

        fn render<'a>(
            &mut self,
            _buffer: PluginAudio<'a>,
            _ctx: &'a ProcessContext<'a>,
            out: &'a mut ProcessOutput,
        ) -> PluginResult<()> {
            if self.fail {
                return Err(PluginError::Block("the stand-in fails"));
            }
            let event = WireMidi {
                offset: 0,
                ump: Ump([0x2090_3C40, 0, 0, 0]),
            };
            out.emit_midi(std::iter::repeat_n(event, FLOOD));
            Ok(())
        }
    }

    impl RenderEnd for Faulty {
        fn poll(&mut self) {}
        fn reset(&mut self) {}
        fn set_features(&mut self, _: Features) {}
        fn has_processor(&mut self) -> bool {
            true
        }
    }

    /// The MIDI events a flooding plugin emits in a block: past the cap.
    const FLOOD: usize = MIDI_STACK_CAPACITY + 7;
    const FRAMES: usize = 64;

    /// A render side with a stereo slab and `plugin` seated, and the host's
    /// end of the slab.
    fn seated(name: &str, plugin: Faulty) -> (RenderLink, RenderSide, impl Sized) {
        let (mut link, mut side) = link();
        let (host, writer) = slab(name, FRAMES);
        link.attach_slab(writer);
        link.send(ToRender::Config(RenderConfig {
            max_block: FRAMES,
            inputs: 2,
            outputs: 2,
            ..RenderConfig::new()
        }));
        link.send(ToRender::Seat(Box::new(plugin)));
        side.drain();
        (link, side, host)
    }

    fn block(seq: u64) -> HostMessage {
        HostMessage::ProcessAudio(ProcessAudioData {
            seq,
            num_samples: FRAMES,
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        })
    }

    /// **The render thread never calls `tracing`**, whatever goes wrong in a
    /// block: a plugin that fails it, a block refused, a MIDI flood past the
    /// cap. What was lost is counted, and the main thread logs it.
    #[test]
    fn a_failed_refused_or_flooded_block_logs_nothing_on_the_render_thread() {
        let events = Arc::new(AtomicUsize::new(0));
        let during = tracing::subscriber::with_default(CountEvents(Arc::clone(&events)), || {
            let (mut link, mut side, _host) = seated("quiet_failing", Faulty { fail: true });
            for seq in 1..=5 {
                side.handle(block(seq)).expect("a failed block is answered");
            }
            // Past the activated length: refused.
            side.handle(HostMessage::ProcessAudio(ProcessAudioData {
                seq: 6,
                num_samples: FRAMES + 1,
                ..ProcessAudioData::empty(SampleRate::SR_48K)
            }))
            .expect("a refused block is answered");
            let during = events.load(Ordering::Relaxed);

            // The main thread reaps the failures' causes, and logs them.
            link.send(ToRender::Seat(Box::new(Faulty { fail: false })));
            side.drain();
            let before_flood = events.load(Ordering::Relaxed);
            let reply = side.handle(block(7)).expect("answered").expect("a reply");
            assert!(matches!(
                reply,
                BridgeMessage::AudioProcessed { ref midi_out, .. } if midi_out.len() == MIDI_STACK_CAPACITY
            ));
            assert_eq!(
                events.load(Ordering::Relaxed),
                before_flood,
                "the flood logged"
            );
            assert_eq!(
                link.losses(),
                Losses {
                    refused_blocks: 1,
                    failed_blocks: 5,
                    midi_dropped: 7,
                    back_refused: 0,
                }
            );
            // The main thread says it once, with the totals.
            link.report_losses();
            assert_eq!(events.load(Ordering::Relaxed), before_flood + 1);
            link.report_losses();
            assert_eq!(
                events.load(Ordering::Relaxed),
                before_flood + 1,
                "once per change"
            );
            during
        });
        assert_eq!(during, 0, "the render thread logged");
    }

    /// **The cause of the first block a plugin fails, and of each that makes a
    /// power of two, goes home to be logged and freed there**; the others are
    /// counted only, so a plugin failing every block is not a flood of
    /// hand-backs.
    #[test]
    fn a_failed_blocks_cause_goes_home_on_a_doubling_schedule() {
        let (mut link, mut side, _host) = seated("failing_causes", Faulty { fail: true });
        // The seat and slab hand-backs of the setup are home already.
        while link.back.pop().is_some() {}
        for seq in 1..=9 {
            side.handle(block(seq)).expect("answered");
        }
        let mut causes = 0;
        while let Some(back) = link.back.pop() {
            assert!(matches!(back, Back::Failure(PluginError::Block(_))));
            causes += 1;
        }
        // Failures 1, 2, 4 and 8.
        assert_eq!(causes, 4);
        assert_eq!(link.losses().failed_blocks, 9);
    }

    /// **A count is logged when it first appears and each time it doubles.**
    #[test]
    fn losses_are_reported_when_first_nonzero_and_each_doubling() {
        let none = Losses::default();
        let one = Losses {
            failed_blocks: 1,
            ..none
        };
        assert!(!none.worsened_since(none));
        assert!(one.worsened_since(none));
        assert!(!one.worsened_since(one));
        let three = Losses {
            failed_blocks: 3,
            ..none
        };
        assert!(!three.worsened_since(Losses {
            failed_blocks: 2,
            ..none
        }));
        assert!(
            Losses {
                failed_blocks: 4,
                ..none
            }
            .worsened_since(Losses {
                failed_blocks: 2,
                ..none
            })
        );
        // Another count appearing is news too.
        assert!(
            Losses {
                failed_blocks: 1,
                midi_dropped: 1,
                ..none
            }
            .worsened_since(one)
        );
    }

    /// The server's end of an audio connection in memory: it reads the same
    /// request frame every time, through the buffers the real connection reads
    /// through, and counts the replies it writes into a fixed sink.
    struct Replay {
        frames: FrameBuf,
        request: Vec<u8>,
        sink: Box<[u8]>,
        replies: usize,
    }

    impl Transport for Replay {
        fn recv(&mut self) -> BridgeResult<HostMessage> {
            self.frames.read(&mut self.request.as_slice())
        }

        fn wait_readable(&mut self, _: Duration) -> BridgeResult<bool> {
            Ok(true)
        }

        fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
            let mut sink: &mut [u8] = &mut self.sink;
            self.frames.write(&mut sink, msg)?;
            sink.flush()?;
            self.replies += 1;
            Ok(())
        }
    }

    /// **A block served the way production serves it allocates nothing**, from
    /// the first one: `step` reads the request through the connection's fixed
    /// buffers, decodes it with its extras (MIDI, transport changes and
    /// per-note integer expression, as a host sends them), renders it, and
    /// answers with the plugin's MIDI-out past its cap, which is counted and
    /// not logged. The block's automation and note expression are empty here:
    /// a decode of points still allocates (see `PointBuf`'s `Deserialize`).
    #[test]
    fn a_step_with_extras_reads_decodes_renders_and_answers_without_allocating() {
        let (mut link, mut side, _host) = seated("step_alloc", Faulty { fail: false });
        let mut block = ProcessAudioData {
            seq: 1,
            num_samples: FRAMES,
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        };
        for offset in 0..3 {
            block.midi_events.push(WireMidi {
                offset,
                ump: Ump([0x2090_3C40, 0, 0, 0]),
            });
        }
        block.transport_changes.push(TransportChange {
            offset: 8,
            transport: TransportInfo::new(SampleRate::SR_48K),
        });
        block.expr_ints.changes.push(NoteExpressionIntValue {
            sample_offset: 0,
            note_id: 1,
            type_id: 2,
            value: 3,
        });
        let mut request = Vec::new();
        write_frame(&mut request, &HostMessage::ProcessAudio(block)).unwrap();
        let mut io: Box<dyn Transport> = Box::new(Replay {
            frames: FrameBuf::new(request.len(), AUDIO_REPLY_BYTES),
            request,
            sink: vec![0; AUDIO_REPLY_BYTES + 4].into_boxed_slice(),
            replies: 0,
        });
        const STEPS: usize = 16;
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..STEPS {
                side.step(Some(&mut io), Duration::ZERO).unwrap();
            }
        });
        link.reap();
        assert_eq!(link.losses().midi_dropped, 7 * STEPS as u64);
        assert_eq!(link.losses().refused_blocks, 0);
    }
}
