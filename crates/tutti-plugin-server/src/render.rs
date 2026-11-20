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
//!   and when the main thread rings the thread's wake, so a recall is never
//!   left waiting for a block: [`crate::ties`]).
//! - **Commands** ([`ToRender`]) travel one way, main to render, through a
//!   `tutti_rt` ring: the slab with the scratch sized for it, the plugin's
//!   parameter output, the figures a block reads ([`RenderConfig`]) and the seat the Processor
//!   arrives in. A command carries a finished value: the main thread builds
//!   the scratch, the parameter output and the slab's room, and the render
//!   thread only swaps them in. What the render thread has finished with goes
//!   back through a second queue ([`Back`]) to be dropped on the main thread.
//!   A hand-back the queue refuses is dropped where it is and counted
//!   ([`Losses::back_refused`]), which only an abandoned session reaches. A
//!   block's failure is a constant ([`BlockFault`]), so losing one frees
//!   nothing.
//! - **Losses** ([`Losses`]) are `tutti_rt` tallies the render thread adds to
//!   and the main thread reads and logs ([`RenderLink::report_losses`]): the
//!   render thread never calls `tracing`.
//!
//! Commands are applied before the request that follows them is served, so a
//! command the main thread pushed before it answered the host is in effect for
//! every block the host sends after that answer.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_platform::{StopToken, ThreadHandle};
use tutti_rt::LockExt;

use tutti_buffers::Float;
use tutti_buffers::FloatWidth;
use tutti_plugin_ipc::{AudioReply, AudioRequest, SlabLayout, SlabWriter};
use tutti_plugin_types::Samples;
use tutti_plugin_types::{
    BlockStatus, BridgeError, BridgeResult, Features, MidiEventVec, PluginIo, PluginResult,
    ProcessOutput, Processor,
};
use tutti_plugin_types::{EditWriter, ParameterChanges};
use tutti_rt::{
    Overflow, PushError, Returned, RingReader, RingWriter, Rt, Tally, TallyWriter, ring, tally,
};

use crate::audio_pipeline::{
    AudioBlock, AudioPipeline, AudioScratch, BlockFault, Clock, ProcessExtras,
};
use crate::main_loop::Bell;
use crate::served::Render;
use crate::ties::{Notify, Patience, Ties, ties};
use crate::transport::AudioTransport;

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
    /// it, returns one that was recalled). Whether the Processor's presence
    /// changed.
    fn poll(&mut self) -> bool;
    /// Forgets the plugin's signal history, between blocks. A plugin whose
    /// Processor is not here has nothing to forget.
    fn reset(&mut self);
    /// What the plugin consumes, as it reports now.
    fn set_features(&mut self, features: Features);
    /// Whether the Processor is here: a block rendered now reaches the plugin
    /// (and drains the writes queued to it).
    fn has_processor(&mut self) -> bool;
    /// Delivers parameter changes between blocks by the format's own route
    /// ([`Processor::flush_parameters`]); a plugin whose Processor is not here
    /// has nothing to deliver them to.
    fn flush_parameters(&mut self, changes: &ParameterChanges) -> PluginResult<()>;
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

    fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()> {
        self.render(io)
    }

    fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()> {
        self.render(io)
    }
}

impl<P: Processor + 'static> End<P> {
    /// One block at either width: silence when no Processor is held.
    fn render<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        // A Processor that is not here (recalled for a rate change, or
        // refused): the block is silence and the plugin is not touched.
        let Some(processor) = self.rt.held() else {
            io.out.clear();
            io.buffer.output().silence();
            return Ok(());
        };
        processor.process(io)
    }
}

impl<P: Processor + 'static> RenderEnd for End<P> {
    fn poll(&mut self) -> bool {
        let before = self.rt.held().is_some();
        let _ = self.rt.poll(Processor::stop);
        before != self.rt.held().is_some()
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

    fn flush_parameters(&mut self, changes: &ParameterChanges) -> PluginResult<()> {
        match self.rt.held() {
            Some(processor) => processor.flush_parameters(changes),
            None => Ok(()),
        }
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
    /// The writer of the slots the render thread leaves each block's parameter
    /// values in, for the main thread to tell the plugin's edit controller of
    /// ([`Session::tick`](crate::session::Session::tick)).
    Informs(EditWriter),
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
///
/// The variants that own memory are never read: a value is handed back only
/// to be dropped by the thread that may free it.
pub(crate) enum Back {
    /// The seat of a plugin that is gone.
    #[expect(dead_code, reason = "handed back only to be dropped")]
    Seat(Box<dyn RenderEnd>),
    /// The slab.
    #[expect(dead_code, reason = "handed back only to be dropped")]
    Slab(SlabWriter),
    /// The scratch a new one replaced.
    #[expect(dead_code, reason = "handed back only to be dropped")]
    Scratch(Box<AudioScratch>),
    /// The parameter output a new one replaced.
    #[expect(dead_code, reason = "handed back only to be dropped")]
    Output(Box<ProcessOutput>),
    /// The writer of the slots a new one replaced.
    #[expect(dead_code, reason = "handed back only to be dropped")]
    Informs(EditWriter),
    /// Why the plugin failed a block, for the main thread to log.
    Failure(BlockFault),
    /// A block that reached the plugin after the [`ToRender::Barrier`] of this
    /// number was taken.
    Ack(u64),
}

/// What the render thread lost, counted since the session began.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Losses {
    /// Blocks refused: with no plugin seated, for their length (empty, or past
    /// the activated length or the slab's room), for a slab laid out unlike
    /// the plugin's ports, or for a rate the plugin refused. Each reads back as
    /// silence.
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
    ties: Ties,
    to: RingWriter<ToRender>,
    back: RingReader<Back>,
    losses: LossReaders,
    /// What [`report_losses`](Self::report_losses) last logged.
    logged: Losses,
    /// The format blocks are rendered in, which scratch is built for.
    format: FloatWidth,
    /// The layout of the attached slab, if one is: what scratch is built for.
    slab_layout: Option<SlabLayout>,
    /// The parameter queues and the points each holds that the plugin's output
    /// was last built for.
    params: (usize, usize),
    /// The number of the last barrier sent, and of the last one acknowledged.
    barrier_sent: u64,
    barrier_acked: u64,
    /// Counts the loads: what the render thread left for the edit controller
    /// before one is stale ([`Informs`]). One writer, the main thread.
    informs_loads: TallyWriter<u64>,
}

impl RenderLink {
    /// A state or preset load is about to replace the values the blocks so far
    /// left for the edit controller: they are voided, wherever the render thread
    /// is in writing them.
    pub(crate) fn void_informs(&self) {
        self.informs_loads.add(1);
    }

    /// What wakes the render thread and what it wakes.
    pub(crate) fn ties(&self) -> &Ties {
        &self.ties
    }

    /// Attaches `slab`: the render thread takes it with scratch sized to its
    /// layout, in the format blocks are rendered in.
    pub(crate) fn attach_slab(&mut self, slab: SlabWriter) {
        let layout = slab.layout_ref().clone();
        let scratch = Box::new(AudioScratch::for_layout(self.format, &layout));
        self.slab_layout = Some(layout);
        self.send(ToRender::Slab { slab, scratch });
    }

    /// Detaches the slab (an unload).
    pub(crate) fn detach_slab(&mut self) {
        self.slab_layout = None;
        self.send(ToRender::DropSlab);
    }

    /// Renders in `format` from the next block on: scratch of the slab's shape
    /// is rebuilt when it changed, and later slabs are sized in it.
    pub(crate) fn set_format(&mut self, format: FloatWidth) {
        if format == self.format {
            return;
        }
        self.format = format;
        if let Some(layout) = &self.slab_layout {
            let scratch = Box::new(AudioScratch::for_layout(format, layout));
            self.send(ToRender::Scratch(scratch));
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

    /// Reaps what the render thread handed back, and logs what it has lost
    /// when a count has first become nonzero or doubled since it was last
    /// logged ([`Losses::worsened_since`]): the render thread counts and never
    /// logs.
    pub(crate) fn report_losses(&mut self) {
        self.reap();
        let now = self.losses();
        if now.worsened_since(self.logged) {
            self.log_losses(now);
        }
    }

    /// Reaps, and logs the final counts when they differ from what was last
    /// logged: a run that stops at 3 reports 3.
    pub(crate) fn report_final_losses(&mut self) {
        self.reap();
        let now = self.losses();
        if now != self.logged {
            self.log_losses(now);
        }
    }

    fn log_losses(&mut self, now: Losses) {
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

    /// Queues `command` and wakes the render thread, waiting for it to make
    /// room if the queue is full. A command the render thread does not take
    /// within [`SEND_PATIENCE`] is dropped and logged: the render thread has
    /// stopped.
    pub(crate) fn send(&mut self, command: ToRender) {
        self.reap();
        let mut command = command;
        let patience = Patience::new(Instant::now(), SEND_PATIENCE);
        loop {
            let seen = self.ties.settle.seen();
            match self.to.push(command) {
                Ok(()) => {
                    self.ties.wake.ring();
                    return;
                }
                Err(Returned {
                    error: PushError::Disconnected,
                    ..
                }) => {
                    tracing::error!("plugin-server: the render thread is gone");
                    return;
                }
                Err(Returned { value, .. }) => command = value,
            }
            if patience.left(Instant::now()).is_none() {
                tracing::error!("plugin-server: the render thread takes no commands");
                return;
            }
            // Room is made when the render thread takes a command.
            self.ties.settle.wait_past(seen, patience);
        }
    }

    /// Drops what the render thread handed back, noting the barriers it
    /// acknowledged.
    pub(crate) fn reap(&mut self) {
        while let Some(back) = self.back.pop() {
            match back {
                Back::Ack(id) => self.barrier_acked = self.barrier_acked.max(id),
                Back::Failure(BlockFault(reason)) => {
                    tracing::warn!("the plugin failed a block: {reason}");
                }
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
        let patience = Patience::new(Instant::now(), within);
        loop {
            let seen = self.ties.settle.seen();
            self.reap();
            if self.barrier_acked >= id {
                return true;
            }
            if patience.left(Instant::now()).is_none() {
                return false;
            }
            // The acknowledgement is handed back by the render thread.
            self.ties.settle.wait_past(seen, patience);
        }
    }
}

/// Where the render thread leaves each block's last value of each parameter,
/// for the main thread to tell the plugin's edit controller of.
///
/// A state or preset load makes what the slots hold stale. The main thread
/// counts its loads (a [`Tally`]); the render thread reads the count when a
/// block begins and, after writing the block's values, ends the epoch if the
/// count moved meanwhile. A block that began before a load and writes after it
/// voids its own values; a block that began after it sees the new count at its
/// start and keeps them, however long the render thread idled before it.
/// Wait-free.
pub(crate) struct Informs {
    writer: Option<EditWriter>,
    loads: Tally<u64>,
}

impl Informs {
    fn new(loads: Tally<u64>) -> Self {
        Self {
            writer: None,
            loads,
        }
    }

    /// Installs the writer of the slots, returning the one it replaces for the
    /// main thread to free.
    fn install(&mut self, writer: EditWriter) -> Option<EditWriter> {
        self.writer.replace(writer)
    }

    /// The loads so far, read as a block begins.
    pub(crate) fn begin(&self) -> u64 {
        self.loads.get()
    }

    /// Leaves each parameter's last value of `changes`, then ends the epoch if
    /// a load came since `began` ([`begin`](Self::begin)).
    pub(crate) fn note(&mut self, changes: &ParameterChanges, began: u64) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        for queue in &changes.queues {
            if let Some(point) = queue.points.last() {
                // A parameter the plugin did not list has no slot.
                let _ = writer.write(queue.param_id, point.value);
            }
        }
        if self.loads.get() != began {
            writer.reset();
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
    /// Where each block leaves the last value it carried for each parameter,
    /// for the edit controller.
    informs: Informs,
    /// What ends the thread's wait: rung by the main thread.
    wake: Bell,
    /// How the thread tells the main thread it has made progress.
    notify: Notify,
    losses: LossWriters,
    /// Blocks the plugin has failed: the cause of the first, and of each
    /// that makes a power of two, goes home ([`Back::Failure`]).
    failures: u64,
}

/// A session's two halves: the control side the main thread drives, and the
/// render side, which a thread runs ([`RenderThread::spawn`]) once the host's
/// audio connection is there.
///
/// # Errors
///
/// The OS refused a bell.
pub(crate) fn link() -> std::io::Result<(RenderLink, RenderSide)> {
    let (ties, render_ties) = ties()?;
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
    let loads = tally();
    Ok((
        RenderLink {
            ties,
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
            slab_layout: None,
            params: (0, 0),
            barrier_sent: 0,
            barrier_acked: 0,
            informs_loads: loads.writer(),
        },
        RenderSide {
            commands,
            back: back_tx,
            slab: None,
            pipeline,
            config,
            seat: None,
            barrier: None,
            informs: Informs::new(loads),
            wake: render_ties.wake,
            notify: render_ties.notify,
            losses: writers,
            failures: 0,
        },
    ))
}

impl RenderSide {
    /// Applies what the main thread queued. Allocates and frees nothing: a
    /// command carries finished values, and what a swap replaces goes back to
    /// the main thread.
    pub(crate) fn drain(&mut self) {
        let mut took = false;
        while let Some(command) = self.commands.pop() {
            took = true;
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
                ToRender::Informs(writer) => {
                    if let Some(old) = self.informs.install(writer) {
                        self.give_back(Back::Informs(old));
                    }
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
        if took {
            // Room in the queue, for a main thread that waits for it.
            self.notify.bump();
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
        self.notify.bump();
    }

    /// Between blocks: trades the Processor with the main thread.
    pub(crate) fn poll_seat(&mut self) {
        if let Some(seat) = self.seat.as_mut()
            && seat.poll()
        {
            // A Processor came or went: the main thread may be waiting for it.
            self.notify.bump();
        }
    }

    /// One pass of the thread: applies the commands, trades the Processor,
    /// waits for a request on `io` or for the main thread's wake, and serves
    /// the request. With no `io` (the host's audio connection has gone) it
    /// waits for the wake alone.
    ///
    /// # Errors
    ///
    /// The transport fails (the host went away, or sent a frame that does not
    /// decode).
    pub(crate) fn step(&mut self, io: Option<&mut Box<AudioTransport>>) -> BridgeResult<()> {
        self.drain();
        self.poll_seat();
        let Some(io) = io else {
            self.wake.wait(None)?;
            return Ok(());
        };
        let readable = io.wait_readable(None, &self.wake)?;
        self.wake.clear();
        if !readable {
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

    /// Serves one message of the audio connection: `ProcessAudio` and
    /// `ApplyParameters` (answered) and `Reset` (not).
    ///
    /// # Errors
    ///
    /// A block's reply cannot be built.
    pub(crate) fn handle(&mut self, msg: AudioRequest) -> BridgeResult<Option<AudioReply>> {
        match msg {
            AudioRequest::ProcessAudio(data) => {
                let began = self.informs.begin();
                let midi = &data.midi_events;
                let reply = self.handle_process(AudioBlock {
                    seq: data.seq,
                    num_samples: data.num_samples,
                    midi,
                    extras: ProcessExtras::of(&data),
                })?;
                // The edit controller is told of what the Processor was, each
                // parameter's last value of the block, whoever wrote it: the
                // host's write rode this block as a change on its first frame.
                self.informs.note(&data.param_changes, began);
                Ok(Some(reply))
            }
            // The host's writes no block has carried, in order with the blocks
            // on this connection: the plugin has them, through the format's own
            // route for parameters outside a block, before the answer.
            AudioRequest::ApplyParameters { id, param_changes } => {
                let began = self.informs.begin();
                if let Some(seat) = self.seat.as_mut()
                    && seat.flush_parameters(&param_changes).is_err()
                {
                    self.failures += 1;
                }
                self.informs.note(&param_changes, began);
                Ok(Some(AudioReply::ParametersApplied { id }))
            }
            // The plugin forgets its signal history,
            // in order with the blocks on this connection, so a reset is never
            // reordered against a block. The pipeline is left alone, and the
            // host depends on that: clearing it on a seek is entirely the
            // host's job (it drops its in-flight block and never rewinds its
            // sequence, so a pre-seek block published late is structurally
            // unmatchable), and anything done to it here would be a second
            // mechanism that could disagree with the first.
            AudioRequest::Reset => {
                if let Some(seat) = self.seat.as_mut() {
                    seat.reset();
                }
                Ok(None)
            }
        }
    }

    /// Whether a block of `frames` may reach the plugin: not empty, within the
    /// length it was activated for, within the slab's per-channel room (the
    /// scratch's size), and into a slab laid out as the plugin's ports are now.
    /// The length is the peer's to choose, so it is checked here, once, for
    /// every format: unchecked, a long one sized the scratch to it on the audio
    /// thread, and the formats disagreed past the activated length (CLAP
    /// refused it, VST2 split it, VST3 passed it to the plugin). A plugin that
    /// changed its ports before the host set up a slab for them is read and
    /// written as the other layout otherwise. A refused block is counted and
    /// reads back as silence.
    fn block_fits(&mut self, frames: Samples) -> bool {
        let (room, matches) = self.slab.as_ref().map_or((Samples::ZERO, false), |slab| {
            let layout = slab.layout_ref();
            (
                Samples::new(layout.samples_per_channel),
                layout.input_channels() == self.config.inputs
                    && layout.output_channels() == self.config.outputs,
            )
        });
        let fits = matches
            && frames > Samples::ZERO
            && frames <= Samples::new(self.config.max_block)
            && frames <= room
            && self
                .pipeline
                .fits(self.config.inputs.max(self.config.outputs), frames);
        if !fits {
            self.losses.refused_blocks.add(1);
        }
        fits
    }

    fn handle_process(&mut self, block: AudioBlock<'_>) -> BridgeResult<AudioReply> {
        // Read before `block` is moved into the pipeline: every reply below
        // echoes it.
        let seq = block.seq;
        let silent = || AudioReply::AudioProcessed {
            latency_us: 0,
            seq,
            midi_out: MidiEventVec::new(),
            status: BlockStatus::Continue,
            // Nothing is published on these paths.
            refused: true,
        };

        // NOTE: every early return below replies WITHOUT publishing to the slab,
        // and that is deliberate. The host's read is gated on the output
        // sequence, so an unpublished block reads back as silence; there is no
        // separate "this block failed" signal to send, and none is wanted.
        // Publishing something here to "answer properly" would be actively
        // wrong: it would hand the host a slot full of whatever the previous
        // block left, marked valid.
        if self.seat.is_none() {
            // No plugin: nothing is published, and the reply is the refusal.
            self.losses.refused_blocks.add(1);
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
        if !self.block_fits(Samples::new(block.num_samples)) {
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
            // Both were checked above: a refusal all the same, counted, so the
            // host's queue stays in step and it hears silence.
            losses.refused_blocks.add(1);
            return Ok(silent());
        };

        let reaches = seat.has_processor();
        let mut output = pipeline.process(seat.as_mut(), slab, block);
        if let Some(fault) = output.failure.take() {
            *failures += 1;
            // A plugin failing every block is logged on a doubling schedule,
            // not on every block; the others are counted only.
            if failures.is_power_of_two() && back.push(Back::Failure(fault)).is_err() {
                losses.back_refused.add(1);
            }
        }
        if reaches && let Some(id) = barrier.take() {
            // This block ran the Processor after the barrier was taken.
            if back.push(Back::Ack(id)).is_err() {
                losses.back_refused.add(1);
            }
        }
        Ok(AudioReply::AudioProcessed {
            latency_us: output.latency_us,
            seq,
            // An allocation-free copy of the inline list, which stays in the
            // pipeline's output for the next block to clear.
            midi_out: {
                let dropped = u64::try_from(pipeline.midi_dropped()).unwrap_or(u64::MAX);
                losses.midi_dropped.add(dropped);
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

/// A render thread that could not start, with the side it was to run.
pub(crate) struct SpawnFailed {
    /// Why it could not start.
    pub(crate) error: std::io::Error,
    /// The render side, still whole.
    pub(crate) side: RenderSide,
}

impl std::fmt::Debug for SpawnFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnFailed")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// The thread that renders: owns the [`RenderSide`] and the host's audio
/// connection until it is stopped.
pub(crate) struct RenderThread {
    handle: Option<ThreadHandle<RenderSide>>,
    /// Rung to end the thread's wait when it is stopped.
    wake: Bell,
    /// Why the audio connection ended, if it did: the thread then only trades
    /// the Processor, until it is stopped.
    ended: Arc<OnceLock<Arc<BridgeError>>>,
}

impl RenderThread {
    /// Starts the thread over `audio`, the host's audio connection, or over
    /// none: a thread that only trades the Processor until it is stopped.
    ///
    /// The thread asks the OS for the highest priority it grants
    /// ([`crate::server::request_audio_priority`]): it is the one that renders.
    ///
    /// # Errors
    ///
    /// The thread could not start: the render side comes back with the error,
    /// for whoever still has a Processor to bring home through it.
    pub(crate) fn spawn(
        side: RenderSide,
        audio: Option<Box<AudioTransport>>,
    ) -> Result<Self, Box<SpawnFailed>> {
        let ended = Arc::new(OnceLock::new());
        let wake = side.wake.clone();
        let slot = Arc::new(Mutex::new(Some(side)));
        let spawned = tutti_platform::spawn_named("plugin-render", None, {
            let (ended, slot) = (Arc::clone(&ended), Arc::clone(&slot));
            move |stop: &StopToken| {
                let side = slot.lock_recover().take().expect("the side is taken once");
                // Declared before the first call into the plugin's code, so a
                // panic anywhere below is an end the main loop sees.
                let _unwinding = EndedByPanic(&ended);
                crate::server::request_audio_priority("the render thread");
                run(side, audio, stop, &ended)
            }
        });
        match spawned {
            Ok(handle) => Ok(Self {
                handle: Some(handle),
                wake,
                ended,
            }),
            Err(error) => Err(Box::new(SpawnFailed {
                error,
                side: slot
                    .lock_recover()
                    .take()
                    .expect("a thread that did not start took nothing"),
            })),
        }
    }

    /// Why the audio connection ended, once it has.
    pub(crate) fn ended(&self) -> Option<&Arc<BridgeError>> {
        self.ended.get()
    }

    /// Stops the thread and takes its state back, waiting at most `patience`
    /// for it: a thread stuck inside the plugin is given up on (detached), and
    /// the caller ends the process.
    ///
    /// # Errors
    ///
    /// The thread panicked (its state went with it), or it did not end within
    /// `patience`.
    pub(crate) fn stop_within(mut self, patience: Duration) -> Result<RenderSide, NotHome> {
        let handle = self.handle.take().expect("stopped once");
        handle.token().stop();
        self.wake.ring();
        match handle.stop_within(patience) {
            Ok(Ok(side)) => Ok(side),
            Ok(Err(panicked)) => Err(NotHome::Panicked(panicked)),
            Err(stuck) => {
                stuck.detach();
                Err(NotHome::Stuck)
            }
        }
    }
}

/// Why a stopped render thread did not hand its state back.
#[derive(Debug)]
pub(crate) enum NotHome {
    /// The thread panicked.
    Panicked(tutti_platform::Panicked),
    /// The thread is still running (a plugin that does not return from a block).
    Stuck,
}

/// Records, as the end of the audio connection, a thread that unwinds: the main
/// loop reads that and ends the session, which a panic that only reached the
/// join would leave running.
struct EndedByPanic<'a>(&'a OnceLock<Arc<BridgeError>>);

impl Drop for EndedByPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = self.0.set(Arc::new(BridgeError::Other(
                "the render thread panicked".into(),
            )));
        }
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        // The handle's drop stops the thread and joins it: the wake ends the
        // wait the join would otherwise sit through.
        if let Some(handle) = self.handle.as_ref() {
            handle.token().stop();
            self.wake.ring();
        }
        drop(self.handle.take());
    }
}

/// The thread's loop: serves the audio connection, and after it ends goes on
/// trading the Processor until `stop`.
fn run(
    mut side: RenderSide,
    mut audio: Option<Box<AudioTransport>>,
    stop: &StopToken,
    ended: &OnceLock<Arc<BridgeError>>,
) -> RenderSide {
    while !stop.is_stopped() {
        if let Err(error) = side.step(audio.as_mut()) {
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

    /// What the render thread left for the edit controller: the slots' reader,
    /// the writer in an [`Informs`], and the main thread's count of loads.
    fn informs() -> (Informs, tutti_plugin_types::EditReader, TallyWriter<u64>) {
        let loads = tally();
        let writer_end = loads.writer();
        let mut informs = Informs::new(loads);
        let (writer, reader) = tutti_plugin_types::edit_slots([tutti_units::ParamId::new(1)]);
        assert!(informs.install(writer).is_none());
        (informs, reader, writer_end)
    }

    fn changes_of(value: f64) -> ParameterChanges {
        let mut changes = ParameterChanges::reserved(1, 1);
        assert!(changes.add_change(tutti_units::ParamId::new(1), 0, value));
        changes
    }

    fn told(reader: &mut tutti_plugin_types::EditReader) -> Vec<f64> {
        let mut out = Vec::new();
        reader.drain(|_, value| out.push(value.get()));
        out
    }

    /// **A block's last value of a parameter is left for the edit controller**,
    /// the newest of several blocks.
    #[test]
    fn a_blocks_last_value_is_left_for_the_edit_controller() {
        let (mut informs, mut reader, _) = informs();
        let began = informs.begin();
        informs.note(&changes_of(0.25), began);
        informs.note(&changes_of(0.5), began);
        assert_eq!(told(&mut reader), [0.5]);
    }

    /// **A load voids the values a block that began before it writes after
    /// it**: the block reads the count, the load comes, the block writes, and
    /// its values do not reach the controller.
    #[test]
    fn a_block_that_began_before_a_load_is_voided() {
        let (mut informs, mut reader, loads) = informs();
        let began = informs.begin();
        loads.add(1);
        informs.note(&changes_of(0.25), began);
        assert!(told(&mut reader).is_empty(), "the stale block's value");
    }

    /// **A write after a load, on a render thread that idled between them,
    /// reaches the controller**: the first block after the load reads the new
    /// count when it begins, and keeps its values.
    #[test]
    fn the_first_write_after_a_load_on_an_idle_render_thread_is_told() {
        let (mut informs, mut reader, loads) = informs();
        loads.add(1);
        let began = informs.begin();
        informs.note(&changes_of(0.75), began);
        assert_eq!(told(&mut reader), [0.75]);
    }
    use crate::transport::{AUDIO_REPLY_BYTES, Transport};
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tutti_plugin_ipc::{AudioSlab, FrameBuf, ProcessAudioData, SlabLayout, write_frame};
    use tutti_plugin_types::{
        ChannelLayout, MIDI_STACK_CAPACITY, NoteExpressionIntValue, PluginError, SampleRate,
        TransportChange, TransportInfo, WireMidi,
    };
    use tutti_ump::Ump;

    /// A slab of `samples` frames, stereo each way, and the host's end that
    /// keeps its memory alive.
    fn slab(name: &str, samples: usize) -> (impl Sized, SlabWriter) {
        let layout = SlabLayout::new(
            samples,
            FloatWidth::F32,
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
        let (mut link, mut side) = link().expect("bells");
        let (_first_host, first) = slab("drain_first", 256);
        link.attach_slab(first);
        link.reserve_params(4, 64);
        side.drain();

        let (_second_host, second) = slab("drain_second", 4096);
        link.attach_slab(second);
        link.fit_params(300);
        link.set_format(FloatWidth::F64);
        assert_no_alloc::assert_no_alloc(|| side.drain());
        assert!(side.has_slab());

        // What the drain replaced is home, to be freed here.
        link.reap();
        assert_eq!(link.losses(), Losses::default());
    }

    /// A host connection that says a frame has begun and then does what `recv`
    /// is told to: a peer whose frame never completes, or a transport that
    /// breaks.
    struct Begun<F: FnMut() -> BridgeResult<AudioRequest> + Send>(F);

    impl<F: FnMut() -> BridgeResult<AudioRequest> + Send> Transport<AudioRequest, AudioReply>
        for Begun<F>
    {
        fn recv(&mut self) -> BridgeResult<AudioRequest> {
            (self.0)()
        }

        fn wait_readable(&mut self, _: Option<Duration>, _: &Bell) -> BridgeResult<bool> {
            Ok(true)
        }

        fn send(&mut self, _: &AudioReply) -> BridgeResult<()> {
            Ok(())
        }
    }

    /// **A render thread stuck in a read is given up on, not joined for ever**:
    /// the stop returns `Stuck` within its patience, so the server ends the
    /// process instead of leaving an orphan that nothing can stop. The read
    /// returns only when the test lets it, after the stop has answered.
    #[test]
    fn a_render_thread_stuck_in_a_read_is_given_up_on() {
        let (_link, side) = link().expect("bells");
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (inside, entered) = std::sync::mpsc::channel::<()>();
        let stalled = Begun(move || {
            let _ = inside.send(());
            let _ = released.recv();
            Err(BridgeError::ProcessCrashed)
        });
        let thread = RenderThread::spawn(side, Some(Box::new(stalled))).expect("the thread");

        entered.recv().expect("the thread reads");
        let stopped = thread.stop_within(Duration::from_millis(50)).err();

        assert!(matches!(stopped, Some(NotHome::Stuck)), "{stopped:?}");
        drop(release);
    }

    /// **A render thread that panics ends the audio connection as the main loop
    /// sees it**, and its stop reports the panic: the main thread neither
    /// carries on over a dead renderer nor panics itself on the join.
    #[test]
    fn a_render_thread_that_panics_is_seen_by_the_main_loop_and_reported_by_its_stop() {
        let (_link, side) = link().expect("bells");
        let broken = Begun(|| panic!("the transport broke"));
        let thread = RenderThread::spawn(side, Some(Box::new(broken))).expect("the thread");

        tutti_fixtures::wait_until("the panic to be seen", Duration::from_secs(60), || {
            thread.ended().is_some()
        });

        let stopped = thread.stop_within(Duration::from_secs(60)).err();
        match stopped {
            Some(NotHome::Panicked(panicked)) => {
                assert!(
                    panicked.message.contains("the transport broke"),
                    "{panicked}"
                );
            }
            other => panic!("expected the panic, got {other:?}"),
        }
    }

    /// The tests that need a socket pair: a unix local socket is a descriptor.
    #[cfg(unix)]
    mod woken {
        use super::*;
        use crate::main_loop::Stream;
        use crate::transport::SocketTransport;
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;

        /// **A command wakes a render thread that waits on a quiet connection**:
        /// with no block and no timer to end its wait, the thread takes the slab the
        /// main thread queued, and says so. The wait for that outlasts the test
        /// harness's limit, so a thread no command wakes fails.
        #[test]
        fn a_command_wakes_a_render_thread_that_waits_on_a_quiet_connection() {
            let (mut link, side) = link().expect("bells");
            let (_host_end, server_end) = UnixStream::pair().expect("socket pair");
            let audio = SocketTransport::audio(Stream::from(OwnedFd::from(server_end)))
                .expect("the audio transport");
            let thread = RenderThread::spawn(side, Some(Box::new(audio))).expect("the thread");

            let (_slab_host, slab) = slab("wake", 256);
            let settle = link.ties().settle.clone();
            let seen = settle.seen();
            link.attach_slab(slab);
            settle.wait_past(
                seen,
                Patience::new(Instant::now(), Duration::from_secs(600)),
            );

            let side = thread
                .stop_within(Duration::from_secs(600))
                .expect("the thread hands its side back");
            assert!(side.has_slab(), "the command was taken while no block came");
        }
    }

    /// **A parameter list that grows after load gets its output reserved
    /// again, at the load's room; one that shrinks keeps it.** An output
    /// reserved for 4 parameters at a room of 10, fitted to 8, holds 8 queues;
    /// fitted to 2, it still does.
    #[test]
    fn a_grown_parameter_list_is_reserved_again_at_the_loads_room() {
        let (mut link, mut side) = link().expect("bells");
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

    /// How a stand-in plugin behaves.
    enum Mode {
        /// Fails every block with a constant.
        Fail,
        /// Floods its MIDI output past the cap.
        Flood,
        /// Fails the odd blocks and floods the even.
        Alternate,
        /// Fails every block with the next of these, built beforehand: errors
        /// that own memory, as a Processor breaking its contract returns.
        Owned(Vec<PluginError>),
    }

    /// A stand-in plugin.
    struct Faulty {
        mode: Mode,
        blocks: usize,
    }

    impl Faulty {
        fn new(mode: Mode) -> Self {
            Self { mode, blocks: 0 }
        }
    }

    impl Render for Faulty {
        fn features(&self) -> Features {
            Features::empty()
        }

        fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()> {
            self.render(io)
        }

        fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()> {
            self.render(io)
        }
    }

    impl Faulty {
        fn render<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
            let out = &mut *io.out;
            self.blocks += 1;
            let fails = match &mut self.mode {
                Mode::Fail => true,
                Mode::Flood => false,
                Mode::Alternate => self.blocks % 2 == 1,
                Mode::Owned(errors) => {
                    return Err(errors.pop().expect("an error for every block"));
                }
            };
            if fails {
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
        fn poll(&mut self) -> bool {
            false
        }
        fn reset(&mut self) {}
        fn set_features(&mut self, _: Features) {}
        fn has_processor(&mut self) -> bool {
            true
        }
        fn flush_parameters(&mut self, _: &ParameterChanges) -> PluginResult<()> {
            Ok(())
        }
    }

    /// The MIDI events a flooding plugin emits in a block: past the cap.
    const FLOOD: usize = MIDI_STACK_CAPACITY + 7;
    const FRAMES: usize = 64;

    /// A render side with a stereo slab and `plugin` seated, and the host's
    /// end of the slab.
    fn seated(name: &str, plugin: Faulty) -> (RenderLink, RenderSide, impl Sized) {
        let (mut link, mut side) = link().expect("bells");
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

    fn block(seq: u64) -> AudioRequest {
        AudioRequest::ProcessAudio(ProcessAudioData {
            seq,
            num_samples: FRAMES,
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        })
    }

    /// Fills the render side's queue to the main thread until it refuses, and
    /// returns what it refused doing so.
    fn fill_back_queue(side: &mut RenderSide, link: &RenderLink) -> u64 {
        for _ in 0..=COMMANDS {
            side.give_back(Back::Ack(0));
        }
        let refused = link.losses().back_refused;
        assert!(refused > 0, "the queue took more than it holds");
        refused
    }

    /// **The render thread never calls `tracing`**, whatever goes wrong: a
    /// plugin that fails a block, a block refused for its length, for a rate
    /// or for want of a plugin, a MIDI flood past the cap, a hand-back the
    /// full queue to the main thread refuses (from `give_back`, from a failure
    /// and from a replaced seat). What was lost is counted, and the main
    /// thread logs it.
    #[test]
    fn nothing_that_goes_wrong_in_a_block_logs_on_the_render_thread() {
        let events = Arc::new(AtomicUsize::new(0));
        let on_render_thread = |f: &mut dyn FnMut()| {
            let before = events.load(Ordering::Relaxed);
            f();
            assert_eq!(
                events.load(Ordering::Relaxed),
                before,
                "the render thread logged"
            );
        };
        tracing::subscriber::with_default(CountEvents(Arc::clone(&events)), || {
            let (mut link, mut side, _host) = seated("quiet", Faulty::new(Mode::Fail));
            let mut filled = 0;
            on_render_thread(&mut || filled = fill_back_queue(&mut side, &link));
            // A plugin that fails: each failure's cause would go home, and
            // the queue refuses it (failures 1, 2 and 4).
            for seq in 1..=5 {
                on_render_thread(&mut || {
                    side.handle(block(seq)).expect("a failed block is answered");
                });
            }
            // Past the activated length: refused.
            on_render_thread(&mut || {
                side.handle(AudioRequest::ProcessAudio(ProcessAudioData {
                    seq: 6,
                    num_samples: FRAMES + 1,
                    ..ProcessAudioData::empty(SampleRate::SR_48K)
                }))
                .expect("a refused block is answered");
            });
            // A plugin that floods, in place of the one that failed: the
            // replaced seat goes home, and the queue refuses it.
            assert!(
                link.to
                    .push(ToRender::Seat(Box::new(Faulty::new(Mode::Flood))))
                    .is_ok()
            );
            on_render_thread(&mut || side.drain());
            on_render_thread(&mut || {
                let reply = side.handle(block(7)).expect("answered").expect("a reply");
                assert!(matches!(
                    reply,
                    AudioReply::AudioProcessed { ref midi_out, .. } if midi_out.len() == MIDI_STACK_CAPACITY
                ));
            });
            // A rate the plugin refused.
            assert!(
                link.to
                    .push(ToRender::Config(RenderConfig {
                        max_block: FRAMES,
                        inputs: 2,
                        outputs: 2,
                        rate_refused: true,
                        ..RenderConfig::new()
                    }))
                    .is_ok()
            );
            on_render_thread(&mut || side.drain());
            on_render_thread(&mut || {
                side.handle(block(8)).expect("answered");
            });
            // No plugin at all.
            let (link_none, mut none) = super::link().expect("bells");
            on_render_thread(&mut || {
                none.handle(block(1)).expect("answered");
            });
            assert_eq!(
                link_none.losses().refused_blocks,
                1,
                "no plugin is a refusal"
            );

            assert_eq!(
                link.losses(),
                Losses {
                    refused_blocks: 2,
                    failed_blocks: 5,
                    midi_dropped: 7,
                    back_refused: filled + 3 + 1,
                }
            );
            // The main thread says it once, with the totals.
            let before = events.load(Ordering::Relaxed);
            link.report_losses();
            assert_eq!(events.load(Ordering::Relaxed), before + 1);
            link.report_losses();
            assert_eq!(
                events.load(Ordering::Relaxed),
                before + 1,
                "once per change"
            );
        });
    }

    /// **A plugin whose error owns memory frees nothing on the render thread**:
    /// each of its blocks fails with `PluginError::Other(String)`, on blocks
    /// whose cause is not sent home and on blocks whose cause the full queue
    /// refuses, all under the allocation gate (which aborts on a free).
    #[test]
    fn a_failure_that_owns_memory_is_not_freed_on_the_render_thread() {
        const BLOCKS: usize = 12;
        let errors = (0..BLOCKS)
            .map(|i| PluginError::Other(format!("the plugin failed block {i}")))
            .collect();
        let (link, mut side, _host) = seated("owned_errors", Faulty::new(Mode::Owned(errors)));
        let filled = fill_back_queue(&mut side, &link);
        assert_no_alloc::assert_no_alloc(|| {
            for seq in 1..=BLOCKS as u64 {
                let reply = side.handle(block(seq));
                assert!(reply.is_ok(), "a failed block is answered");
            }
        });
        let losses = link.losses();
        assert_eq!(losses.failed_blocks, BLOCKS as u64);
        // Failures 1, 2, 4 and 8 are due home, and the queue is full.
        assert_eq!(losses.back_refused, filled + 4);
    }

    /// **The cause of the first block a plugin fails, and of each that makes a
    /// power of two, goes home to be logged**; the others are counted only, so
    /// a plugin failing every block is not a flood of hand-backs.
    #[test]
    fn a_failed_blocks_cause_goes_home_on_a_doubling_schedule() {
        let (mut link, mut side, _host) = seated("failing_causes", Faulty::new(Mode::Fail));
        // The seat and slab hand-backs of the setup are home already.
        while link.back.pop().is_some() {}
        for seq in 1..=9 {
            side.handle(block(seq)).expect("answered");
        }
        let mut causes = 0;
        while let Some(back) = link.back.pop() {
            assert!(matches!(back, Back::Failure(BlockFault(_))));
            causes += 1;
        }
        // Failures 1, 2, 4 and 8.
        assert_eq!(causes, 4);
        assert_eq!(link.losses().failed_blocks, 9);
    }

    /// **A run that stops at 3 reports 3**: the tick logs the counts when they
    /// first appear and each time they double, and the teardown logs the final
    /// counts if they are not what was last logged.
    #[test]
    fn the_final_counts_are_reported_when_they_differ_from_the_last_report() {
        let events = Arc::new(AtomicUsize::new(0));
        tracing::subscriber::with_default(CountEvents(Arc::clone(&events)), || {
            let (mut link, mut side, _host) = seated("final", Faulty::new(Mode::Fail));
            for seq in 1..=2 {
                side.handle(block(seq)).expect("answered");
            }
            // The causes of failures 1 and 2, and the counts at 2.
            link.report_losses();
            let after_two = events.load(Ordering::Relaxed);
            assert_eq!(after_two, 3);
            side.handle(block(3)).expect("answered");
            // 3 is not twice 2, and failure 3's cause is not sent home.
            link.report_losses();
            assert_eq!(events.load(Ordering::Relaxed), after_two);
            link.report_final_losses();
            assert_eq!(
                events.load(Ordering::Relaxed),
                after_two + 1,
                "the final 3 is reported"
            );
            link.report_final_losses();
            assert_eq!(events.load(Ordering::Relaxed), after_two + 1, "once");
        });
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

    impl Transport<AudioRequest, AudioReply> for Replay {
        fn recv(&mut self) -> BridgeResult<AudioRequest> {
            self.frames.read(&mut self.request.as_slice())
        }

        fn wait_readable(&mut self, _: Option<Duration>, _: &Bell) -> BridgeResult<bool> {
            Ok(true)
        }

        fn send(&mut self, msg: &AudioReply) -> BridgeResult<()> {
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
        let (mut link, mut side, _host) = seated("step_alloc", Faulty::new(Mode::Alternate));
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
        write_frame(&mut request, &AudioRequest::ProcessAudio(block)).unwrap();
        let mut io: Box<AudioTransport> = Box::new(Replay {
            frames: FrameBuf::new(request.len(), AUDIO_REPLY_BYTES),
            request,
            sink: vec![0; AUDIO_REPLY_BYTES + 4].into_boxed_slice(),
            replies: 0,
        });
        // Half the blocks fail and half flood, so the failure causes and the
        // barrier's acknowledgement are pushed home inside the gate too.
        const STEPS: usize = 16;
        let barrier = link.barrier();
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..STEPS {
                side.step(Some(&mut io)).unwrap();
            }
        });
        assert!(
            link.wait_applied(barrier, Duration::ZERO),
            "the ack came home"
        );
        let losses = link.losses();
        assert_eq!(losses.midi_dropped, 7 * (STEPS as u64 / 2));
        assert_eq!(losses.failed_blocks, STEPS as u64 / 2);
        assert_eq!(losses.refused_blocks, 0);
        assert_eq!(losses.back_refused, 0);
    }
}
