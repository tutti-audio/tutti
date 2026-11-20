//! The realtime half of an activation: [`Vst2Processor`].
//!
//! The entry point [`Vst2Processor::process_planes`] updates the cached transport
//! snapshot, then drives the scratch buffers and the `vst` crate's
//! `AudioBuffer`. It returns any MIDI events the plugin emitted.
//! [`Processor::process`] is the same over the shared vocabulary's
//! [`PluginIo`].
//!
//! The render slot is the one the plugin was activated at, VST 2.4's
//! `processReplacing` for f32 and `processReplacingF64` for f64, whatever the
//! block's width: the scratch converts at the edge.

use std::sync::Arc;
use tutti_graph::UmpAt;
use tutti_plugin_types::{MidiEventVec, TransportInfo};
use tutti_rt::{Counter, Tally};

use vst::RenderHandle;

use tutti_buffers::FloatWidth;
use tutti_buffers::{ByWidth, Float, InputOutput};
use tutti_plugin_types::{
    ParameterChanges, PluginError, PluginIo, PluginResult, ProcessContext, ProcessOutput,
    Processor, SampleRate,
};
use tutti_units::Normalized;

use crate::host::HostState;
use crate::loaded::{AudioSide, LossesTold};
use crate::time_info::{advance_time_info, build_vst2_time_info};
use crate::transport_cell::TransportWriter;
use crate::types::Vst2ProcessContext;
use vst::MidiIo;
use vst::RenderScratch;

/// The realtime half of an activation: renders blocks.
///
/// `Send` to move to the realtime thread, not `Sync`. It holds the audio handle on the
/// plugin (a [`RenderHandle`]: the render entries, `setParameter`, `effStartProcess` and
/// `effStopProcess`, and nothing else) and never reaches the
/// [`Vst2Loaded`](crate::Vst2Loaded) the Controller owns. Dropping it releases the handle
/// and nothing more: the plugin is closed by the Controller's `Vst2Loaded`, on its thread,
/// and a Processor that outlives it renders silence.
///
/// The doc test below is the guard: an `unsafe impl Sync` for the
/// type makes it compile, and the test fails:
///
/// ```compile_fail
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<tutti_vst2_host::Vst2Processor>();
/// ```
pub struct Vst2Processor {
    render: RenderHandle,
    /// How many parameters the plugin declared: the bound an automation point's
    /// id is checked against.
    param_count: usize,
    scratch: RenderScratch,
    time_info: TransportWriter,
    midi: MidiIo,
    /// The maximum block the plugin was told (`effSetBlockSize`). A block longer
    /// than this is split before it reaches the plugin.
    block_size: usize,
    /// The rate the plugin was told (`effSetSampleRate`).
    sample_rate: SampleRate,
    /// The host's end of the plugin's callbacks, for its drop counters.
    host: Arc<HostState>,
    /// The plugin's refused audio entries, as the plugin instance counts them.
    refusals: EntryRefusals,
    /// The losses `render` has already put in its `ProcessOutput`; a rejoin keeps them.
    told: LossesTold,
    /// The events staged for this block's pieces the plugin refused (the gate was closed,
    /// or the entry missing), which never reached it: counted lost by
    /// [`render`](Self::render).
    refused_midi: u64,
}

/// The tallies of the audio entries the plugin refused, read from the plugin instance:
/// the gate was closed or held, the plugin grew its pins past the buffer, it has no entry
/// for the block, or its dispatcher was cleared. Each refusal is a block (or piece) that
/// rendered silence.
#[derive(Clone)]
pub(crate) struct EntryRefusals {
    pub gate: Tally<u64>,
    pub pins: Tally<u64>,
    pub entries: Tally<u64>,
    pub dispatcher: Tally<u64>,
}

impl EntryRefusals {
    fn total(&self) -> u64 {
        self.gate
            .get()
            .saturating_add(self.pins.get())
            .saturating_add(self.entries.get())
            .saturating_add(self.dispatcher.get())
    }
}

impl std::fmt::Debug for Vst2Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst2Processor")
            .field("block_size", &self.block_size)
            .field("format", &self.scratch.format())
            .field("processing", &self.render.is_processing())
            .finish_non_exhaustive()
    }
}

/// Everything a Processor is built from.
pub(crate) struct ProcessorParts {
    pub render: RenderHandle,
    pub param_count: usize,
    pub scratch: RenderScratch,
    pub audio: AudioSide,
    pub block_size: usize,
    pub sample_rate: SampleRate,
    pub host: Arc<HostState>,
    pub refusals: EntryRefusals,
}

impl Vst2Processor {
    pub(crate) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            render,
            param_count,
            scratch,
            audio,
            block_size,
            sample_rate,
            host,
            refusals,
        } = parts;
        Self {
            render,
            param_count,
            scratch,
            time_info: audio.time_info,
            midi: audio.midi,
            block_size,
            sample_rate,
            host,
            refusals,
            told: audio.told,
            refused_midi: 0,
        }
    }

    /// Every MIDI event lost on the way in to the plugin since load: SysEx dropped whole,
    /// messages of another group, and events past a block's room.
    fn midi_in_lost_total(&self) -> u64 {
        self.midi
            .send
            .sysex_dropped()
            .get()
            .saturating_add(self.midi.send.groups_dropped().get())
            .saturating_add(self.midi.send.events_dropped().get())
    }

    /// How many audio entries the plugin refused since load: blocks, or pieces of a long
    /// block, that rendered silence.
    #[must_use]
    pub fn entries_refused(&self) -> u64 {
        self.refusals.total()
    }

    /// The audio handle: `deactivate` compares it with the Controller's instance.
    pub(crate) fn audio_handle(&self) -> &RenderHandle {
        &self.render
    }

    /// Whether `effStartProcess` is the plugin's last word: true from
    /// activation, false after [`stop`](Self::stop) until the next block.
    #[must_use]
    pub fn is_processing(&self) -> bool {
        self.render.is_processing()
    }

    /// Ends processing (`effStopProcess`) if it is on. The next block starts it
    /// again. Realtime-safe; it is also what a rejoin sends first, once.
    pub fn stop(&mut self) {
        self.render.stop_process();
    }

    /// Starts processing (`effStartProcess`) if a stop left it off.
    fn ensure_processing(&mut self) {
        self.render.start_process();
    }

    /// What the rejoin keeps of the audio side, and drops the rest of.
    pub(crate) fn into_audio(mut self) -> AudioSide {
        // A deactivation is a discontinuity of the plugin's output.
        self.midi.reset_out();
        AudioSide {
            time_info: self.time_info,
            midi: self.midi,
            told: self.told,
        }
    }

    /// Whether the plugin was told `effSetProcessPrecision(64)`, which is what
    /// makes [`process_planes`](Self::process_planes) enter `processReplacingF64`.
    #[must_use]
    pub fn is_double_precision(&self) -> bool {
        self.scratch.format() == FloatWidth::F64
    }

    /// Returns how many plugin-emitted MIDI events were **dropped** since the
    /// plugin was loaded because the MIDI-out queue was full. Monotonic.
    ///
    /// Counted apart from the parameter drops because the consequence differs: a
    /// dropped note-off whose note-on landed is a stuck note.
    #[must_use]
    pub fn dropped_midi_out(&self) -> &Tally<u64> {
        self.host.dropped_midi_out()
    }

    /// Returns how many SysEx messages were **dropped** whole since load rather
    /// than handed to the plugin truncated: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// left without their `END` by a later `START` or by a `SINGLE` on their
    /// own group, or one whose packet found the reassembly buffer's room
    /// full (more packets in one block than it reserves for). Monotonic.
    ///
    /// The host reassembles a SysEx that spans several UMP packets (and
    /// blocks) and hands the plugin one complete `0xF0 … 0xF7` message, as a
    /// `kVstSysExType` event, at its `END` packet's frame.
    #[must_use]
    pub fn dropped_sysex(&self) -> &Counter<u64> {
        self.midi.send.sysex_dropped()
    }

    /// Returns how many incoming channel voice messages were **dropped** since
    /// load because they were of a Group other than 0: VST2 has one MIDI input,
    /// which takes Group 0's channels only, and a message of another Group is
    /// never heard on them. Monotonic.
    #[must_use]
    pub fn groups_dropped(&self) -> &Counter<u64> {
        self.midi.send.groups_dropped()
    }

    /// Returns how many incoming MIDI events were **dropped** since load because
    /// one block carried more than
    /// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) (the
    /// plugin node never hands it more). Monotonic.
    #[must_use]
    pub fn dropped_midi_in(&self) -> &Counter<u64> {
        self.midi.send.events_dropped()
    }

    /// Renders one block of `io.frames()` frames at its width and returns the
    /// MIDI the plugin emitted during it.
    ///
    /// `io` holds one plane per channel. `ctx` carries this block's MIDI input,
    /// transport and sample rate. A zero-length block returns immediately
    /// without entering the plugin.
    ///
    /// A plugin told 64-bit at activation (`Prepare::format` of `F64` and
    /// `effFlagsCanDoubleReplacing`) is entered through `processReplacingF64`, so
    /// the samples never pass through f32; one told 32 is entered through
    /// `processReplacing`, and a 64-bit block for it is converted to f32 at the
    /// scratch and back. That fallback is lossy and deliberate: the alternative
    /// is silence (what the vendor's `process_f64` returns on a null
    /// `processReplacingF64`), which would turn an f64 master chain into a mute
    /// for every plugin that only speaks f32. Callers wanting to avoid the
    /// narrowing read `metadata().supports_f64` and activate at `F32`.
    ///
    /// A block longer than the maximum the plugin was told
    /// ([`Prepare::max_block`](tutti_plugin_types::Prepare::max_block)) is
    /// rendered as several calls of at most that length, in order: a plugin
    /// sizes its buffers to the announced maximum. MIDI events keep their frames
    /// across the pieces (each is delivered in the piece it falls in, with its
    /// offset rebased), the plugin's MIDI out keeps its frames the same way, and
    /// each piece's transport snapshot is the block's advanced by the frames
    /// before it.
    ///
    /// The returned events live in a buffer owned by the Processor and are
    /// replaced on the next call; copy out what you need to keep.
    ///
    /// Real-time safe: no allocation or locking on this path.
    pub fn process_planes<S: Float>(
        &mut self,
        io: &mut InputOutput<'_, S>,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        let num_samples = io.frames().get();
        if num_samples == 0 {
            return &self.midi.out;
        }
        self.ensure_processing();
        // An `Arc` clone (no allocation): the scope outlives the `&mut self` calls below.
        let host = Arc::clone(&self.host);
        let _in_process = host.enter_process();

        let (input, mut output) = io.split();
        // A block longer than the maximum the plugin was told is rendered in
        // pieces no longer than it: a plugin sizes its internal buffers to the
        // announced maximum, so a longer call overruns them.
        let mut start = 0;
        while start < num_samples {
            let len = (num_samples - start).min(self.block_size.max(1));
            let refused = self.refusals.total();
            let staged = self.begin_piece(ctx, start, len, num_samples);
            self.scratch
                .render_piece(&mut self.render, input, &mut output, start, len);
            if self.refusals.total() != refused {
                // The plugin never heard the events staged for the piece. Those staging
                // dropped are already in the staging counters.
                self.refused_midi += staged as u64;
            }
            self.midi.drain_from(start);
            start += len;
        }
        &self.midi.out
    }

    /// Sets up one piece of a block, `len` frames from `start` of `total`:
    /// the transport snapshot the plugin reads, then the MIDI events that fall
    /// in the piece. The piece's first call publishes `ctx`'s transport; each
    /// later piece advances that snapshot by the frames before it. Returns how many MIDI
    /// events were staged for the plugin.
    fn begin_piece(
        &mut self,
        ctx: &Vst2ProcessContext,
        start: usize,
        len: usize,
        total: usize,
    ) -> usize {
        if start == 0 {
            self.update_transport(ctx);
        } else {
            // Every earlier piece was a full `block_size`.
            self.advance_transport(self.block_size.max(1), ctx.sample_rate);
        }
        self.dispatch_midi(ctx.midi, start, len, total)
    }

    /// Refreshes the snapshot the plugin reads back via `audioMasterGetTime`.
    ///
    /// The previously stored snapshot (the writer's own copy) is handed to the
    /// builder:
    /// `kVstTransportChanged` is an *edge*, so the only way to know whether the
    /// transport state moved is to compare against what this plugin was last
    /// told. A block with no transport (`ctx.transport` of `None`) publishes a stopped
    /// transport at the origin ([`TransportInfo::new`]), so a plugin never reads a
    /// rolling transport that the host stopped reporting.
    ///
    /// Runs on the audio thread, so it must not allocate — hence
    /// [`TransportWriter`], which overwrites
    /// in place, rather than an `ArcSwap` store that would allocate and free a
    /// snapshot per block inside the callback.
    ///
    /// Must complete *before* the plugin is entered: the plugin issues
    /// `audioMasterGetTime` re-entrantly from inside `process`, and a seqlock
    /// reader cannot make progress against a write in flight on its own thread.
    /// `&mut self` makes the write the only one, and it returns before the
    /// plugin is called.
    fn update_transport(&mut self, ctx: &Vst2ProcessContext) {
        let stopped;
        let transport = match ctx.transport {
            Some(t) => t,
            None => {
                stopped = TransportInfo::new(ctx.sample_rate);
                &stopped
            }
        };
        let previous = self.time_info.last();
        let next = build_vst2_time_info(
            transport,
            ctx.sample_rate,
            previous.as_ref(),
            self.host.automation(),
        );
        self.time_info.write(next);
    }

    /// Advances the published snapshot for the next piece of a split block
    /// ([`advance_time_info`]).
    fn advance_transport(&mut self, frames: usize, sample_rate: SampleRate) {
        if let Some(t) = self.time_info.last() {
            self.time_info
                .write(advance_time_info(t, frames, sample_rate));
        }
    }

    fn dispatch_midi(&mut self, midi: &[UmpAt], start: usize, len: usize, total: usize) -> usize {
        // Marked live unless the host renders offline, as `audioMasterGetCurrentProcessLevel`
        // answers.
        self.midi.send.set_live(!self.host.is_offline());
        self.midi
            .send
            .send_piece(&mut self.render, midi, start, len, total)
    }
}

impl Vst2Processor {
    /// Writes a block's automation to the plugin: the last point of each queue,
    /// as one `setParameter`. VST2 has no sample-accurate parameter event, so a
    /// point inside the block lands on its first frame.
    ///
    /// A host write: the echo the plugin may make through `audioMasterAutomate`
    /// from inside the call is not reported as the plugin's own edit
    /// (`host_write`). Returns how many queues were
    /// refused, because they name a parameter the plugin does not have or one it
    /// exposes no `setParameter` for. Realtime-safe.
    pub(crate) fn write_automation(&mut self, changes: &ParameterChanges) -> u32 {
        let mut refused = 0;
        for queue in &changes.queues {
            let Some(point) = queue.points.last() else {
                continue;
            };
            let Some(index) = crate::parameters::index_of(queue.param_id, self.param_count) else {
                refused += 1;
                continue;
            };
            // `Normalized` is already on the unit interval, so the cast cannot
            // hand the plugin a NaN or an out-of-range value.
            let value: Normalized = point.value;
            if !crate::host::host_write(|| self.render.set_parameter(index, value.get() as f32)) {
                refused += 1;
            }
        }
        refused
    }

    /// The automation of a block that carries any.
    fn apply_automation(&mut self, changes: Option<&ParameterChanges>) -> u32 {
        changes.map_or(0, |changes| self.write_automation(changes))
    }

    /// One block from the shared vocabulary: applies the automation, renders at
    /// the buffer's width and copies what the plugin emitted into `out`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Block`] when the buffer is not the width the plugin was
    /// activated at, in either direction. It does not allocate and does not enter the plugin.
    pub fn render<S: Float>(
        &mut self,
        buffer: &mut InputOutput<'_, S>,
        ctx: &ProcessContext<'_>,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        let transport = ctx.transport;
        let sample_rate = self.sample_rate;
        let host_ctx = Vst2ProcessContext {
            midi: ctx.midi_events,
            transport,
            sample_rate,
        };
        // The width is matched once per block against the activation's.
        match (self.scratch.format(), buffer.by_width()) {
            (FloatWidth::F32, ByWidth::F32(_)) | (FloatWidth::F64, ByWidth::F64(_)) => {}
            _ => {
                return Err(PluginError::Block(
                    "the block's sample format is not the one the plugin was activated with",
                ));
            }
        }
        let refused = self.apply_automation(ctx.param_changes);
        let emitted = self.process_planes(buffer, &host_ctx);
        // `out` arrives cleared, with its capacity intact: past its cap the MIDI
        // is dropped and counted. VST2 reports parameter changes through
        // `audioMasterAutomate`, not a per-block list, so there is nothing for
        // `out`'s parameter points.
        out.emit_midi(emitted.iter().copied());
        out.count_params_refused(refused as usize);
        // What was lost since the last block, each in its own counter. Input the host refused
        // (staging's drops and the events of a refused piece, in messages); the plugin's
        // output that found no room is dropped.
        let midi_in = self.midi_in_lost_total();
        let midi_out = self.host.dropped_midi_out().get();
        let entries = self.refusals.total();
        let lost_in =
            midi_in.saturating_sub(self.told.midi_in) + std::mem::take(&mut self.refused_midi);
        out.count_input_refused(count_of(lost_in));
        out.count_midi_dropped(count_of(midi_out.saturating_sub(self.told.midi_out)));
        out.count_entries_refused(count_of(entries.saturating_sub(self.told.entries)));
        self.told = LossesTold {
            midi_in,
            midi_out,
            entries,
        };
        Ok(())
    }
}

/// `n` as a count for a `ProcessOutput`, saturating.
fn count_of(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

impl Processor for Vst2Processor {
    /// A direct `setParameter` of each parameter's last value, which the SDK
    /// allows between blocks.
    fn flush_parameters(&mut self, changes: &ParameterChanges) -> PluginResult<()> {
        let refused = self.write_automation(changes);
        if refused == 0 {
            Ok(())
        } else {
            Err(PluginError::Block("the plugin refused a parameter write"))
        }
    }

    /// Takes the node's own intake: events go to the plugin's own queue.
    fn event_input(&self) -> std::num::NonZeroU32 {
        tutti_plugin_types::MIDI_STACK_BOUND
    }

    fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    fn stop(&mut self) {
        Vst2Processor::stop(self);
    }

    /// Asks the plugin to forget its signal history with `effStopProcess`, the opcode the
    /// header has for ending processing (`aeffectx.h`: "no arguments"), and starts it again
    /// with `effStartProcess` in the next block. This is VST3's `setProcessing(false)` twin,
    /// and like it a request a plugin may ignore: the header says nothing of what a plugin
    /// does on it, and clearing delay lines and tails there is what practice has plugins do
    /// (authority 2, the SDK's `suspend`/`resume` convention; JUCE's host resets with
    /// `effMainsChanged(0)`, `(1)`, which is a main-thread opcode and so not an option for a
    /// realtime reset). It also drops the host's MIDI state: a SysEx message open across the
    /// reset does not complete on the far side of it, and the RPN, NRPN and bank each channel
    /// had selected are sent in full again.
    fn reset(&mut self) {
        self.midi.send.reset_midi();
        self.midi.reset_out();
        self.render.stop_process();
    }
}
