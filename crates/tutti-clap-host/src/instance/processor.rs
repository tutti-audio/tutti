//! The realtime half of an activation: [`ClapProcessor`], whose sample format
//! is a value chosen at activation, and the trait's `Processor` impl on it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_buffers::FloatWidth;
use tutti_buffers::{Float, InputOutput};
use tutti_plugin_types::{
    EditReader, Flushed, NoteExpressionChanges, PluginError, PluginIo, PluginResult, Processor,
    ReportSink,
};
use tutti_rt::Reader;
use tutti_units::Normalized;
use tutti_units::ParamId;

use tutti_clap_ffi::AudioSide;

use super::ClapProcessContext;
use super::audio::ProcessOutputRef;
use super::config::{AudioScratch, RangeTable};
use super::held::HeldWrites;
use super::thread_pool::PoolCell;
use crate::types::ClapNoteExpression;

/// What activation hands [`ClapProcessor::new`].
pub(crate) struct ProcessorParts {
    pub audio_side: AudioSide,
    pub pool: Reader<PoolCell>,
    pub input_ports: Vec<u32>,
    pub output_ports: Vec<u32>,
    pub max_frames: u32,
    pub scratch: Box<AudioScratch>,
    pub ranges: Reader<RangeTable>,
    pub drain: EditReader,
    pub reports: ReportSink,
    pub reports_dropped: Arc<AtomicU64>,
}

/// The realtime half of an activation: it renders blocks and nothing else. Its
/// sample [`format`](Self::format) is a value chosen at activation.
///
/// `Send` to move to the realtime thread, not `Sync`. It holds the audio half
/// of the plugin ([`AudioSide`]), which can call only the audio-thread entries
/// and takes the `[audio-thread]` claim for every call, so it never touches the
/// `ClapLoaded` its [`ClapController`](super::ClapController) holds. Dropping it
/// releases its share of the activation and calls nothing in the plugin.
///
/// Both halves hold one plugin instance. Why that is sound (the two halves
/// call disjoint entries, audio entries never overlap, and the instance is
/// destroyed once, by the main thread, last) is argued rule by rule in the
/// `tutti_clap_ffi::Plugin` module docs; this crate holds no `unsafe` of its own.
pub struct ClapProcessor {
    /// Boxed (several KiB): a Processor moves by value through activation.
    pub(crate) scratch: Box<AudioScratch>,
    pub(crate) ranges: Reader<RangeTable>,
    pub(crate) drain: EditReader,
    /// The writes a flush kept for the next block's `process`: a parameter
    /// flagged `REQUIRES_PROCESS` is never delivered out of band while the
    /// plugin is active.
    pub(crate) held: HeldWrites,
    /// The plugin's own parameter edits (its output `PARAM_VALUE` and gesture
    /// events), on their way to the Controller's `take_reported_edits`.
    pub(crate) reports: ReportSink,
    /// What `reports` refused for want of room, which the Controller counts.
    pub(crate) reports_dropped: Arc<AtomicU64>,
    pub(crate) audio_side: AudioSide,
    pub(crate) pool: Reader<PoolCell>,
    pub(crate) input_ports: Vec<u32>,
    pub(crate) output_ports: Vec<u32>,
    pub(crate) max_frames: u32,
}

impl std::fmt::Debug for ClapProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapProcessor")
            .field("format", &self.format())
            .field("max_frames", &self.max_frames)
            .field("input_ports", &self.input_ports)
            .field("output_ports", &self.output_ports)
            .finish_non_exhaustive()
    }
}

impl ClapProcessor {
    pub(crate) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            audio_side,
            pool,
            input_ports,
            output_ports,
            max_frames,
            scratch,
            mut ranges,
            drain,
            reports,
            reports_dropped,
        } = parts;
        let held = HeldWrites::with_room(ranges.lend().ranges.len());
        Self {
            scratch,
            ranges,
            drain,
            held,
            reports,
            reports_dropped,
            audio_side,
            pool,
            input_ports,
            output_ports,
            max_frames,
        }
    }

    /// The audio half this activation holds; `deactivate` checks it belongs to
    /// the Controller's plugin.
    pub(crate) fn audio_side(&self) -> &AudioSide {
        &self.audio_side
    }

    /// Whether `start_processing` has succeeded and no `stop_processing` has
    /// followed. Distinct from *active*: an activated instance is not
    /// processing until the first `process` call self-starts it.
    pub fn is_processing(&self) -> bool {
        self.audio_side.is_processing()
    }

    /// Clears the plugin's processing state — buffers, filters, oscillators,
    /// envelopes, LFOs — and kills its voices. Parameter values are unchanged.
    ///
    /// Call this on any playback discontinuity the host creates: a locate, a
    /// loop wrap, a punch. Without it a reverb tail, a ringing filter or a hung
    /// voice bleeds across the jump into material it never belonged to.
    ///
    /// Also resets the `steady_time` counter this instance passes to
    /// `clap_process`. CLAP permits `steady_time` to jump backward **because**
    /// `reset` was called, so the two belong in one call; `stop_processing`
    /// zeroes it for the same reason, at the other discontinuity a host makes.
    ///
    /// # Threading
    /// CLAP marks `reset` `[audio-thread & active]`. The `[audio-thread]` half
    /// is taken here rather than asserted: the call runs under an
    /// audio claim, so the calling OS
    /// thread becomes the audio thread for its duration (which the spec permits
    /// for any thread). It never waits: while the main thread holds the
    /// instance, the plugin's `reset` is owed and the next block runs it first.
    pub fn reset(&mut self) {
        // Under the audio claim, which the FFI half takes; a retired instance
        // (the Controller is gone) is left alone.
        if self.audio_side.is_retired() {
            return;
        }
        self.audio_side.reset();
        self.scratch.steady_time = 0;
        // The plugin forgot its input: a SysEx open across the reset must not
        // complete on the far side of it.
        self.scratch.input_events.reset_midi();
        // Nor does the plugin's output carry a parameter run across it.
        self.scratch.out_translator.reset();
    }

    /// Takes the parameter writes no block has delivered, into `out`, skipping
    /// the stale: what a flush kept for the next block, then the Controller's
    /// own, so a later write of a parameter follows its earlier one. Main
    /// thread, with the Processor held (nothing is rendering): for a caller
    /// that must not lose them, as a rejoin must not.
    pub fn take_edits(&mut self, out: &mut Vec<(ParamId, Normalized)>) {
        self.held
            .take(self.drain.epoch(), |id, value| out.push((id, value)));
        self.drain.drain(|id, value| out.push((id, value)));
    }

    /// Ends processing (`stop_processing`) if it started, under the audio
    /// claim. The next `process` starts it again. Never waits: while the main
    /// thread holds the instance the plugin stays processing, and the next
    /// deactivation stops it.
    pub fn stop(&mut self) {
        let was_processing = self.audio_side.is_processing();
        self.audio_side.stop();
        // The CLAP steady_time counter is per start/stop cycle — reset it so
        // the next start_processing begins the monotonic sequence at 0.
        if was_processing && !self.audio_side.is_processing() && !self.audio_side.is_retired() {
            self.scratch.steady_time = 0;
        }
    }
}

impl ClapProcessor {
    /// The format this processor was activated at: a value, fixed at
    /// activation, that every block's buffers are matched against.
    #[must_use]
    pub fn format(&self) -> FloatWidth {
        self.scratch.process.format()
    }

    /// Whether this processor renders 64-bit audio.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        self.format() == FloatWidth::F64
    }

    /// Renders one block, with the buffers, the context and the output taken
    /// apart. [`Processor::process`] is this with a [`PluginIo`]; a caller whose
    /// channel tables have distinct lifetimes, as an `InputOutput` allows, calls
    /// this one.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] when the buffer's sample format is not the one
    /// the processor was activated with; [`PluginError::Block`] when the plugin
    /// failed the block.
    pub fn render<S: Float>(
        &mut self,
        buffer: &mut InputOutput<'_, S>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        self.process_shared(buffer, ctx, out)
    }

    /// One block from the shared vocabulary: converts the context into CLAP's
    /// (note expressions into scratch), processes, and
    /// copies what the plugin emitted into `out`.
    fn process_shared<S: Float>(
        &mut self,
        buffer: &mut InputOutput<'_, S>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        self.scratch.in_note_expressions.clear();
        if let Some(changes) = ctx.note_expression {
            let scratch = &mut self.scratch;
            convert_note_expressions(
                changes,
                &mut scratch.in_note_expressions,
                &mut scratch.in_note_expressions_dropped,
            );
        }
        // `process` borrows `self` for the output it returns, so the expressions
        // are moved out of the scratch for the call and put back after. A move of
        // a `Vec`'s three words, no allocation.
        let expressions = std::mem::take(&mut self.scratch.in_note_expressions);
        let clap_ctx = ClapProcessContext {
            midi: ctx.midi_events,
            // An empty list means "no automation this block"; the plugin reads
            // `None` and `Some(empty)` differently, so the mapping is kept.
            params: ctx.param_changes.filter(|p| !p.is_empty()),
            expressions: &expressions,
            transport: ctx.transport,
            transport_changes: ctx.transport_changes,
        };
        let result = self.process(buffer, &clap_ctx);
        let result = result
            .map(|output| copy_process_output(output, out))
            .map_err(|e| PluginError::Block(block_reason(&e)));
        self.scratch.in_note_expressions = expressions;
        if result.is_ok() {
            self.report_edits();
            // The plugin's own word on the block. CLAP's
            // *continue* and *continue if not quiet* say nothing the graph can
            // use; *tail* and *sleep* are the two it narrows on.
            out.set_status(if self.is_sleeping() {
                tutti_plugin_types::BlockStatus::Sleep
            } else if self.is_tailing() {
                tutti_plugin_types::BlockStatus::Tail
            } else {
                tutti_plugin_types::BlockStatus::Continue
            });
        }
        result
    }

    /// Queues the parameter edits the block's output events carried, in the
    /// order the plugin pushed them, for the Controller's `take_reported_edits`.
    ///
    /// `CLAP_EVENT_PARAM_VALUE` is a change (plain, so normalized against the
    /// declared bounds, as the editor instance's is), and
    /// `CLAP_EVENT_PARAM_GESTURE_BEGIN` and `_END` its gesture. A value flagged
    /// `CLAP_EVENT_DONT_RECORD` is reported, since a host that shows the value
    /// follows it, with the flag, since one that records automation must not
    /// (events.h: "should not be recorded"; the example is a value moved by a
    /// MIDI CC that is itself recorded). Nothing here allocates.
    pub(super) fn report_edits(&mut self) {
        let table = self.ranges.lend();
        for event in self.scratch.output_events.events() {
            let Some(heard) = event.heard(&table.ranges) else {
                continue;
            };
            if self.reports.send(heard).is_err() {
                self.reports_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Why a block failed, as a constant: the audio thread builds this for every
/// errored block, so it must not allocate (`ClapError`'s own text is formatted).
fn block_reason(error: &crate::ClapError) -> &'static str {
    use crate::ClapError;
    match error {
        ClapError::PluginReturnedError => "the plugin returned CLAP_PROCESS_ERROR",
        ClapError::BlockTooLarge { .. } => "the block is longer than the activated max_frames",
        ClapError::BlockTooSmall => "the block is shorter than the activated min_frames",
        ClapError::StartProcessingFailed => "the plugin refused to start processing",
        ClapError::NotActive => "the plugin is no longer active",
        ClapError::Busy => "the main thread held the plugin, and the block does not wait",
        ClapError::Poisoned => {
            "host code panicked while holding the plugin; no block runs on it again"
        }
        ClapError::BufferTooShort => "a channel buffer is shorter than the block",
        ClapError::FormatMismatch => "the block's sample format is not the activated one",
        ClapError::LayoutMismatch => "the port layout is larger than the activation sized for",
        _ => "the plugin failed the block",
    }
}

/// The block's note expressions, narrowed to CLAP's voice-addressed form.
///
/// The expression dimension is the shared `NoteExpressionType` on both sides;
/// only CLAP's wrapper differs (it adds port/channel/key), which `from_shared`
/// fills with defaults. Past the inline room the rest are dropped.
fn convert_note_expressions(
    changes: &NoteExpressionChanges,
    into: &mut Vec<ClapNoteExpression>,
    dropped: &mut u64,
) {
    for change in &changes.changes {
        if into.len() == into.capacity() {
            *dropped += 1;
        } else {
            into.push(ClapNoteExpression::from_shared(change));
        }
    }
}

/// Copy one block's emitted events out of the CLAP host's pooled buffers into
/// the caller's reusable [`ProcessOutput`](tutti_plugin_types::ProcessOutput).
///
/// `out` arrives cleared, with its heap capacity intact, so every copy and
/// `add_change` below writes into storage that already exists.
fn copy_process_output(output: ProcessOutputRef<'_>, out: &mut tutti_plugin_types::ProcessOutput) {
    out.emit_midi(output.midi_events.iter().copied());
    out.count_midi_dropped(output.midi_lost);

    for queue in &output.param_changes.queues {
        for point in &queue.points {
            // Pooled queues: not `ParameterChanges::add_change`, which grows.
            out.emit_param_point(queue.param_id, point.sample_offset, point.value);
        }
    }

    // CLAP's note expression uses the shared `NoteExpressionType` (Pressure /
    // Expression included), so this narrows CLAP's voice-addressed value down
    // to the protocol shape without losing the dimension.
    for expr in output.note_expressions {
        out.note_expression
            .add_change(tutti_plugin_types::NoteExpressionValue {
                sample_offset: expr.sample_offset,
                note_id: expr.note_id,
                expression_type: expr.expression_type,
                value: expr.value,
            });
    }
}

impl Processor for ClapProcessor {
    fn event_input(&self) -> std::num::NonZeroU32 {
        // A list with no MIDI room takes none; one is the least a bound says.
        self.scratch
            .input_events
            .event_input()
            .unwrap_or(std::num::NonZeroU32::MIN)
    }

    fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    fn stop(&mut self) {
        ClapProcessor::stop(self);
    }

    /// `params.flush` ([`ClapProcessor::flush_parameters`]).
    fn flush_parameters(
        &mut self,
        changes: &tutti_plugin_types::ParameterChanges,
    ) -> PluginResult<Flushed> {
        ClapProcessor::flush_parameters(self, changes)
    }

    fn reset(&mut self) {
        ClapProcessor::reset(self);
    }
}

// A Processor moves by value through activation (`Lifecycle`, the server's
// `Server`), and every debug frame on that path holds a copy. Its scratch is
// boxed, so it stays small; a new inline array that tips this bound fails the
// build.
const _: () = assert!(size_of::<ClapProcessor>() <= 4096);
