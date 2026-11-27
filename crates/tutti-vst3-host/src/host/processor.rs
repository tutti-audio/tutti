//! The realtime half of an activation: [`Vst3Processor<T>`] at a fixed sample
//! type, and [`ProcessorAny`], the trait's `Processor`, which picks the
//! type at activation.

use std::sync::Arc;
use tutti_graph::UmpAt;

use vst3::Steinberg::Vst::ProcessSetup;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{ByWidth, Float, InputOutput};
use tutti_plugin_types::{
    ChannelTable, EditReader, ParamAccess, ParamEdit, ParamWriteError, PluginError, PluginIo,
    PluginResult, Processor, ReportSink, RtMidiEvents,
};
use tutti_rt::{Generation, TallyWriter, tally};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

use crate::events::{InputEvents, OutputStream, Stager, drain_midi};
use crate::types::{
    ParameterChanges, ProcessContextRequirements, ProcessMode, ProcessOutputRef, TransportInfo,
    Vst3Sample, to_process_context,
};
use tutti_vst3_ffi::{
    AudioSide, BusBuffers, Busy, EventList, FlushIo, ParameterChangesImpl, ProcessIo,
};

use super::midi_learn::MidiLearnProducer;
use super::midi_mapping::learnable_controller;
use super::sequencer::{SeqScratch, expr_ints_of, fill_chords, fill_expr_texts, fill_scales};
use super::shell::{Hold, Shell};

/// The plugin's `process` (or the call that readies it) failed: the status it
/// returned, or why the host did not make the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("the plugin's process failed: {error}")]
pub struct ProcessFailed {
    /// What failed, by name.
    pub error: tutti_vst3_ffi::Vst3Error,
}

/// Sample rate / block size / channel counts captured at activation. Read by
/// `apply_process_setup` to fill `ProcessSetup` and by `process` to size
/// `ProcessData`. Channel counts are re-synced post-activation in
/// `activate_buses` (some plugins only finalise their arrangement once active).
pub(super) struct ProcessConfig {
    pub(super) sample_rate: SampleRate,
    pub(super) block_size: usize,
    pub(super) num_input_channels: ChannelLayout,
    pub(super) num_output_channels: ChannelLayout,
    /// The mode last handed to `setupProcessing`. This is the value
    /// `ProcessData::processMode` must agree with, so both reads go through
    /// this one field rather than two constants that could drift apart.
    pub(super) setup_mode: ProcessMode,
    /// The mode written into each block's `ProcessData`. Equal to `setup_mode`
    /// except while a live realtime↔prefetch toggle is in effect — the one
    /// divergence `ProcessSetupCheck` permits without a fresh `setupProcessing`.
    pub(super) block_mode: ProcessMode,
}

/// The input half of `ProcessData`: per-bus audio scratch plus the COM-wrapped
/// event and parameter-change lists the host stages for the plugin to read.
/// All three are reused in place each block to keep the RT path allocation-free.
pub(super) struct InputStaging {
    /// Per-bus `AudioBusBuffers` scratch. Bus 0 is mapped onto the live caller
    /// buffer; extra input buses receive silence.
    pub(super) buses: BusBuffers,
    pub(super) events: vst3::ComWrapper<EventList>,
    /// Fills `events` from a block's sources; keeps the per-note bend ranges.
    pub(super) stager: Stager,
    pub(super) param_changes: vst3::ComWrapper<ParameterChangesImpl>,
}

/// The output half of `ProcessData`: per-bus audio scratch, the COM-wrapped
/// lists the plugin writes into, plus the pooled buffers `process` drains those
/// emitted events into so it can return a borrowed [`ProcessOutputRef`].
pub(super) struct OutputStaging {
    /// Per-bus `AudioBusBuffers` scratch. Bus 0 is mapped onto the live caller
    /// buffer; extra output buses get a discard sink.
    pub(super) buses: BusBuffers,
    pub(super) events: vst3::ComWrapper<EventList>,
    pub(super) param_changes: vst3::ComWrapper<ParameterChangesImpl>,
    /// Pooled return-value buffers. `process` drains the plugin's emitted
    /// events into these so the call can return a borrowed view.
    ///
    /// Capped: the plugin decides how many events it emits, so an unbounded
    /// pool would let it provoke a `malloc` in the audio callback.
    pub(super) emitted_midi: RtMidiEvents,
    /// Translates the plugin's MIDI 1.0 output into UMP, keeping its
    /// parameter-number and bank runs across blocks (one event bus is one
    /// Group of its state). Built with the activation, so a deactivation drops
    /// it; [`Vst3Processor::stop`] forgets the runs without sending them.
    ///
    /// Boxed (8 KiB): an `AudioIO` moves by value through activation.
    pub(super) emitted_stream: Box<OutputStream>,
    /// What `emitted_stream` had lost as of the last block, and what it lost in
    /// the block in hand (the change): the events the host reports as lost MIDI.
    pub(super) lost_told: u64,
    pub(super) emitted_lost: usize,
    pub(super) emitted_param_changes: ParameterChanges,
}

impl OutputStaging {
    /// Resets the emitted-event return pools so a borrow into them reads empty.
    /// Called at the top of every `process` block: the bail-out paths return
    /// `emitted_ref()` directly, and the steady-state path then drains the
    /// plugin's fresh events in. Clears in place, keeping heap capacity: the
    /// param queues go back to the set's spares with their point storage.
    pub(super) fn clear_emitted(&mut self) {
        self.emitted_midi.clear();
        self.emitted_lost = 0;
        self.emitted_param_changes.clear();
    }

    /// Borrows the emitted-event return pools as a [`ProcessOutputRef`].
    pub(super) fn emitted_ref(&self) -> ProcessOutputRef<'_> {
        ProcessOutputRef {
            midi_events: &self.emitted_midi,
            midi_lost: self.emitted_lost,
            parameter_changes: &self.emitted_param_changes,
        }
    }

    /// Drains the plugin's emitted events from the COM output lists into the
    /// return pools. Both `fill_*` clear their destination first and reuse its
    /// heap capacity, so this is allocation-free after warmup.
    pub(super) fn drain_emitted(&mut self) {
        drain_midi(
            &self.events,
            &mut self.emitted_stream,
            &mut self.emitted_midi,
        );
        let lost = self.emitted_stream.lost_total();
        self.emitted_lost =
            usize::try_from(lost.saturating_sub(self.lost_told)).unwrap_or(usize::MAX);
        self.lost_told = lost;
        // A refusal is counted in `lists_busy` and leaves the pool empty.
        if !self
            .param_changes
            .fill_changes(&mut self.emitted_param_changes)
        {
            self.emitted_param_changes.clear();
        }
    }
}

/// All the per-block scratch the realtime `process()` loop needs, grouped to
/// mirror `ProcessData`'s own input/output split. Separated from [`Vst3Loaded`](super::Vst3Loaded)
/// so GUI-only hosting doesn't pay the allocation cost.
///
/// `ptrs` is shared: its flat per-channel pointers feed both `input.buses` and
/// `output.buses` each block.
pub(super) struct AudioIO {
    pub(super) config: ProcessConfig,
    /// Typed flat pointer tables for the single committed sample format `T`.
    /// Sized to the per-direction channel total (main + sidechain/aux buses).
    pub(super) input: InputStaging,
    pub(super) output: OutputStaging,
    /// RT-side MIDI-learn CC capture, paired with the
    /// [`Vst3Loaded`](super::Vst3Loaded)'s consumer. Inert (a single
    /// relaxed atomic load) unless learn is armed.
    pub(super) midi_learn: MidiLearnProducer,
}

impl ProcessConfig {
    /// The `ProcessSetup` describing this activation.
    ///
    /// The single source of the setup values, so what `setupProcessing`
    /// receives and what the conformance observer is told cannot drift: a
    /// second, hand-built copy for the observer would agree with `ProcessData`
    /// by construction and hide exactly the mismatch the check exists to find.
    pub(super) fn process_setup(&self, symbolic_sample_size: i32) -> ProcessSetup {
        ProcessSetup {
            processMode: self.setup_mode.to_vst3(),
            symbolicSampleSize: symbolic_sample_size,
            // `activate_typed` refused a block past `i32::MAX`.
            maxSamplesPerBlock: i32::try_from(self.block_size).unwrap_or(i32::MAX),
            // The ABI's `ProcessSetup.sampleRate` is a bare `f64`: the unit ends here.
            sampleRate: self.sample_rate.get(),
        }
    }
}

/// The realtime half of an activation at sample type `T`: it renders blocks and
/// nothing else.
///
/// `Send` to move to the realtime thread, not `Sync`. It reaches the plugin only
/// through its own reference to `IAudioProcessor`, so it never touches the
/// [`Vst3Loaded`](super::Vst3Loaded) its [`Vst3Controller`](super::Vst3Controller)
/// holds. Dropping it ends its share of the activation. The plugin is destroyed
/// when the last half goes, on the control side: a Processor that is the last
/// hands the plugin to a retirement queue that
/// [`Vst3Loaded::reap_retired`](super::Vst3Loaded::reap_retired) empties, so
/// dropping it on the realtime thread does not tear a plugin down there.
pub struct Vst3Processor<T: Vst3Sample> {
    _format: std::marker::PhantomData<fn() -> T>,
    // The scratch is listed first so it drops before the rest.
    pub(super) audio: AudioIO,
    /// Boxed (6 KiB): a Processor moves by value through activation. A block
    /// takes it out of the `Option` and puts it back, which moves a pointer and
    /// allocates nothing; `None` only after a `process` that panicked, whose
    /// blocks are then refused.
    seq: Option<Box<SeqScratch>>,
    /// Sequencer-context texts cut at the room of one event's text since
    /// activation.
    seq_truncated: TallyWriter<u64>,
    audio_side: AudioSide,
    /// What `getProcessContextRequirements` answered at activation; `None` when
    /// the plugin does not implement the interface.
    context_requirements: Option<ProcessContextRequirements>,
    drain: EditReader,
    /// The plugin's `outputParameterChanges` on their way to the Controller:
    /// the last point of each emitted parameter, per block.
    reports: ReportSink,
    /// The writes taken for the block in hand, latest per parameter, and what
    /// a rejoin carried over from the last activation before the first block.
    /// Holds one write per declared parameter and no more: it is reserved at
    /// activation and never grows on the audio thread.
    pending: Vec<(ParamId, Normalized)>,
    /// Writes to a parameter the plugin did not declare or declared read-only,
    /// carried into the activation, drained from the slots or named by a
    /// block's changes: the plugin's count, which its Control reads
    /// ([`Control::refused_edits`](tutti_plugin_types::Control::refused_edits)).
    refused: TallyWriter<u64>,
    /// The parameters the plugin declared at activation and what each declares
    /// of its writability, sorted by id: the write policy
    /// ([`ParamWriteError::check_sorted`]) reads it, so a Controller write or
    /// automation for an undeclared or read-only parameter is not sent. Taken
    /// per activation, and shared with the Controller (which answers writes from
    /// it): a plugin that changes its parameters later is read again at the next
    /// activation.
    declared: Arc<[(ParamId, ParamAccess)]>,
    /// Whether `setProcessing(1)` is the plugin's last word.
    processing: bool,
    /// Last: the plugin outlives everything above, and a last holder retires
    /// it instead of destroying it here.
    shell: Hold,
}

impl<T: Vst3Sample> std::fmt::Debug for Vst3Processor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Processor")
            .field("block_size", &self.audio.config.block_size)
            .field("processing", &self.processing)
            .finish_non_exhaustive()
    }
}

/// Everything a Processor is built from.
pub(super) struct ProcessorParts {
    pub shell: Arc<Shell>,
    pub audio: AudioIO,
    pub audio_side: AudioSide,
    pub context_requirements: Option<ProcessContextRequirements>,
    pub drain: EditReader,
    pub reports: ReportSink,
    /// The plugin's count of refused writes, which the activation already added
    /// the carried ones it refused to.
    pub refused: TallyWriter<u64>,
    pub declared: Arc<[(ParamId, ParamAccess)]>,
}

impl<T: Vst3Sample> Vst3Processor<T> {
    pub(super) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            shell,
            audio,
            audio_side,
            context_requirements,
            drain,
            reports,
            refused,
            declared,
        } = parts;
        let pending = Vec::with_capacity(declared.len());
        Self {
            _format: std::marker::PhantomData,
            audio,
            seq: Some(Box::default()),
            seq_truncated: tally().writer(),
            audio_side,
            context_requirements,
            drain,
            reports,
            pending,
            refused,
            declared,
            processing: true,
            shell: Hold::new(shell),
        }
    }

    /// The shell both halves of this activation hold; `deactivate` compares it.
    pub(super) fn shell(&self) -> &Arc<Shell> {
        self.shell.shell()
    }

    /// Whether `setProcessing(1)` is the plugin's last word: true from
    /// activation, false after [`stop`](Self::stop) until the next block.
    pub fn is_processing(&self) -> bool {
        self.processing
    }

    /// Ends processing (`setProcessing(0)`) if it is on. The next
    /// [`process`](Self::process) starts it again.
    pub fn stop(&mut self) {
        if std::mem::take(&mut self.processing) {
            // A refusal (the plugin is gone or deactivated) leaves nothing to stop.
            let _ = self.audio_side.set_processing(false);
        }
        self.reset_midi_state();
    }

    /// Drops the MIDI state the plugin forgets across a stop or a reset: a SysEx
    /// open in the input list must not complete on the far side of it, and the
    /// stager's notes and MIDI 1.0 selections go with it. [`stop`](Self::stop) runs it
    /// whether or not processing was on, since the state is the host's.
    fn reset_midi_state(&mut self) {
        self.audio.input.events.reset_sysex();
        self.audio.input.stager.reset();
        // Nor does the plugin's output carry a parameter run across it.
        self.audio.output.emitted_stream.reset();
    }

    /// Takes the parameter writes the Controller made that no block has
    /// taken, and those a rejoin carried in, latest per parameter, into `out`. Main
    /// thread, with the Processor held (nothing is rendering): for a caller
    /// that must not lose them, as a rejoin must not.
    pub fn take_edits(&mut self, out: &mut Vec<(ParamId, Normalized)>) {
        self.drain_slots();
        out.extend(self.pending.iter().copied());
        self.pending.clear();
    }

    /// What the plugin's output stream has dropped or evicted since activation:
    /// its note-id table's evictions, expressions on ids no note holds, and
    /// events on a bus past the sixteen Groups.
    #[must_use]
    pub fn output_stream(&self) -> &OutputStream {
        &self.audio.output.emitted_stream
    }

    /// Moves what the slots hold into `pending`, the latest write to each
    /// parameter winning. Allocation-free: `pending` is reserved for every
    /// declared parameter.
    /// Returns the generation the drain swept: what the caller publishes once the
    /// plugin's call returns.
    fn drain_slots(&mut self) -> Generation {
        let Self {
            drain,
            pending,
            refused,
            declared,
            ..
        } = self;
        drain.drain(|id, value| stage_write(pending, refused, declared, id, value));
        drain.swept()
    }

    /// Runs one realtime processing block.
    ///
    /// `events` bundles the MIDI and per-note-expressive streams staged into the
    /// plugin's input event list (sorted by `sample_offset`); chord/scale/text
    /// strings are interned into the event list's arena for the duration of the
    /// call. `param_changes` is forwarded as `inputParameterChanges`;
    /// `transport` populates `ProcessContext`. The returned
    /// [`ProcessOutput`](crate::types::ProcessOutput)
    /// carries any MIDI / parameter-change events the plugin emitted
    /// (plugin-emitted legacy-MIDI-CC-out is decoded to MIDI).
    ///
    /// An empty output if `buffer.frames() == Samples::new(0)`.
    ///
    /// # Errors
    ///
    /// [`ProcessFailed`] when the plugin returns a non-OK `tresult`; the block's
    /// outputs are cleared and what the plugin emitted is dropped.
    ///
    /// Call from the audio thread. Buffers and event lists are sized at
    /// activation, so the steady-state path does not allocate.
    pub fn process(
        &mut self,
        buffer: &mut InputOutput<'_, T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> std::result::Result<ProcessOutputRef<'_>, ProcessFailed> {
        // Reset the return pools up front so the bail-out paths below return a
        // borrow into known-empty state, and the steady-state path drains the
        // plugin's fresh events into them.
        self.audio.output.clear_emitted();

        if buffer.frames() == Samples::new(0) {
            return Ok(self.audio.output.emitted_ref());
        }
        // Processing resumes lazily after a `stop`. (Activation starts it.)
        if !self.processing {
            // `kNotImplemented` is a plugin that has no use for the call, not a
            // failure; any other refusal fails the block (outputs cleared), and
            // `processing` stays off so the next block asks again.
            match self.audio_side.set_processing(true) {
                Ok(()) | Err(tutti_vst3_ffi::Vst3Error::NotImplemented) => self.processing = true,
                Err(error) => {
                    buffer.output().silence();
                    return Err(ProcessFailed { error });
                }
            }
        }
        // The Controller's writes since the last block, put ahead of this
        // block's automation below, so where both set one parameter the
        // automation has the last word.
        let drained = self.drain_slots();

        // MIDI learn (IMidiLearn): when armed, capture each incoming controller
        // the plugin node did not map (a mapped one is a parameter change by
        // now), so the plugin can learn the controllers that have no mapping
        // yet. A single relaxed atomic load when disarmed (the norm).
        self.capture_midi_learn(events.midi);

        // Stage every input event source into the input list: the MIDI the
        // node's controller map did not take, and the rest.
        let list = &self.audio.input.events;
        let staged = if events.is_empty() {
            list.clear()
        } else {
            self.audio.input.stager.stage(list, events)
        };
        if !staged {
            // A plugin thread is inside the input list, which therefore still
            // holds the last block's events: processing now would replay them.
            // The block is silence, nothing is emitted, and the skip is counted
            // (`lists_busy`). The controller's writes wait in `pending`.
            buffer.output().silence();
            return Ok(self.audio.output.emitted_ref());
        }
        if !self.audio.output.events.clear() {
            // The output list is busy the same way: the plugin would append to
            // the last block's output, and the host would read it twice.
            buffer.output().silence();
            return Ok(self.audio.output.emitted_ref());
        }

        // Refill the pooled input/output ParameterChanges wrappers in
        // place instead of building fresh ComWrappers every block. A list a
        // plugin thread still holds is skipped like a busy event list: the
        // block is silence, nothing is emitted, and the skip is counted
        // (`lists_busy`). The controller's writes wait in `pending`.
        let input_params = param_changes.filter(|pc| !pc.is_empty());
        // A queue for a parameter the plugin did not declare or declared
        // read-only is a write it refuses (counted per queue; the list counts
        // the points).
        if let Some(changes) = input_params {
            let refused = changes
                .queues
                .iter()
                .filter(|q| ParamWriteError::check_sorted(q.param_id, &self.declared).is_err())
                .count();
            if refused > 0 {
                self.refused.add(refused as u64);
            }
        }
        let staged = (|| {
            let mut staging = self.audio.input.param_changes.try_begin().ok_or(Busy)?;
            if let Some(changes) = input_params {
                // Automation never writes a parameter the plugin did not
                // declare or declared read-only: its points are counted with
                // the dropped ones.
                staging.refill_where(changes, |id| {
                    ParamWriteError::check_sorted(id, &self.declared).is_ok()
                })?;
            }
            for &(id, value) in self.pending.iter() {
                staging.prepend(id, value)?;
            }
            // The output list must be empty before the plugin appends to it.
            drop(staging);
            let _ = self.audio.output.param_changes.try_begin().ok_or(Busy)?;
            Ok::<(), Busy>(())
        })();
        if staged.is_err() {
            buffer.output().silence();
            return Ok(self.audio.output.emitted_ref());
        }

        let mut process_context = to_process_context(
            transport,
            self.context_requirements
                .unwrap_or(ProcessContextRequirements::all()),
        );
        // `ProcessContext` is the SDK's C struct: the unit stops at its `f64`.
        process_context.sampleRate = self.audio.config.sample_rate.get();

        // The setup comes from `process_setup()`, the same accessor
        // `apply_process_setup` hands the plugin, so a conformance observer
        // comparing the two sees the real negotiated value.
        let setup = self.audio.config.process_setup(T::VST3_SYMBOLIC_SIZE);

        // Bind each direction to this block's channel slices: bus 0 maps onto
        // the live channels, extra input buses get silence, extra output buses
        // a sink. Channels beyond the scratch's flat width are dropped.
        let frames = buffer.frames();
        let (input, mut output) = buffer.split();
        let (Ok(input_table), Ok(mut output_table)) = (
            ChannelTable::of_planes(input),
            ChannelTable::of_planes_mut(&mut output),
        ) else {
            buffer.output().silence();
            return Err(ProcessFailed {
                error: tutti_vst3_ffi::Vst3Error::InvalidArgument,
            });
        };
        let bound = T::bind_inputs(&mut self.audio.input.buses, &input_table).and_then(|inputs| {
            T::bind_outputs(&mut self.audio.output.buses, &mut output_table)
                .map(|outputs| (inputs, outputs))
        });
        let (inputs, outputs) = match bound {
            Ok(buses) => buses,
            Err(error) => {
                buffer.output().silence();
                return Err(ProcessFailed { error });
            }
        };
        // The staged writes reach the plugin only from here: every return
        // before this keeps them in `pending` for the next block, so the
        // applied mark below never covers a write the plugin was not handed.
        self.pending.clear();
        let result = self.audio_side.process(ProcessIo {
            mode: self.audio.config.block_mode,
            num_samples: frames.get(),
            inputs,
            outputs,
            input_events: &self.audio.input.events,
            output_events: &self.audio.output.events,
            input_params: &self.audio.input.param_changes,
            output_params: &self.audio.output.param_changes,
            context: &mut process_context,
            setup: &setup,
        });

        // The plugin has the values the drain took once its call returned,
        // whatever it answered; a block that left before the call leaves them
        // in `pending` for the next one.
        self.drain.publish_applied(drained);
        if let Err(error) = result {
            buffer.output().silence();
            return Err(ProcessFailed { error });
        }

        self.audio.output.drain_emitted();
        self.report_emitted();
        Ok(self.audio.output.emitted_ref())
    }

    /// Hands the Controller what the plugin put in `outputParameterChanges`:
    /// the last value of each parameter this block, to be applied there with
    /// `setParamNormalized` on the main thread
    /// ([`flush_edits`](tutti_plugin_types::Control::flush_edits)). The portal's
    /// "Processor and Controller" section has the host transmit them; a meter,
    /// a processor-driven parameter or a program change reaches the controller
    /// no other way. A full ring drops the value; the
    /// next block's value for the parameter replaces it. Never allocates.
    fn report_emitted(&mut self) {
        for queue in &self.audio.output.emitted_param_changes.queues {
            if let Some(point) = queue.points.last() {
                let _ = self.reports.send(ParamEdit::change(
                    queue.param_id,
                    Normalized::clamped(point.value.get()),
                ));
            }
        }
    }

    /// Delivers parameter changes to a plugin that is active but not
    /// processing, with a `process` call that carries no audio: `numSamples`
    /// is 0 and no bus is passed, only the parameter list (the portal's FAQ,
    /// "Processing": "the host must flush parameter changes periodically ... by
    /// sending them to the processor via the process call (with audio buffer
    /// set to nullptr)").
    ///
    /// It runs only while [`is_processing`](Self::is_processing) is false (after
    /// [`stop`](Self::stop), and so also from the stop that
    /// [`reset`](tutti_plugin_types::Processor::reset) makes): it takes what
    /// the Controller queued and calls `process` once if there is any. A call
    /// while processing does nothing, since the next block carries the writes.
    /// The plugin is not told `setProcessing(true)`: it was stopped on
    /// purpose, and the next real block starts it.
    ///
    /// Whoever holds the Processor while nothing renders calls this (the
    /// control side between a stop and a rejoin, or the graph node's `stop`
    /// path); [`stop`](Self::stop) itself does not, so that a stop stays one
    /// call. Returns whether a flush call was made.
    ///
    /// # Errors
    ///
    /// [`ProcessFailed`] when the plugin refuses the call; the changes are
    /// then lost, as for a refused block.
    pub fn flush_parameters(&mut self) -> std::result::Result<bool, ProcessFailed> {
        if self.processing {
            return Ok(false);
        }
        let drained = self.drain_slots();
        if self.pending.is_empty() {
            self.drain.publish_applied(drained);
            return Ok(false);
        }
        let Some(mut staging) = self.audio.input.param_changes.try_begin() else {
            return Ok(false);
        };
        for &(id, value) in self.pending.iter() {
            if staging.prepend(id, value).is_err() {
                return Ok(false);
            }
        }
        drop(staging);
        if self.audio.output.param_changes.try_begin().is_none() {
            return Ok(false);
        }
        self.pending.clear();
        let result = self.audio_side.flush(FlushIo {
            mode: self.audio.config.block_mode,
            input_params: &self.audio.input.param_changes,
            output_params: &self.audio.output.param_changes,
        });
        self.drain.publish_applied(drained);
        result
            .map(|()| true)
            .map_err(|error| ProcessFailed { error })
    }

    /// Delivers `changes` between blocks, the Controller's writes first. While
    /// the plugin is processing, with a `process` call that carries no audio
    /// (`numSamples = 0`, the SDK's way to flush parameters): the render thread
    /// that calls it renders no block meanwhile. Before it processes (no block
    /// yet, or after a stop) the SDK has no flush to offer, since `process`
    /// follows `setProcessing(true)` and this does not toggle it: the writes are
    /// staged for the next block's input changes.
    ///
    /// # Errors
    ///
    /// [`ProcessFailed`] when the plugin refuses the call, or a list a plugin
    /// thread holds cannot be filled (nothing was delivered).
    pub fn flush_changes(
        &mut self,
        changes: &ParameterChanges,
    ) -> std::result::Result<(), ProcessFailed> {
        use tutti_vst3_ffi::Vst3Error;
        let drained = self.drain_slots();
        if !self.processing {
            // The SDK has the host call `setProcessing(true)` before `process`,
            // and a flush must not toggle it: the writes wait in `pending` and
            // the next block's input changes carry them at offset 0, after what
            // the Controller queued.
            let Self {
                pending,
                refused,
                declared,
                ..
            } = self;
            for queue in &changes.queues {
                if let Some(point) = queue.points.last() {
                    stage_write(pending, refused, declared, queue.param_id, point.value);
                }
            }
            return Ok(());
        }
        let Some(mut staging) = self.audio.input.param_changes.try_begin() else {
            return Err(ProcessFailed {
                error: Vst3Error::NotInitialized,
            });
        };
        let declared = &self.declared;
        // A queue for a parameter the plugin did not declare, or declared
        // read-only, is a write it refuses; the list counts its points dropped.
        let refused = changes
            .queues
            .iter()
            .filter(|q| ParamWriteError::check_sorted(q.param_id, declared).is_err())
            .count();
        if refused > 0 {
            self.refused.add(refused as u64);
        }
        let staged = staging
            .refill_where(changes, |id| {
                ParamWriteError::check_sorted(id, declared).is_ok()
            })
            .is_ok()
            && self
                .pending
                .iter()
                .all(|&(id, value)| staging.prepend(id, value).is_ok());
        drop(staging);
        if !staged || self.audio.output.param_changes.try_begin().is_none() {
            return Err(ProcessFailed {
                error: Vst3Error::NotInitialized,
            });
        }
        self.pending.clear();
        let result = self.audio_side.flush(FlushIo {
            mode: self.audio.config.block_mode,
            input_params: &self.audio.input.param_changes,
            output_params: &self.audio.output.param_changes,
        });
        self.drain.publish_applied(drained);
        result.map_err(|error| ProcessFailed { error })
    }

    /// Feed incoming MIDI CC controllers to the MIDI-learn capture (for
    /// `IMidiLearn`). When learn is disarmed — the common case — this is a single
    /// relaxed atomic load and an immediate return, so the hot path pays almost
    /// nothing. When armed, each mappable controller (CC / channel-pressure /
    /// pitch-bend) is decoded to its VST3 `(channel, controller)` and captured
    /// allocation-free; the value is irrelevant to learning and dropped.
    #[inline]
    fn capture_midi_learn(&self, midi_events: &[UmpAt]) {
        // Cheap bail when disarmed (the norm): avoid even decoding the MIDI.
        if !self.audio.midi_learn.is_armed() {
            return;
        }
        for event in midi_events {
            if let Some((channel, controller)) = learnable_controller(event.1) {
                self.audio.midi_learn.capture(channel, controller);
            }
        }
    }

    /// Blocks skipped (rendered as silence, nothing emitted) or lists left
    /// unread since activation because a plugin thread was inside a call on
    /// the input or output event list (`getEventCount`, `getEvent`,
    /// `addEvent`) or parameter list or one of its queues (`getParameterData`,
    /// `getPoint`, `addPoint`, ...) when the audio thread needed it. Zero for
    /// a plugin that keeps to the threading rules; the host never waits on
    /// the plugin.
    pub fn lists_busy(&self) -> u64 {
        self.audio.input.events.busy()
            + self.audio.output.events.busy()
            + self.audio.input.param_changes.busy()
            + self.audio.output.param_changes.busy()
    }

    /// The input parameter list, shared: a conformance test holds one of its
    /// queues
    /// ([`ParameterChangesImpl::hold_queue_as_plugin`](tutti_vst3_ffi::ParameterChangesImpl::hold_queue_as_plugin))
    /// as a plugin worker thread inside `getPoint` does, while the processor
    /// renders.
    #[cfg(feature = "conformance")]
    pub fn input_param_list(&self) -> vst3::ComWrapper<tutti_vst3_ffi::ParameterChangesImpl> {
        self.audio.input.param_changes.clone()
    }

    /// The input event list, shared: a conformance test holds it
    /// ([`EventList::hold_as_plugin`](tutti_vst3_ffi::EventList::hold_as_plugin))
    /// as a plugin thread inside `getEvent` does, while the processor renders.
    #[cfg(feature = "conformance")]
    pub fn input_event_list(&self) -> vst3::ComWrapper<tutti_vst3_ffi::EventList> {
        self.audio.input.events.clone()
    }

    /// Texts (chord, scale or note-expression text) dropped since activation
    /// because the block's event-list text arena was full, input and output
    /// lists together. Each list reserves its arena once and never grows it,
    /// because the plugin keeps pointers into it for the whole `process`
    /// call; a text past the room is staged as an empty text and counted here.
    pub fn event_text_dropped(&self) -> usize {
        self.audio.input.events.text_dropped() + self.audio.output.events.text_dropped()
    }

    /// Input events not handed to the plugin since activation because the
    /// block staged more than the input event list's reserved room (MIDI,
    /// note expressions, chords, scales and per-note texts together). The
    /// list never grows on the audio thread; past its room a note-off still
    /// takes the place of an earlier event, so no note is left hanging
    /// unless the list is full of note-offs, and every other event is
    /// dropped. Per-note expression values past their own budget (a MIDI
    /// block's worth) are dropped and counted here too. Each lost event is counted once. Zero
    /// while a block stays within the room. Read on this instance only, like
    /// [`param_points_dropped`](Self::param_points_dropped).
    pub fn events_dropped(&self) -> u64 {
        self.audio.input.events.dropped()
    }

    /// SysEx messages not handed to the plugin since this Processor was built
    /// (an activation builds a new one, so a re-activation starts at zero),
    /// each dropped whole rather than truncated: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// left without their `END` by a later `START`, open when the plugin was
    /// stopped ([`stop`](Self::stop)), or cut by more than a block's MIDI cap routed through the
    /// plugin's CC mapping. The input list
    /// reassembles a multi-packet SysEx (across blocks if it spans them) and
    /// hands the plugin one complete `0xF0 … 0xF7` message at its `END`.
    /// Read on this instance only, like
    /// [`events_dropped`](Self::events_dropped).
    pub fn sysex_dropped(&self) -> u64 {
        self.audio.input.events.sysex_dropped()
    }

    /// MIDI messages not handed to the plugin since this Processor was built
    /// because they were of a Group it has no event input bus for (Group `n` is
    /// event bus `n`): each is dropped whole and counted, never heard on another
    /// Group's channels. For a plugin with no event input bus at all, every
    /// event offered (of any kind) is counted here too: both share this one
    /// counter. Read on this instance only, like
    /// [`sysex_dropped`](Self::sysex_dropped).
    pub fn groups_dropped(&self) -> u64 {
        self.audio.input.stager.groups_dropped()
    }

    /// MIDI messages not handed to the plugin since this Processor was built
    /// because VST3 has no event for them: a controller, channel pressure,
    /// pitch bend, program change, RPN or NRPN that no `IMidiMapping`
    /// parameter took, and a System message. Each is dropped whole and
    /// counted: `DataEvent` has the one type `kMidiSysEx`, so the bytes are
    /// never sent as one (ivstevents.h).
    pub fn unsendable_dropped(&self) -> u64 {
        self.audio.input.stager.unsendable_dropped()
    }

    /// Events the plugin emitted through `addEvent` that its output list
    /// refused (with `kResultFalse`) or that gave way to a later note-off,
    /// since activation: past the room the list reserved, as for
    /// [`events_dropped`](Self::events_dropped).
    pub fn output_events_dropped(&self) -> u64 {
        self.audio.output.events.dropped()
    }

    /// Sequencer-context texts (chord, scale and note-expression text) cut
    /// since activation because they were longer than the room of one event's
    /// text ([`MAX_EVENT_TEXT_LEN`](tutti_vst3_ffi::MAX_EVENT_TEXT_LEN) UTF-16
    /// units). The plugin gets the text up to the cut, on a character boundary.
    pub fn sequencer_texts_truncated(&self) -> u64 {
        self.seq_truncated.get()
    }

    /// Host parameter points not handed to the plugin whole since
    /// activation: the input list holds a chunk's points as the host's chunk
    /// does (a few per parameter and a shared pool of a chunk's room), and a
    /// point past a queue's room, or past its own few with the pool spent,
    /// gives way to its parameter's latest value and is counted here, as is
    /// one the `IMidiMapping` route's merge of host automation and mapped
    /// controllers could not hold. Zero while the host stays within the
    /// room the instance was activated for. Read on this instance only: the
    /// plugin-server does not report it (as it does not report the CLAP
    /// host's `param_points_dropped`).
    pub fn param_points_dropped(&self) -> u64 {
        self.audio.input.param_changes.points_dropped()
    }

    /// Points the plugin emitted that the host could not hold whole since
    /// activation: past an output queue's `MIN_POINT_ROOM` as the plugin
    /// added them (`addPoint`), or past it in the drained output set with
    /// the set's pool spent. Each time the parameter kept its latest value.
    /// Read on this instance only, like
    /// [`param_points_dropped`](Self::param_points_dropped).
    pub fn output_param_points_dropped(&self) -> u64 {
        self.audio.output.emitted_param_changes.points_overflowed()
            + self.audio.output.param_changes.points_dropped()
    }

    /// Toggles between [`Realtime`](ProcessMode::Realtime) and
    /// [`Prefetch`](ProcessMode::Prefetch) on a live instance, without
    /// re-running `setupProcessing`.
    ///
    /// This pair is the sole per-block mode change the VST3 spec allows: the
    /// `ProcessSetup`/`ProcessData` agreement rule in
    /// `ProcessSetupCheck::check` names it as an explicit exception, so only
    /// `ProcessData::processMode` moves here and the negotiated setup is left
    /// alone. Returns `false` — changing nothing — when the instance was
    /// activated [`Offline`](ProcessMode::Offline), because reaching it from
    /// either of these needs a fresh setup and the resulting disagreement would
    /// be a spec violation. Activate again with another
    /// [`Prepare::mode`](tutti_plugin_types::Prepare::mode) to change an
    /// offline instance's mode.
    ///
    /// Must not be called from inside [`process`](Self::process).
    pub fn set_prefetch(&mut self, prefetch: bool) -> bool {
        let requested = if prefetch {
            ProcessMode::Prefetch
        } else {
            ProcessMode::Realtime
        };
        // Gate on the *negotiated setup* mode, not the current block mode: it
        // is the setup value every block is checked against, and it is what an
        // offline activation pinned.
        if !self.audio.config.setup_mode.switchable_to(requested) {
            return false;
        }
        self.audio.config.block_mode = requested;
        true
    }

    /// The mode this instance's blocks are currently processed in.
    pub fn process_mode(&self) -> ProcessMode {
        self.audio.config.block_mode
    }
}

/// The trait's Processor: the sample type is chosen at activation, so the two
/// typed processors sit in one enum and `process` matches the buffer's width to
/// the arm.
#[derive(Debug)]
pub enum ProcessorAny {
    /// Activated at 32-bit.
    F32(Vst3Processor<f32>),
    /// Activated at 64-bit; the plugin said it can process it.
    F64(Vst3Processor<f64>),
}

impl ProcessorAny {
    pub(super) fn shell(&self) -> &Arc<Shell> {
        match self {
            Self::F32(p) => p.shell(),
            Self::F64(p) => p.shell(),
        }
    }

    /// [`Vst3Processor::is_processing`].
    pub fn is_processing(&self) -> bool {
        match self {
            Self::F32(p) => p.is_processing(),
            Self::F64(p) => p.is_processing(),
        }
    }

    /// [`Vst3Processor::take_edits`].
    pub fn take_edits(&mut self, out: &mut Vec<(ParamId, Normalized)>) {
        match self {
            Self::F32(p) => p.take_edits(out),
            Self::F64(p) => p.take_edits(out),
        }
    }

    /// [`Vst3Processor::flush_parameters`].
    ///
    /// # Errors
    ///
    /// [`ProcessFailed`] when the plugin refuses the call.
    pub fn flush_parameters(&mut self) -> std::result::Result<bool, ProcessFailed> {
        match self {
            Self::F32(p) => p.flush_parameters(),
            Self::F64(p) => p.flush_parameters(),
        }
    }

    /// Whether this processor renders 64-bit audio.
    #[must_use]
    pub fn is_f64(&self) -> bool {
        matches!(self, Self::F64(_))
    }

    /// Renders one block, with the buffers, the context and the output taken
    /// apart. [`Processor::process`] is this with a [`PluginIo`].
    ///
    /// # Errors
    ///
    /// [`PluginError::Block`] when the plugin failed the block or the buffer's
    /// sample format is not the one the processor was activated with. Neither
    /// allocates.
    pub fn render<S: Float>(
        &mut self,
        buffer: &mut InputOutput<'_, S>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        match (self, buffer.by_width()) {
            (Self::F32(p), ByWidth::F32(buf)) => p.process_shared(buf, ctx, out),
            (Self::F64(p), ByWidth::F64(buf)) => p.process_shared(buf, ctx, out),
            _ => Err(PluginError::Block(
                "the block's sample format is not the one the plugin was activated with",
            )),
        }
    }
}

impl Processor for ProcessorAny {
    /// The input list's MIDI share is [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) events whatever
    /// the other sources hold (`EVENT_CAPACITY` less the expression and
    /// harmony rooms), the node's own intake size. A packet that encodes to
    /// several VST3 events (a note with per-note values held, a bend
    /// sensitivity change re-sending each live note's bend) is staged whole or
    /// not at all and counted, so a block of such packets can overflow the
    /// share inside this bound.
    fn event_input(&self) -> std::num::NonZeroU32 {
        tutti_plugin_types::MIDI_STACK_BOUND
    }

    fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    fn stop(&mut self) {
        match self {
            Self::F32(p) => p.stop(),
            Self::F64(p) => p.stop(),
        }
    }

    /// `process` with no audio ([`Vst3Processor::flush_changes`]).
    fn flush_parameters(&mut self, changes: &ParameterChanges) -> PluginResult<()> {
        match self {
            Self::F32(p) => p.flush_changes(changes),
            Self::F64(p) => p.flush_changes(changes),
        }
        .map_err(|_| PluginError::Block("the plugin refused a parameter flush"))
    }

    /// `setProcessing(false)`, started again by the next block: what the SDK
    /// offers for a reset (`ivstaudioprocessor.h`: it "could be used to reset
    /// some buffers (like Delay line or Reverb)"), with the SysEx reassembly
    /// dropped as [`stop`](Vst3Processor::stop) drops it.
    fn reset(&mut self) {
        Processor::stop(self);
    }
}

impl<T: Vst3Sample> Vst3Processor<T> {
    /// One block from the shared vocabulary: converts the context into VST3's
    /// (the sequencer streams into scratch, transport on the stack), processes,
    /// and copies what the plugin emitted into `out`.
    fn process_shared(
        &mut self,
        buffer: &mut InputOutput<'_, T>,
        ctx: &tutti_plugin_types::ProcessContext<'_>,
        out: &mut tutti_plugin_types::ProcessOutput,
    ) -> PluginResult<()> {
        let transport = ctx.transport.cloned().unwrap_or_else(|| {
            tutti_plugin_types::TransportInfo::new(self.audio.config.sample_rate)
        });
        // The shared and VST3 note-expression values are one type, so this is a
        // borrow, not a conversion.
        let note_expressions = ctx
            .note_expression
            .map(|n| n.changes.as_slice())
            .unwrap_or_default();
        // Refilled into `seq`, inline storage that never grows. An absent
        // stream leaves its buffer empty, which is what the plugin should see.
        let expressive = ctx.expressive.as_ref();
        let Some(mut seq) = self.seq.take() else {
            return Err(PluginError::Block(
                "an earlier block panicked in the plugin's process",
            ));
        };
        fill_chords(
            expressive.and_then(|e| e.chords),
            &mut seq.chords,
            &self.seq_truncated,
        );
        fill_scales(
            expressive.and_then(|e| e.scales),
            &mut seq.scales,
            &self.seq_truncated,
        );
        fill_expr_texts(
            expressive.and_then(|e| e.expr_texts),
            &mut seq.expr_texts,
            &self.seq_truncated,
        );
        let events = InputEvents {
            midi: ctx.midi_events,
            note_expressions,
            chords: seq.chords.as_slice(),
            scales: seq.scales.as_slice(),
            expr_texts: seq.expr_texts.as_slice(),
            expr_ints: expr_ints_of(expressive),
        };
        let result = match self.process(buffer, &events, ctx.param_changes, &transport) {
            Ok(output) => {
                copy_process_output(output, out);
                Ok(())
            }
            Err(ProcessFailed {
                error: tutti_vst3_ffi::Vst3Error::Poisoned,
            }) => Err(PluginError::Block(
                "host code panicked while holding the plugin; no block runs on it again",
            )),
            Err(_) => Err(PluginError::Block("the plugin's process returned an error")),
        };
        self.seq = Some(seq);
        result
    }
}

/// Stages a write for the next block's input changes: the latest to each
/// parameter wins, and one to a parameter the plugin did not declare (or past the
/// room) is counted dropped. Allocation-free: `pending` is reserved for every
/// declared parameter.
fn stage_write(
    pending: &mut Vec<(ParamId, Normalized)>,
    dropped: &TallyWriter<u64>,
    declared: &[(ParamId, ParamAccess)],
    id: ParamId,
    value: Normalized,
) {
    if ParamWriteError::check_sorted(id, declared).is_err() {
        dropped.add(1);
    } else if let Some(slot) = pending.iter_mut().find(|(waiting, _)| *waiting == id) {
        slot.1 = value;
    } else if pending.len() < declared.len() {
        pending.push((id, value));
    } else {
        dropped.add(1);
    }
}

/// Copies one block's emitted events out of the host's pooled buffers into the
/// caller's reusable [`ProcessOutput`](tutti_plugin_types::ProcessOutput).
///
/// `out` arrives cleared, with its heap capacity intact, so every copy below
/// writes into storage that already exists: past its cap the MIDI is dropped
/// and counted. VST3 note-expression output is not read back (the input side
/// carries it inbound only), so `out`'s stays empty.
fn copy_process_output(output: ProcessOutputRef<'_>, out: &mut tutti_plugin_types::ProcessOutput) {
    out.emit_midi(output.midi_events.iter().copied());
    out.count_midi_dropped(output.midi_lost);
    for queue in &output.parameter_changes.queues {
        for point in &queue.points {
            // `emit_param_point`, not `ParameterChanges::add_change`: the former
            // draws its queues from `out`'s own pool, so a warm block reuses the
            // `points` buffers rather than allocating one per automated
            // parameter.
            out.emit_param_point(queue.param_id, point.sample_offset, point.value);
        }
    }
}

// A Processor moves by value through activation (`Staged`, `Lifecycle`, the
// server's `Server`), and every debug frame on that path holds a copy. Its
// large buffers are boxed, so it stays small; a new inline array that tips
// this bound fails the build.
const _: () = assert!(size_of::<ProcessorAny>() <= 4096);
const _: () = assert!(size_of::<super::instance::Staged<f32>>() <= 4096);
