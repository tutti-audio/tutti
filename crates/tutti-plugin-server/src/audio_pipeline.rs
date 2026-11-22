//! One audio block: shared-memory in, plugin, shared-memory out.
//!
//! Everything the audio path owns lives here: the format-tagged
//! scratch buffers and the `process`
//! driver that takes an explicit `&mut dyn Render` + `&mut SlabWriter`
//! so the server doesn't have to weave field borrows.
//!
//! Input/output cross this layer as value types ([`AudioBlock`],
//! [`AudioOutput`]). The server decodes `AudioRequest::ProcessAudio` into an
//! `AudioBlock` and builds the wire `AudioReply::AudioProcessed` from the
//! resulting `AudioOutput`.

use crate::served::Render;
use tutti_buffers::FloatWidth;
use tutti_buffers::{Float, InputOutput};
use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_plugin_ipc::{ProcessAudioData, SlabLayout, SlabWriter};
use tutti_plugin_types::{
    BlockStatus, ChannelTable, ChordChanges, ExpressiveContext, Features, MAX_BLOCK_CHANNELS,
    MIDI_STACK_CAPACITY, MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParameterChanges, PluginError, PluginIo, PluginResult,
    ProcessContext, ProcessOutput, Samples, ScaleChanges, TransportChange, TransportInfo, WireMidi,
    sanitize, silence,
};
use tutti_rt::TallyWriter;
use tutti_ump::Ump;

/// The negotiated sample format.
///
/// Fixed at plugin load, so the audio path reads it without synchronisation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Clock {
    /// The format negotiated at load — `F64` only when the host asked for it
    /// *and* the plugin advertised `F64_AUDIO`. Selects the scratch variant.
    pub format: FloatWidth,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            format: FloatWidth::F32,
        }
    }
}

/// One channel's samples, in fixed storage.
type Channel<T> = Box<[T]>;

/// The channels of one direction: a fixed shape, never pushed to.
type Channels<T> = Box<[Channel<T>]>;

/// Per-channel scratch, exactly one sample format at a time.
///
/// Built by the control side for a slab's layout ([`for_layout`](Self::for_layout))
/// and shipped to the render thread, which only borrows it: it has no way to
/// grow, so a block never sizes it.
///
/// An enum rather than both buffers side by side: a session runs at one
/// negotiated format for the life of a plugin, so holding the unused width would
/// double the resident scratch for no reachable case.
pub(crate) enum AudioScratch {
    /// `f32` scratch, one channel per channel in each direction.
    F32 {
        /// Input scratch, in flat bus order as the plugin's `process` expects.
        input: Channels<f32>,
        /// Output scratch, zeroed before each block.
        output: Channels<f32>,
    },
    /// `f64` scratch, used only when the plugin advertised `F64_AUDIO` and the
    /// host asked for it.
    F64 {
        /// Input scratch, in flat bus order as the plugin's `process` expects.
        input: Channels<f64>,
        /// Output scratch, zeroed before each block.
        output: Channels<f64>,
    },
}

impl AudioScratch {
    /// No channels: what a pipeline holds until a slab is attached.
    /// Allocates nothing.
    pub(crate) fn empty(format: FloatWidth) -> Self {
        Self::new(format, 0, Samples::new(0))
    }

    /// Scratch in `format` for `channels` channels of `frames` frames each, in
    /// both directions. Allocates: control side.
    pub(crate) fn new(format: FloatWidth, channels: usize, frames: Samples) -> Self {
        fn shaped<T: Default + Clone>(channels: usize, frames: Samples) -> Channels<T> {
            (0..channels)
                .map(|_| vec![T::default(); frames.get()].into_boxed_slice())
                .collect()
        }
        match format {
            FloatWidth::F64 => Self::F64 {
                input: shaped(channels, frames),
                output: shaped(channels, frames),
            },
            FloatWidth::F32 => Self::F32 {
                input: shaped(channels, frames),
                output: shaped(channels, frames),
            },
        }
    }

    /// Scratch in `format` for the blocks `layout`'s slab holds: its widest
    /// direction, at its per-channel room. Allocates: control side.
    pub(crate) fn for_layout(format: FloatWidth, layout: &SlabLayout) -> Self {
        Self::new(
            format,
            layout.input_channels().max(layout.output_channels()),
            Samples::new(layout.samples_per_channel),
        )
    }

    /// Whether a block of `frames` over `channels` channels fits.
    pub(crate) fn fits(&self, channels: usize, frames: Samples) -> bool {
        fn holds<T>(bufs: &[Channel<T>], channels: usize, frames: Samples) -> bool {
            bufs.len() >= channels && bufs.iter().all(|chan| chan.len() >= frames.get())
        }
        match self {
            Self::F32 { input, output } => {
                holds(input, channels, frames) && holds(output, channels, frames)
            }
            Self::F64 { input, output } => {
                holds(input, channels, frames) && holds(output, channels, frames)
            }
        }
    }
}

/// Per-block ancillary data a block carries besides its audio and MIDI.
///
/// Every field but `param_changes` is forwarded to the plugin **only if it
/// advertised the matching `Features` flag** — the engine hands a plugin what it
/// asked to consume. `param_changes` is the exception and goes through
/// unconditionally; see `AudioPipeline::process` for why.
#[derive(Debug)]
pub(crate) struct ProcessExtras<'a> {
    /// Sample-accurate parameter automation for this block. Forwarded
    /// unconditionally, not gated on `PARAM_AUTOMATION`.
    pub param_changes: &'a ParameterChanges,
    /// Per-note expression (pressure, timbre, pitch bend). Gated on
    /// `Features::NOTE_EXPRESSION`.
    pub note_expression: &'a NoteExpressionChanges,
    /// Chord context for this block. Gated on `Features::SEQUENCER_CONTEXT`.
    pub chords: &'a ChordChanges,
    /// Scale context for this block. Gated on `Features::SEQUENCER_CONTEXT`.
    pub scales: &'a ScaleChanges,
    /// Text-valued note expression. Gated on `Features::SEQUENCER_CONTEXT`.
    pub expr_texts: &'a NoteExpressionTextChanges,
    /// Integer-valued note expression. Gated on `Features::SEQUENCER_CONTEXT`.
    pub expr_ints: &'a NoteExpressionIntChanges,
    /// Tempo, time signature, playhead and loop region. Gated on
    /// `Features::TRANSPORT`.
    pub transport: &'a TransportInfo,
    /// The transport changes inside the block, each at its frame. Gated on
    /// `Features::TRANSPORT`; only a format with an inter-block transport event
    /// (CLAP) reads them.
    pub transport_changes: &'a [TransportChange],
}

impl<'a> ProcessExtras<'a> {
    /// The extras `data`, a block's request, carries.
    pub(crate) fn of(data: &'a ProcessAudioData) -> Self {
        Self {
            param_changes: &data.param_changes,
            note_expression: &data.note_expression,
            chords: &data.chords,
            scales: &data.scales,
            expr_texts: &data.expr_texts,
            expr_ints: &data.expr_ints,
            transport: &data.transport,
            transport_changes: &data.transport_changes,
        }
    }
}

/// One inbound block: which block it is, how many samples, MIDI, and the extras.
pub(crate) struct AudioBlock<'a> {
    /// Which block this is. **Not opaque to the server**: it selects the slab
    /// ring slot to read the inputs from and to publish the outputs into, so the
    /// server both interprets it and checks it. Echoed in the `AudioProcessed`
    /// reply as well.
    pub seq: u64,
    /// Frames in this block, per channel — never a sample count. The scratch
    /// buffers and every slab read/write below are sized from it.
    pub num_samples: usize,
    /// MIDI arriving with this block, already in the host's event order.
    ///
    /// Raw wire offsets: they become `Offset`s of this block's `num_samples`
    /// where the plugin's context is built ([`block_midi`]).
    pub midi: &'a [WireMidi],
    /// The block's automation, note expression and transport.
    pub extras: ProcessExtras<'a>,
}

/// One processed block's result. The plugin's audio output is written back into
/// the shared slab in place; the measured latency travels onward to the host,
/// and the plugin's emitted MIDI is read from
/// [`AudioPipeline::midi_out`]. (Parameter output is dropped — no host
/// consumer.)
///
/// The MIDI is **not** carried here. It lives in the pipeline's own reusable
/// [`ProcessOutput`], and moving it into this struct each block would drop that
/// buffer's capacity — reintroducing the per-block allocation on the audio
/// thread that owning it cost in the first place.
#[derive(Default)]
pub(crate) struct AudioOutput {
    /// Wall-clock microseconds this block spent inside the plugin, measured by
    /// the server. A *diagnostic*, not the plugin's reported PDC latency —
    /// that arrives as a `News::LatencyChanged` in `Samples`.
    pub latency_us: u64,
    /// The block's audio was not published: its input slot was recycled, or an
    /// output channel was not written. The host reads it as silence.
    pub refused: bool,
    /// Why the plugin failed the block, when it did: a value with no drop
    /// glue, which the caller hands to the control side to log.
    pub failure: Option<BlockFault>,
}

/// Why a block failed, as a constant: nothing the render thread holds of a
/// failure owns memory, so neither keeping it nor losing it frees anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockFault(pub &'static str);

impl BlockFault {
    /// The fault `error` stands for: a block failure keeps its constant
    /// reason, any other variant is named by its kind.
    ///
    /// The format hosts' Processors fail a block with [`PluginError::Block`]
    /// alone, which owns nothing. A variant that owns heap memory is a
    /// Processor breaking that contract; it is not dropped here (this thread
    /// frees nothing), and leaks instead, once per failed block and without a
    /// cap for as long as the Processor keeps breaking it.
    #[allow(
        clippy::mem_forget,
        reason = "a free is not allowed on the render thread"
    )]
    pub(crate) fn of(error: PluginError) -> Self {
        let fault = Self(match &error {
            PluginError::Block(reason) => reason,
            PluginError::Load { .. } => "the plugin failed the block: a load error",
            PluginError::State(_) | PluginError::StateFailed(_) => {
                "the plugin failed the block: a state error"
            }
            PluginError::Process(_) => "the plugin failed the block: a process error",
            PluginError::Editor(_) => "the plugin failed the block: an editor error",
            PluginError::Bridge(_) => "the plugin failed the block: a bridge error",
            PluginError::Other(_) => "the plugin failed the block",
        });
        std::mem::forget(error);
        fault
    }
}

/// Driver for one audio block. Holds scratch across calls so the audio
/// thread never allocates: the scratch and the plugin's output are built by
/// the control side and installed whole ([`install_scratch`](Self::install_scratch),
/// [`install_output`](Self::install_output)).
pub(crate) struct AudioPipeline {
    scratch: Box<AudioScratch>,
    /// The plugin's non-audio outputs for the current block.
    ///
    /// Owned here, and handed to `Render::render` as an out-parameter,
    /// so its storage persists across blocks. A loader that copies
    /// out of a borrowing `ProcessOutputRef` copies into *this*, which is what
    /// makes the copy allocation-free once warm.
    ///
    /// Read back through [`midi_out`](Self::midi_out) after each
    /// [`process`](Self::process); it is cleared at the top of the next one.
    plugin_output: Box<ProcessOutput>,
    /// What the plugin said of itself at the end of the last block, as the
    /// reply carries it ([`status`](Self::status)).
    status: BlockStatus,
    /// Blocks the plugin failed. A failed block publishes nothing (the host
    /// reads silence) and the session goes on.
    failed_blocks: TallyWriter<u64>,
}

impl AudioPipeline {
    /// A pipeline with no scratch and an empty output, counting the blocks the
    /// plugin fails into `failed_blocks`. Control side.
    pub(crate) fn new(failed_blocks: TallyWriter<u64>) -> Self {
        Self {
            scratch: Box::new(AudioScratch::empty(FloatWidth::F32)),
            plugin_output: Box::default(),
            status: BlockStatus::Continue,
            failed_blocks,
        }
    }

    /// The MIDI the plugin emitted during the most recent
    /// [`process`](Self::process).
    ///
    /// Borrowed, not moved: see [`AudioOutput`]. Valid until the next
    /// `process`, which clears it.
    pub(crate) fn midi_out(&self) -> &MidiEventVec {
        &self.plugin_output.midi_events
    }

    /// What the plugin said of itself at the end of the most recent
    /// [`process`](Self::process): a sleep only over a silent block.
    pub(crate) fn status(&self) -> BlockStatus {
        self.status
    }

    /// The MIDI events the most recent [`process`](Self::process) dropped
    /// past the list's cap.
    pub(crate) fn midi_dropped(&self) -> usize {
        self.plugin_output.midi_dropped()
    }

    /// Installs `output`, the plugin's output reserved for its parameters
    /// ([`ProcessOutput::reserve_params`]), and returns the one it replaces,
    /// for the control side to free. Moves two boxes: allocation-free.
    pub(crate) fn install_output(&mut self, output: Box<ProcessOutput>) -> Box<ProcessOutput> {
        std::mem::replace(&mut self.plugin_output, output)
    }

    /// The parameter queues the plugin's output holds, for the session's
    /// tests of when it is re-reserved.
    #[cfg(test)]
    pub(crate) fn param_queues(&self) -> usize {
        self.plugin_output.param_changes.capacity()
    }

    /// Installs `scratch` and returns the one it replaces, for the control
    /// side to free. Moves two boxes: allocation-free.
    pub(crate) fn install_scratch(&mut self, scratch: Box<AudioScratch>) -> Box<AudioScratch> {
        std::mem::replace(&mut self.scratch, scratch)
    }

    /// Whether the scratch holds a block of `frames` over `channels` channels.
    pub(crate) fn fits(&self, channels: usize, frames: Samples) -> bool {
        self.scratch.fits(channels, frames)
    }

    /// Read input from `shm`, run `plugin`, write output back. Returns the
    /// plugin's MIDI output plus the measured processing latency.
    ///
    /// # Real-time
    ///
    /// Runs on the realtime audio thread and does not allocate: the scratch is
    /// built for the slab by the control side ([`install_scratch`](Self::install_scratch))
    /// and never grows, a block longer than it holds is refused, and the slice
    /// tables handed to the plugin are fixed stack arrays. Denormals are
    /// flushed for the duration of the block. Pinned by
    /// `tests::process_is_alloc_free` and
    /// `tests::a_shorter_block_after_a_longer_one_does_not_allocate`.
    ///
    /// # Failure is silence, never wrong audio
    ///
    /// Three separate checks all resolve the same way — the output slot goes
    /// unpublished, the host's sequence check fails, and it substitutes silence:
    ///
    /// - the input slot no longer holds this block (the host recycled it), so
    ///   inputs are zeroed rather than read from a foreign block;
    /// - any channel's write into the slab failed, leaving the slot part-filled;
    /// - the block returned early.
    ///
    /// Non-finite samples are a fourth case, handled differently: they are
    /// zeroed per sample rather than dropping the block, because a plugin
    /// emitting one NaN would otherwise poison the whole downstream graph.
    ///
    /// A block the plugin's `process` fails is counted, leaves its MIDI empty and
    /// publishes nothing, so the host reads it as silence and the server goes on
    /// to the next block. A slab read or write failure is absorbed into the
    /// silence path above.
    pub(crate) fn process(
        &mut self,
        plugin: &mut dyn Render,
        shm: &mut SlabWriter,
        block: AudioBlock<'_>,
    ) -> AudioOutput {
        let num_samples = block.num_samples;
        let seq = block.seq;
        // Per-direction widths. Each direction has its own slab region, so there
        // is no base to compute and no branch on bus count.
        // Borrow (don't clone) the layout: the bus list is heap-backed and this
        // is the RT path. Copy the widths out before the scratch loop so the
        // slab is free to be borrowed for read/write.
        let layout = shm.layout_ref();
        let (total_in, total_out) = (layout.input_channels(), layout.output_channels());

        // Does the input ring actually hold this block? A mismatch means the
        // host has moved on and recycled the slot — the bytes there belong to a
        // different block. Feed silence rather than a foreign block's audio: a
        // stateful plugin fed the wrong input produces plausible-sounding wrong
        // output, which is much harder to notice than a gap.
        let inputs_ready = shm.has_input(seq);
        // Scratch holds each direction at full width; the input scratch is the
        // flat, bus-ordered input view handed to the plugin's `process` (the
        // host crate splits it back into per-bus `AudioBusBuffers`).
        let num_channels = total_in.max(total_out);
        if !self.scratch.fits(num_channels, Samples::new(num_samples)) {
            // A block the scratch cannot hold reads silence; the render side
            // checks this before it gets here (`RenderSide::block_fits`).
            self.plugin_output.clear();
            return AudioOutput {
                latency_us: 0,
                refused: true,
                failure: None,
            };
        }

        // Flush denormals to zero for the duration of this block. A plugin
        // whose tail decays toward silence can otherwise emit denormalized
        // floats and spike CPU. The mode is restored when the closure ends.
        tutti_platform::with_no_denormals(move || {
            let start = tutti_platform::Instant::now();

            // Gate each best-effort input on the plugin's advertised features — the
            // engine only hands a plugin what it asked to consume, keyed on the
            // flag, never on the plugin's format. (The host already gates its sends
            // the same way; gating here keeps the server-side context honest even if
            // an over-eager payload arrives.)
            //
            // Parameter automation is the exception: the host sends it *universally*
            // (every plugin has automatable params, regardless of whether it
            // advertises `PARAM_AUTOMATION`), so it
            // is forwarded unconditionally to match. Gating it on `PARAM_AUTOMATION`
            // here would silently drop automation for any plugin that leaves that
            // optional flag unset — host says send, server throws it away.
            let features = plugin.features();
            let mut timed = [(Offset::ZERO, Ump([0; 4])); MIDI_STACK_CAPACITY];
            let timed_len = block_midi(block.midi, num_samples, &mut timed);
            let mut ctx = ProcessContext::new().midi(&timed[..timed_len]);
            let ex = &block.extras;
            ctx = ctx.params(ex.param_changes);
            if features.contains(Features::NOTE_EXPRESSION) {
                ctx = ctx.note_expression(ex.note_expression);
            }
            if features.contains(Features::TRANSPORT) {
                ctx = ctx
                    .transport(ex.transport)
                    .transport_changes(ex.transport_changes);
            }
            if features.contains(Features::SEQUENCER_CONTEXT) {
                ctx = ctx.expressive(ExpressiveContext {
                    chords: Some(ex.chords),
                    scales: Some(ex.scales),
                    expr_texts: Some(ex.expr_texts),
                    expr_ints: Some(ex.expr_ints),
                });
            }

            // The widths are the slab's, and a slab is only made from a layout
            // `check_layout` bounded by `MAX_BLOCK_CHANNELS` per direction
            // (`AudioSlab::create`/`open`), so they fit the slice tables.
            debug_assert!(
                num_channels <= MAX_BLOCK_CHANNELS,
                "channel count > {MAX_BLOCK_CHANNELS}"
            );
            let (in_n, out_n) = (total_in, total_out);
            // Set false by any channel whose write into the slab failed. Publishing
            // a slot whose later channels never landed is worse than not publishing
            // at all: the host's sequence check would pass and it would read the
            // previous occupant's samples in those channels — half this block
            // spliced onto half another, which sounds almost right.
            let mut all_channels_written = true;
            // Split the two fields apart so the render arms can hold `buffers`
            // mutably while also handing `plugin_output` to the plugin. Cleared
            // once, here: `process` fills it and every early return below leaves it
            // empty rather than carrying the previous block's events.
            // Whether every output sample of the block is exactly zero: a
            // plugin's *sleep* is believed only over silence.
            let mut silent = true;
            let mut failure = None;
            self.status = BlockStatus::Continue;
            let Self {
                scratch,
                plugin_output,
                failed_blocks,
                ..
            } = self;
            plugin_output.clear();
            match &mut **scratch {
                AudioScratch::F32 { input, output } => {
                    // Read every input bus's channels from this block's input slot.
                    // The flat order IS bus order — the host crate splits it back
                    // into per-bus `AudioBusBuffers`.
                    for (ch, chan) in input.iter_mut().enumerate().take(in_n) {
                        if inputs_ready {
                            let _ = shm.read_input_into::<f32>(seq, ch, &mut chan[..num_samples]);
                        } else {
                            chan[..num_samples].fill(0.0);
                        }
                    }
                    silence(&mut output[..out_n], num_samples);
                    // Stack-allocated slice tables — no per-block Vec::collect.
                    let processed = render_block(
                        plugin,
                        &input[..in_n],
                        &mut output[..out_n],
                        num_samples,
                        &ctx,
                        plugin_output,
                        |plugin, io| plugin.render_f32(io),
                    );
                    if let Err(error) = processed {
                        failed_blocks.add(1);
                        failure = Some(error);
                        plugin_output.clear();
                        all_channels_written = false;
                    } else {
                        // Sanitize before it leaves the subprocess: a misbehaving
                        // plugin can emit NaN/Inf that would otherwise poison the
                        // entire downstream graph. Unconditional — this is a
                        // production hazard, and a finite-check per sample is cheap
                        // on an already memory-bound path.
                        for (ch, chan) in output.iter_mut().enumerate().take(out_n) {
                            sanitize(&mut chan[..num_samples]);
                            silent &= chan[..num_samples].iter().all(|s| *s == 0.0);
                            if shm.write::<f32>(seq, ch, &chan[..num_samples]).is_err() {
                                all_channels_written = false;
                            }
                        }
                    }
                }
                AudioScratch::F64 { input, output } => {
                    for (ch, chan) in input.iter_mut().enumerate().take(in_n) {
                        if inputs_ready {
                            let _ = shm.read_input_into::<f64>(seq, ch, &mut chan[..num_samples]);
                        } else {
                            chan[..num_samples].fill(0.0);
                        }
                    }
                    silence(&mut output[..out_n], num_samples);
                    let processed = render_block(
                        plugin,
                        &input[..in_n],
                        &mut output[..out_n],
                        num_samples,
                        &ctx,
                        plugin_output,
                        |plugin, io| plugin.render_f64(io),
                    );
                    if let Err(error) = processed {
                        failed_blocks.add(1);
                        failure = Some(error);
                        plugin_output.clear();
                        all_channels_written = false;
                    } else {
                        // Sanitize NaN/Inf — see the F32 arm above.
                        for (ch, chan) in output.iter_mut().enumerate().take(out_n) {
                            sanitize(&mut chan[..num_samples]);
                            silent &= chan[..num_samples].iter().all(|s| *s == 0.0);
                            if shm.write::<f64>(seq, ch, &chan[..num_samples]).is_err() {
                                all_channels_written = false;
                            }
                        }
                    }
                }
            }

            // What the plugin said of itself at the end of the block, for the
            // reply. A sleep is sent only over a silent
            // block with no MIDI-out, because the host parks its node on it
            // and the executor writes silence for a parked node. A block the
            // plugin failed said nothing (`plugin_output` was cleared).
            self.status = match self.plugin_output.status() {
                BlockStatus::Sleep
                    if silent
                        && self.plugin_output.midi_events.is_empty()
                        && all_channels_written =>
                {
                    BlockStatus::Sleep
                }
                BlockStatus::Tail => BlockStatus::Tail,
                _ => BlockStatus::Continue,
            };

            // Publish exactly once, after every output channel is in place. Until
            // this store the host's sequence check fails and it emits silence — so a
            // block that returned early above (an error path, no plugin loaded)
            // correctly produces silence rather than stale audio, without needing to
            // signal that separately.
            //
            // Two conditions gate it, and both are about *not writing into a slot
            // that belongs to someone else*:
            //
            // - `inputs_ready` false means the host recycled this block's slot
            //   before this point, so it has moved on and this reply is for a block
            //   nobody is waiting for. `MAX_BEHIND` in the host's `dispatch` should
            //   already have dropped the command, but that is the host's bound on
            //   *sending*; this is the server's own check on *writing*, and the two
            //   failed independently once already.
            // - `all_channels_written` false means the slot is only partly filled.
            //
            // Either way the host reads a stale sequence and substitutes silence,
            // which is the designed failure mode.
            let published = inputs_ready && all_channels_written;
            if published {
                shm.publish(seq);
            }

            // The plugin's MIDI-out travels back to the host so it can re-enter
            // routing, but it is *left in* `plugin_output` and read through
            // `midi_out()` — moving it out here would take the buffer's capacity
            // with it. `param_changes` are not sent to the host (the wire has no
            // return path for them until the ParamEdit event), but a VST3 plugin's reach the
            // server's own edit controller:
            // its Processor hands each block's `outputParameterChanges` to the
            // Controller, which applies them with `setParamNormalized` on the
            // tick, so a `getState` or an editor reads them. `note_expression`
            // is still dropped (no host consumer yet). Audio was written back
            // into the slab above.
            AudioOutput {
                latency_us: start.elapsed().as_micros() as u64,
                refused: !published,
                failure,
            }
        })
    }
}

/// The wire's MIDI as the `(Offset, Ump)` list of a block of `frames`, into
/// `out`; returns how many it wrote. The host never sends an offset outside
/// its block, and an `Offset` cannot hold one: a stray one is clamped to the
/// block's last frame (the one behaviour difference from passing it through).
/// A block of no frames holds no offset, so its MIDI could not be placed and
/// would be dropped; tutti's host never sends one (a graph block holds at least
/// one frame, and every chunk the host ships is made of them), and a debug
/// build says so. Allocates nothing.
fn block_midi(midi: &[WireMidi], frames: usize, out: &mut [UmpAt; MIDI_STACK_CAPACITY]) -> usize {
    debug_assert!(
        frames > 0 || midi.is_empty(),
        "a process block of 0 frames carries MIDI it cannot place"
    );
    if frames == 0 {
        return 0;
    }
    let block = Samples::new(frames);
    let mut n = 0;
    for e in midi.iter().take(MIDI_STACK_CAPACITY) {
        out[n] = (Offset::clamped(e.offset, block), e.ump);
        n += 1;
    }
    n
}

/// Renders one block of the scratch's channels through `plugin`. Allocation-free:
/// the slice tables are [`ChannelTable`]s, which is what makes this callable on
/// the audio thread.
fn render_block<S: Float>(
    plugin: &mut dyn Render,
    inputs: &[Channel<S>],
    outputs: &mut [Channel<S>],
    size: usize,
    ctx: &ProcessContext<'_>,
    out: &mut ProcessOutput,
    render: impl FnOnce(&mut dyn Render, &mut PluginIo<'_, S>) -> PluginResult<()>,
) -> Result<(), BlockFault> {
    // At most `MAX_BLOCK_CHANNELS`: the caller's slices are the slab's widths.
    let in_refs: ChannelTable<&[S]> = inputs.iter().map(|chan| &chan[..size]).collect();
    let mut out_refs: ChannelTable<&mut [S]> =
        outputs.iter_mut().map(|chan| &mut chan[..size]).collect();
    let buffer = InputOutput::from_slices(Samples::new(size), &in_refs, &mut out_refs)
        .map_err(|_| BlockFault("the block's channels differ in length"))?;
    render(plugin, &mut PluginIo { buffer, ctx, out }).map_err(BlockFault::of)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_units::ParamId;

    use tutti_plugin_types::{PluginTail, Samples};

    /// A pipeline whose scratch is sized for `shm`'s layout in `format`, as the
    /// control side sizes the render thread's.
    fn pipeline_for(shm: &SlabWriter, format: FloatWidth) -> AudioPipeline {
        let mut pipeline = AudioPipeline::new(tutti_rt::tally().writer());
        let _ =
            pipeline.install_scratch(Box::new(AudioScratch::for_layout(format, shm.layout_ref())));
        pipeline
    }

    /// **The scratch holds the widest direction at the slab's room, in the
    /// format's width, and a block fits it exactly up to those.**
    #[test]
    fn scratch_is_the_slabs_widest_direction_at_its_room_in_its_format() {
        let layout = SlabLayout::new(
            512,
            FloatWidth::F64,
            vec![ChannelLayout::STEREO],
            vec![ChannelLayout::STEREO, ChannelLayout::MONO],
        );
        let scratch = AudioScratch::for_layout(FloatWidth::F64, &layout);
        let AudioScratch::F64 { input, output } = &scratch else {
            panic!("the format is f64");
        };
        assert_eq!((input.len(), output.len()), (3, 3));
        assert!(input.iter().chain(output.iter()).all(|c| c.len() == 512));
        assert!(scratch.fits(3, Samples::new(512)));
        assert!(!scratch.fits(4, Samples::new(512)), "one channel too wide");
        assert!(!scratch.fits(3, Samples::new(513)), "one frame too long");
        assert!(AudioScratch::empty(FloatWidth::F32).fits(0, Samples::ZERO));
    }

    /// **A block the scratch cannot hold is refused, never sized for**: the
    /// pipeline has no way to grow its scratch, so an overlong block reads as
    /// silence and publishes nothing.
    #[test]
    fn a_block_past_the_scratch_is_refused_not_grown_for() {
        const N: usize = 64;
        let layout = test_layout(
            N,
            FloatWidth::F32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();
        let mut pipeline = AudioPipeline::new(tutti_rt::tally().writer());
        let mut plugin = NanPlugin {
            meta: meta(&[2], &[2]),
            fill: 0.25,
        };
        let block = AudioBlock {
            seq: 1,
            num_samples: N,
            midi: &[],
            extras: ProcessExtras::of(&QUIET),
        };
        let out = pipeline.process(&mut plugin, &mut shm, block);
        assert!(out.refused, "an empty scratch holds no block");
        assert!(!pipeline.fits(2, Samples::new(N)));
    }

    use tutti_plugin_ipc::SlabLayout;

    /// The request of a block with no MIDI, automation, expression or transport:
    /// what the blocks below carry as their extras.
    static QUIET: std::sync::LazyLock<ProcessAudioData> =
        std::sync::LazyLock::new(|| ProcessAudioData::empty(tutti_units::SampleRate::SR_48K));
    use tutti_plugin_types::{ChannelLayout, PluginResult, PluginWiring};

    /// A stand-in plugin whose `render` fills every output sample with a
    /// non-finite value. Used to prove the pipeline sanitizes plugin output
    /// before it reaches the shared-memory slab. Only its wiring and `render`
    /// are meaningful.
    struct NanPlugin {
        meta: PluginWiring,
        fill: f32,
    }

    impl Render for NanPlugin {
        fn features(&self) -> Features {
            self.meta.features
        }

        fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()> {
            self.render(io)
        }

        fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()> {
            self.render(io)
        }
    }

    impl NanPlugin {
        fn render<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
            for mut c in io.buffer.output().iter_mut() {
                c.samples_mut().fill(S::from_f32(self.fill));
            }
            Ok(())
        }
    }

    /// A stand-in plugin that fills its output with `fill` and says `status`.
    struct SaysPlugin {
        meta: PluginWiring,
        fill: f32,
        status: BlockStatus,
    }

    impl Render for SaysPlugin {
        fn features(&self) -> Features {
            self.meta.features
        }

        fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()> {
            self.render(io)
        }

        fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()> {
            self.render(io)
        }
    }

    impl SaysPlugin {
        fn render<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
            for mut chan in io.buffer.output().iter_mut() {
                chan.samples_mut().fill(S::from_f32(self.fill));
            }
            io.out.set_status(self.status);
            Ok(())
        }
    }

    /// The status the pipeline reports for a plugin that fills `fill` and says
    /// `status`.
    fn reported(fill: f32, status: BlockStatus) -> BlockStatus {
        const N: usize = 64;
        let layout = test_layout(
            N,
            FloatWidth::F32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();
        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let mut plugin = SaysPlugin {
            meta: meta(&[2], &[2]),
            fill,
            status,
        };
        let block = AudioBlock {
            seq: 1,
            num_samples: N,
            midi: &[],
            extras: ProcessExtras::of(&QUIET),
        };
        pipeline.process(&mut plugin, &mut shm, block);
        pipeline.status()
    }

    /// **The pipeline reports the plugin's status, and believes a sleep only
    /// over silence**: the host parks its node on a sleep and
    /// the executor writes silence for a parked node, so a plugin asleep over
    /// sound is reported as continuing. A tail passes through.
    #[test]
    fn a_sleep_is_reported_only_over_silence() {
        assert_eq!(reported(0.0, BlockStatus::Sleep), BlockStatus::Sleep);
        assert_eq!(reported(0.5, BlockStatus::Sleep), BlockStatus::Continue);
        assert_eq!(reported(0.5, BlockStatus::Tail), BlockStatus::Tail);
        assert_eq!(reported(0.5, BlockStatus::Continue), BlockStatus::Continue);
    }

    /// A stand-in plugin that records the first sample of every input channel
    /// it was handed (so a test can assert the pipeline delivered the right
    /// flat channels), then echoes each input channel into the matching output
    /// channel. Inputs beyond the output count are still recorded — that's how
    /// the sidechain bus is observed. Sample-erased via an interior-mutable
    /// `RefCell` so `render(&mut self)` can stash the seen values.
    struct EchoProbe {
        meta: PluginWiring,
        seen_inputs: std::cell::RefCell<Vec<f32>>,
    }

    impl Render for EchoProbe {
        fn features(&self) -> Features {
            self.meta.features
        }

        fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()> {
            self.render(io)
        }

        fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()> {
            self.render(io)
        }
    }

    impl EchoProbe {
        fn render<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
            let mut seen = self.seen_inputs.borrow_mut();
            seen.clear();
            let (input, mut output) = io.buffer.split();
            for chan in input.iter() {
                seen.push(chan.samples().first().map_or(0.0, |s| s.to_f32()));
            }
            // Echo input[c] -> output[c] for the channels both directions
            // share, so the test can read outputs back from the slab.
            for (c, mut out) in output.iter_mut().enumerate() {
                let v = input
                    .get_plane(c)
                    .and_then(|i| i.samples().first().copied())
                    .unwrap_or(S::ZERO);
                out.samples_mut().fill(v);
            }
            Ok(())
        }
    }

    use tutti_buffers::FloatWidth as SF;
    use tutti_plugin_ipc::{AudioSlab, RING_SLOTS};
    use tutti_plugin_types::PluginBus;

    /// A slab layout with real bus lists in both directions.
    ///
    /// Every test names its buses explicitly, and the slab rejects an empty
    /// list. Empty would put both directions at offset 0, where an output write
    /// landing on the input region is indistinguishable from correct behaviour.
    fn test_layout(
        samples: usize,
        format: SF,
        inputs: &[ChannelLayout],
        outputs: &[ChannelLayout],
    ) -> SlabLayout {
        SlabLayout {
            samples_per_channel: samples,
            format,
            slots: RING_SLOTS as u32,
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        }
    }

    /// The block number every test drives. Deliberately not 0: a zeroed slab
    /// reads back 0 for "nothing published", so a test using block 0 would have
    /// its sequence check pass by accident rather than because anyone published.
    const SEQ: u64 = 1;

    /// A test [`PluginWiring`] with the given per-bus input/output channel widths.
    fn meta(inputs: &[usize], outputs: &[usize]) -> PluginWiring {
        PluginWiring {
            inputs: inputs
                .iter()
                .map(|&c| {
                    PluginBus::main(ChannelLayout::try_from(c).expect("a test bus is narrow"))
                })
                .collect(),
            outputs: outputs
                .iter()
                .map(|&c| {
                    PluginBus::main(ChannelLayout::try_from(c).expect("a test bus is narrow"))
                })
                .collect(),
            latency_samples: Samples::ZERO,
            tail: PluginTail::Unknown,
            // This fixture is about bus widths; no capability is claimed,
            // and no speaker placement either.
            features: Features::empty(),
            probed: Features::empty(),
            ..PluginWiring::default()
        }
    }

    /// Multi-bus channel split: stereo main + mono sidechain in, stereo out.
    /// The pipeline must hand the plugin all THREE input channels (incl. the
    /// sidechain) in flat bus order, and write the echoed outputs into the
    /// output region — never touching the sidechain input, which lives in a
    /// different region entirely now rather than merely at a higher offset.
    #[test]
    fn process_splits_multibus_channels() {
        const N: usize = 32;
        // total_in = 3, total_out = 2 → 5 flat channels.
        // stereo main + mono sidechain in, stereo out.
        let layout = test_layout(
            N,
            SF::F32,
            &[ChannelLayout::STEREO, ChannelLayout::MONO],
            &[ChannelLayout::STEREO],
        );
        let (mut host, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        // Write a distinct marker into each of the 3 input channels, then
        // publish — without the publish the pipeline correctly refuses to read
        // the region and the plugin would see silence.
        let markers = [11.0f32, 22.0, 33.0];
        for (ch, &m) in markers.iter().enumerate() {
            host.write::<f32>(SEQ, ch, &[m; N]).unwrap();
        }
        host.publish(SEQ);

        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let mut plugin = EchoProbe {
            meta: meta(&[2, 1], &[2]),
            seen_inputs: std::cell::RefCell::new(Vec::new()),
        };
        pipeline.process(
            &mut plugin,
            &mut shm,
            AudioBlock {
                seq: SEQ,
                num_samples: N,
                midi: &[],
                extras: ProcessExtras::of(&QUIET),
            },
        );

        // The plugin saw all three input channels in flat bus order, including
        // the mono sidechain (bus 1).
        assert_eq!(plugin.seen_inputs.borrow().as_slice(), &markers);

        // The server published this block's outputs, so the host-side check
        // would pass.
        assert!(shm.has_output(SEQ), "the pipeline must publish its outputs");

        // Outputs (echo of input ch 0,1) landed in the OUTPUT region, indexed
        // from 0 within it, and the sidechain input was NOT clobbered.
        let mut out0 = vec![0.0f32; N];
        let mut out1 = vec![0.0f32; N];
        shm.read_output_into::<f32>(SEQ, 0, &mut out0).unwrap();
        shm.read_output_into::<f32>(SEQ, 1, &mut out1).unwrap();
        assert!(
            out0.iter().all(|&s| s == 11.0),
            "output bus ch0 == main in ch0"
        );
        assert!(
            out1.iter().all(|&s| s == 22.0),
            "output bus ch1 == main in ch1"
        );

        let mut sc = vec![0.0f32; N];
        shm.read_input_into::<f32>(SEQ, 2, &mut sc).unwrap();
        assert!(
            sc.iter().all(|&s| s == 33.0),
            "sidechain input survived the output write"
        );
    }

    /// A block whose input slot the host already recycled must not have its
    /// output published.
    ///
    /// This is the server half of the ring-slot invariant, and it is deliberately
    /// redundant with the host's `MAX_BEHIND` bound. The host stops *sending* such
    /// a block; this stops the server *writing* one if it ever arrives anyway.
    ///
    /// Why both: at ring depth 2, `slot_for(seq)` and `slot_for(seq + 2)` are the
    /// same slot. `MAX_BEHIND` was `RING_SLOTS` rather than `RING_SLOTS - 1`, so a
    /// block exactly two behind was admitted, and publishing its output would
    /// stamp the sequence of the slot the host was concurrently reading for the
    /// newest block — tearing that block, or destroying the evidence for it. One
    /// bound guarding a shared slot is a single point of failure, and that single
    /// point is what shipped.
    ///
    /// The stale slot here holds real audio, not zeros. If it held zeros the test
    /// could not tell "correctly withheld" from "published silence" — the trap
    /// that made a sibling test on the host side vacuous.
    #[test]
    fn a_block_whose_input_slot_was_recycled_is_not_published() {
        const CH: usize = 2;
        const N: usize = 32;
        const STALE_SEQ: u64 = 7;

        let layout = test_layout(
            N,
            SF::F32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        // Leave recognisable audio in the slot the stale block would land in, as
        // a previous occupant would have. `publish_input` is NOT called for
        // STALE_SEQ — that is precisely what "the host recycled this slot" means.
        for ch in 0..CH {
            shm.write::<f32>(STALE_SEQ, ch, &[0.5f32; N]).unwrap();
        }

        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let mut plugin = EchoProbe {
            meta: meta(&[CH], &[CH]),
            seen_inputs: std::cell::RefCell::new(Vec::new()),
        };
        pipeline.process(
            &mut plugin,
            &mut shm,
            AudioBlock {
                seq: STALE_SEQ,
                num_samples: N,
                midi: &[],
                extras: ProcessExtras::of(&QUIET),
            },
        );

        assert!(
            !shm.has_output(STALE_SEQ),
            "the server published a block whose input slot had been recycled — \
             the host would accept it as the current block's audio"
        );
    }

    /// Drive `AudioPipeline::process` with a plugin that emits `fill` into
    /// every output sample, then read the slab back and assert the pipeline
    /// replaced the non-finite values with 0.0 (fix #2: NaN/Inf sanitize).
    fn assert_sanitized(fill: f32) {
        const CH: usize = 2;
        const N: usize = 64;
        let layout = test_layout(
            N,
            FloatWidth::F32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let mut plugin = NanPlugin {
            meta: meta(&[CH], &[CH]),
            fill,
        };
        let block = AudioBlock {
            seq: SEQ,
            num_samples: N,
            midi: &[],
            extras: ProcessExtras::of(&QUIET),
        };
        pipeline.process(&mut plugin, &mut shm, block);

        for ch in 0..CH {
            let mut out = vec![1.0f32; N];
            shm.read_output_into::<f32>(SEQ, ch, &mut out).unwrap();
            assert!(
                out.iter().all(|s| s.is_finite()),
                "channel {ch} still contains non-finite samples after sanitize"
            );
        }
    }

    #[test]
    fn process_sanitizes_nan_output() {
        assert_sanitized(f32::NAN);
    }

    #[test]
    fn process_sanitizes_inf_output() {
        assert_sanitized(f32::INFINITY);
        assert_sanitized(f32::NEG_INFINITY);
    }

    /// **A shorter block after a longer one does not allocate.** The scratch
    /// is built once, when the slab is set up (by the control side),
    /// and a block uses its first frames. The
    /// reshape reallocated whenever the length changed, so a host's ragged
    /// final chunk freed and rebuilt every channel on the audio thread.
    #[test]
    fn a_shorter_block_after_a_longer_one_does_not_allocate() {
        const N: usize = 128;
        let layout = test_layout(
            N,
            FloatWidth::F32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let mut plugin = NanPlugin {
            meta: meta(&[2], &[2]),
            fill: 0.25,
        };
        assert_no_alloc::assert_no_alloc(|| {
            for frames in [N, 64, N, 1, 100] {
                pipeline.process(
                    &mut plugin,
                    &mut shm,
                    AudioBlock {
                        seq: SEQ,
                        num_samples: frames,
                        midi: &[],
                        extras: ProcessExtras::of(&QUIET),
                    },
                );
            }
        });
    }

    /// RT-safety regression: the per-block path — including the denormal
    /// guard and the NaN/Inf sanitize sweep — must not allocate in steady
    /// state. Warms up once (the first `process` sizes the scratch buffers),
    /// then asserts a run of blocks is allocation-free.
    ///
    /// **This one uses the `NanPlugin` fake, and that is its limit.** A fake
    /// exercises the pipeline's own scratch handling and *no loader at all*, so
    /// it proves nothing about a loader's per-block conversions. See
    /// [`process_through_the_real_vst3_loader_is_alloc_free`], which drives the
    /// same gate through a real plugin.
    fn assert_alloc_free_at(format: FloatWidth) {
        const CH: usize = 2;
        const N: usize = 128;
        let layout = test_layout(
            N,
            format,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        let mut pipeline = pipeline_for(&shm, format);
        // A finite-output fake; sanitize still runs, it just finds nothing.
        let mut plugin = NanPlugin {
            meta: meta(&[CH], &[CH]),
            fill: 0.25,
        };

        // Warm up: first call allocates the scratch buffers.
        pipeline.process(
            &mut plugin,
            &mut shm,
            AudioBlock {
                seq: SEQ,
                num_samples: N,
                midi: &[],
                extras: ProcessExtras::of(&QUIET),
            },
        );

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..256 {
                pipeline.process(
                    &mut plugin,
                    &mut shm,
                    AudioBlock {
                        seq: SEQ,
                        num_samples: N,
                        midi: &[],
                        extras: ProcessExtras::of(&QUIET),
                    },
                );
            }
        });
    }

    #[test]
    fn process_is_alloc_free() {
        assert_alloc_free_at(FloatWidth::F32);
    }

    /// The `Float64` scratch path builds its tables without allocating either.
    #[test]
    fn process_is_alloc_free_at_float64() {
        assert_alloc_free_at(FloatWidth::F64);
    }

    /// RT-safety regression for the multi-bus split: the per-direction channel
    /// read/write, the sequence checks, and the publish must all stay
    /// allocation-free after warm-up. Two input buses + one output bus.
    #[test]
    fn process_multibus_is_alloc_free() {
        const N: usize = 128;
        // stereo main in + mono sidechain in + stereo out → 5 flat channels.
        let layout = test_layout(
            N,
            FloatWidth::F32,
            &[ChannelLayout::STEREO, ChannelLayout::MONO],
            &[ChannelLayout::STEREO],
        );
        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let mut plugin = NanPlugin {
            meta: meta(&[2, 1], &[2]),
            fill: 0.25,
        };

        // Warm up: first call sizes the scratch buffers.
        pipeline.process(
            &mut plugin,
            &mut shm,
            AudioBlock {
                seq: SEQ,
                num_samples: N,
                midi: &[],
                extras: ProcessExtras::of(&QUIET),
            },
        );

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..256 {
                pipeline.process(
                    &mut plugin,
                    &mut shm,
                    AudioBlock {
                        seq: SEQ,
                        num_samples: N,
                        midi: &[],
                        extras: ProcessExtras::of(&QUIET),
                    },
                );
            }
        });
    }

    /// **The gate that covers a real loader.** The same per-block path, driven
    /// through the actual VST3 reference plugin rather than a fake.
    ///
    /// `process_is_alloc_free` above drives `NanPlugin`, which returns without
    /// touching a loader at all, so nothing in it can observe what a loader does
    /// with the plugin's output. This one covers the whole chain: slab read,
    /// COM-boundary crossing, `Server::render`, the host crate's copy into
    /// the pipeline's reusable `ProcessOutput`, sanitize, and slab write.
    ///
    /// Hermetic: the probe is the vendored `audio-probe`, built by this crate's
    /// build script, so this needs no installed plugin. Gated on the `vst3`
    /// feature rather than on the plugin's presence — a missing probe must fail
    /// loudly, not skip.
    ///
    /// # What this does NOT cover, and why
    ///
    /// **The emitted-event copy path is not exercised here**, because
    /// `audio-probe` declares an event *input* and emits nothing at all
    /// (`probeprocessor.cpp` calls `addEventInput` and never touches
    /// `outputEvents` or `outputParameterChanges`). So the plugin's output lists
    /// arrive empty every block, and an owning conversion over an empty
    /// list allocates nothing, so restoring
    /// `ProcessOutputRef::to_owned()` in `tutti-vst3-host`'s `process_block` leaves
    /// this test green. Saying so is the point: a gate that names a property it
    /// cannot observe is the failure this whole class of test exists to avoid.
    ///
    /// The copy itself is covered directly by
    /// [`copying_emitted_events_into_a_warm_output_does_not_allocate`], which
    /// feeds it non-empty lists. Closing the gap *here* needs an emitting
    /// reference plugin — a `outputEvents`-writing variant of `audio-probe`,
    /// which is a shared C++ fixture and a change of its own.
    #[cfg(feature = "vst3")]
    #[test]
    fn process_through_the_real_vst3_loader_is_alloc_free() {
        use crate::served::{Server, ServerPlugin};
        use tutti_vst3_host::Vst3Loaded;

        const N: usize = 128;
        const SR: f64 = 48_000.0;

        let _lock = crate::test_utils::plugin_load_lock();
        let (mut plugin, _) = Server::<Vst3Loaded>::load(
            tutti_fixtures::main_thread(),
            &tutti_fixtures::detached_work(),
            &tutti_plugin_types::PluginRef::file(crate::test_utils::vst3_probe_path()),
            tutti_units::SampleRate::of(SR),
            N,
            FloatWidth::F32,
            None,
            &mut || {},
            tutti_plugin_types::Hand::local(),
            crate::ties::ties().expect("bells").0,
        )
        .expect("the vendored VST3 probe must load");

        // Shape the slab to what the plugin actually declared, so no channel is
        // silently dropped and the widths the pipeline reads are the real ones.
        let loaded = plugin.loaded().clone();
        let layout = test_layout(
            N,
            FloatWidth::F32,
            &loaded.input_widths(),
            &loaded.output_widths(),
        );

        let (_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        let mut shm = AudioSlab::open(&handle, layout).unwrap();

        let mut pipeline = pipeline_for(&shm, FloatWidth::F32);
        let block = || AudioBlock {
            seq: SEQ,
            num_samples: N,
            midi: &[],
            extras: ProcessExtras::of(&QUIET),
        };

        // Warm up outside the gate: the first block sizes the pipeline scratch,
        // and the plugin's own pooled buffers reach their steady capacity.
        for _ in 0..4 {
            pipeline.process(&mut plugin, &mut shm, block());
        }

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..256 {
                pipeline.process(&mut plugin, &mut shm, block());
            }
        });
    }

    /// The emitted-event copy path itself, with **non-empty** lists — the half
    /// [`process_through_the_real_vst3_loader_is_alloc_free`] cannot reach,
    /// because the reference plugin emits nothing.
    ///
    /// This models exactly what a loader does per block: clear the caller's
    /// reusable `ProcessOutput`, then copy a borrowed view's MIDI and parameter
    /// points into it. Once warm that must allocate nothing — which is only
    /// true because `clear` empties the lists in place and, critically, clears
    /// each queue's `points` **before** dropping the queue list.
    ///
    /// The MIDI is past its cap ([`MIDI_STACK_CAPACITY`]), so the copy also
    /// pins that the list drops the excess (counted) rather than growing.
    ///
    /// [`MIDI_STACK_CAPACITY`]: tutti_plugin_types::MIDI_STACK_CAPACITY
    ///
    /// Copying emitted events into a warm output allocates nothing: the `ProcessOutput` is
    /// reused rather than built per block, `ProcessOutput::reserve_params` leaves room for
    /// every queue, the points storage is fixed, and the MIDI goes through `emit_midi` (which
    /// drops at the cap) rather than `midi_events.extend`.
    #[test]
    fn copying_emitted_events_into_a_warm_output_does_not_allocate() {
        // Points well past any small reservation, so the buffers are genuinely
        // on the heap and a lost capacity is a real reallocation rather than a
        // no-op; MIDI past its cap, so the drop path runs inside the gate.
        const EVENTS: usize = 400;
        const PARAMS: usize = 32;
        const POINTS: usize = 40;

        // The event *values* are irrelevant here — only how many there are, and
        // that `WireMidi` is `Copy` so the copy is a memcpy.
        let emitted_midi: Vec<WireMidi> = (0..EVENTS)
            .map(|i| WireMidi {
                offset: i as u32,
                ump: Ump([0x2090_3C40, 0, 0, 0]),
            })
            .collect();
        let mut emitted_params = ParameterChanges::reserved(PARAMS, POINTS);
        for p in 0..PARAMS {
            for pt in 0..POINTS {
                emitted_params.add_change(
                    ParamId::new(p as u32),
                    pt as i32,
                    pt as f64 / POINTS as f64,
                );
            }
        }

        // The copy a loader performs, factored so warm-up and gate run the same
        // code rather than two hand-kept-in-sync versions.
        let copy = |out: &mut ProcessOutput| {
            out.clear();
            out.emit_midi(emitted_midi.iter().copied());
            for queue in &emitted_params.queues {
                for point in &queue.points {
                    out.emit_param_point(queue.param_id, point.sample_offset, point.value.get());
                }
            }
        };

        // Reserved as `Session::handle_load` reserves the pipeline's output.
        let mut out = ProcessOutput::default();
        out.reserve_params(PARAMS, POINTS);
        // Warm up outside the gate: this is where the buffers reach their
        // steady capacity, exactly as the first few real blocks do.
        for _ in 0..4 {
            copy(&mut out);
        }
        assert_eq!(
            out.midi_events.len(),
            tutti_plugin_types::MIDI_STACK_CAPACITY,
            "the fixture must actually fill the buffers"
        );
        assert_eq!(
            out.midi_dropped(),
            EVENTS - tutti_plugin_types::MIDI_STACK_CAPACITY
        );
        assert_eq!(out.param_changes.queues.len(), PARAMS);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..64 {
                copy(&mut out);
            }
        });
    }
}
