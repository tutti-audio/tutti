//! The audio path of a [`ClapProcessor`]: one block through the plugin.

use super::ClapProcessor;
use super::config::AudioScratch;
use crate::error::{ClapError, Result};
use crate::events::{ClapEvent, EventList, ParamBounds};
use crate::types::{ClapNoteExpression, ParameterChanges, TransportInfo};
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING,
    CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_transport,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::process::{CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL, clap_process_status};
use std::sync::atomic::Ordering;
use tutti_buffers::FloatWidth;
use tutti_buffers::{ByWidth, Float, InputOutput};
use tutti_clap_ffi::{Planar, ProcessCall, ProcessError};
use tutti_graph::UmpAt;
use tutti_plugin_types::TransportChange;
use tutti_plugin_types::WireMidi;
use tutti_plugin_types::epoch_before;
use tutti_rt::CappedVec;
use tutti_units::{Normalized, ParamId};

/// Owned snapshot of the plugin's per-block output. Returned for
/// non-RT consumers (tests, offline render) via
/// [`ProcessOutputRef::into_owned`]; the hot path returns
/// [`ProcessOutputRef`] borrowing the instance's pooled buffers instead.
#[derive(Debug, Clone, Default)]
pub struct ProcessOutput {
    /// MIDI the plugin emitted during the block, in event order.
    pub midi_events: Vec<WireMidi>,
    /// Parameter values the plugin changed itself, e.g. from its own GUI.
    pub param_changes: ParameterChanges,
    /// Note expressions the plugin emitted.
    pub note_expressions: Vec<ClapNoteExpression>,
}

/// Borrowing view of the plugin's per-block output. Points into the
/// [`ClapProcessor`]'s pooled return-value buffers — valid until the next
/// `process` call, which clears them in place. RT-safe.
#[derive(Debug, Clone, Copy)]
pub struct ProcessOutputRef<'a> {
    /// MIDI the plugin emitted during the block, in event order.
    pub midi_events: &'a [WireMidi],
    /// MIDI the plugin emitted that is not in `midi_events`: events the pool had
    /// no room for, from a port no Group has, or with a value that is not a number.
    pub midi_lost: usize,
    /// Parameter values the plugin changed itself, e.g. from its own GUI.
    pub param_changes: &'a ParameterChanges,
    /// Note expressions the plugin emitted.
    pub note_expressions: &'a [ClapNoteExpression],
}

impl<'a> ProcessOutputRef<'a> {
    /// Snapshots into an owned [`ProcessOutput`]. Allocates; off-RT only.
    #[must_use]
    pub fn into_owned(self) -> ProcessOutput {
        ProcessOutput {
            midi_events: self.midi_events.to_vec(),
            param_changes: self.param_changes.clone(),
            note_expressions: self.note_expressions.to_vec(),
        }
    }
}

/// All inputs for a single process call. Use `..Default::default()` to fill
/// fields you don't need — compiles to zero-cost empty slices and None.
///
/// ```no_run
/// # use tutti_fixtures::midi;
/// # use tutti_ump::{MidiChannel, NoteVelocity};
/// # use tutti_graph::Offset;
/// # use tutti_buffers::InputOutput;
/// # use tutti_clap_host::{ClapProcessor, ClapProcessContext, TransportInfo};
/// # use tutti_units::{Bpm, SampleRate};
/// # fn ex(plugin: &mut ClapProcessor, buffer: &mut InputOutput<'_, f32>)
/// # -> tutti_clap_host::Result<()> {
/// let transport = TransportInfo::new(SampleRate::SR_48K).tempo(Bpm::of(120.0));
/// let midi = [(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 16384))];
/// plugin.process(buffer, &ClapProcessContext {
///     midi: &midi,
///     transport: Some(&transport),
///     ..Default::default()
/// })?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ClapProcessContext<'a> {
    /// MIDI to deliver during this block. Sorted by time before being handed
    /// to the plugin, so the caller need not pre-sort.
    pub midi: &'a [UmpAt],
    /// Parameter changes to apply during this block, as normalized `0..=1`
    /// values. They are denormalized against the plugin's declared ranges on
    /// the way in; changes for an id the plugin never described are dropped
    /// when the plugin reports parameters at all.
    pub params: Option<&'a ParameterChanges>,
    /// Per-voice note expressions to deliver during this block.
    pub expressions: &'a [ClapNoteExpression],
    /// Transport snapshot for the block. `None` sends no transport event, so
    /// the plugin sees a host with no timeline rather than a stopped one.
    pub transport: Option<&'a TransportInfo>,
    /// The transport changes inside the block, in time order, each at its
    /// frame: one `CLAP_EVENT_TRANSPORT` per change, in time order with the
    /// block's other input events (`clap_event_transport::tempo_inc` is "until
    /// the next transport event", so a plugin follows a tempo that changes
    /// part-way). Sent only with a [`transport`](Self::transport); a change
    /// at frame 0 or past the block is clamped into it. At most the graph's
    /// `MAX_TRANSPORT_CHANGES` are taken, the room reserved for them.
    pub transport_changes: &'a [TransportChange],
}

impl ClapProcessor {
    /// Processes one block of audio through the plugin.
    ///
    /// The sample format is a value fixed at activation
    /// ([`format`](Self::format)): the block's samples must be the
    /// same width, matched once per block, and a block of the other width is
    /// refused ([`ClapError::FormatMismatch`]) before the plugin is called. (The
    /// 64-bit support check happened once in `Loaded::activate`.)
    ///
    /// Call from the audio thread. The first call after activation (or after
    /// a [`stop`](Self::stop)) also runs the plugin's `start_processing`. The
    /// call does not allocate: event lists and channel pointers live in scratch
    /// sized at activation. The parameter edits the Controller queued are
    /// drained first and sent to the plugin at the head of the block, before
    /// `ctx`'s automation. The returned [`ProcessOutputRef`] borrows the MIDI,
    /// parameter changes and note expressions the plugin emitted; it is valid
    /// until the next call.
    ///
    /// # Errors
    ///
    /// - [`ClapError::FormatMismatch`] if the buffer's width is not the one the
    ///   processor was activated with.
    /// - [`ClapError::BlockTooLarge`] if the buffer holds more frames than the
    ///   instance was activated for; activate again with a larger
    ///   `Prepare::max_block` off the audio thread.
    /// - [`ClapError::NotActive`] if the Controller has been dropped.
    /// - [`ClapError::StartProcessingFailed`] if the plugin refuses
    ///   `start_processing`.
    /// - [`ClapError::PluginReturnedError`] if the plugin returns
    ///   `CLAP_PROCESS_ERROR`; the output channels are zeroed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tutti_fixtures::midi;
    /// # use tutti_ump::{MidiChannel, NoteVelocity};
    /// # use tutti_graph::Offset;
    /// # use tutti_buffers::InputOutput;
    /// # use tutti_clap_host::{ClapProcessor, ClapProcessContext, TransportInfo};
    /// # use tutti_units::{Bpm, SampleRate};
    /// # fn ex(active: &mut ClapProcessor, buffer: &mut InputOutput<'_, f32>)
    /// # -> tutti_clap_host::Result<()> {
    /// let transport = TransportInfo::new(SampleRate::SR_48K).tempo(Bpm::of(120.0));
    /// let midi = [(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 16384))];
    /// active.process(buffer, &ClapProcessContext {
    ///     midi: &midi,
    ///     transport: Some(&transport),
    ///     ..Default::default()
    /// })?;
    /// # Ok(()) }
    /// ```
    pub fn process<S: Float>(
        &mut self,
        buffer: &mut InputOutput<'_, S>,
        ctx: &ClapProcessContext<'_>,
    ) -> Result<ProcessOutputRef<'_>> {
        let empty_params = ParameterChanges::new();
        let params = ctx.params.unwrap_or(&empty_params);
        match (self.format(), buffer.by_width()) {
            (FloatWidth::F32, ByWidth::F32(io)) => {
                let frames = io.frames();
                let (inputs, outputs) = io.split();
                self.process_impl(
                    Planar::F32 { inputs, outputs },
                    frames.get(),
                    ctx.midi,
                    params,
                    ctx.expressions,
                    ctx.transport,
                    ctx.transport_changes,
                )
            }
            (FloatWidth::F64, ByWidth::F64(io)) => {
                let frames = io.frames();
                let (inputs, outputs) = io.split();
                self.process_impl(
                    Planar::F64 { inputs, outputs },
                    frames.get(),
                    ctx.midi,
                    params,
                    ctx.expressions,
                    ctx.transport,
                    ctx.transport_changes,
                )
            }
            _ => Err(ClapError::FormatMismatch),
        }
    }

    /// Parameter points the host sent past the room this instance was
    /// activated for (every parameter's points for a block of
    /// the block size it was activated for): each replaced its
    /// parameter's previous point in the block, or was lost when there was
    /// none. Zero while the host stays within it.
    pub fn param_points_dropped(&self) -> u64 {
        self.scratch.input_events.param_points_dropped()
    }

    /// SysEx messages not handed to the plugin since this instance was
    /// activated (a [`reset`](Self::reset) does not zero it), each dropped whole rather than truncated:
    /// longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// left without their `END` by a later `START`, or open at a reset. The input list
    /// reassembles a multi-packet SysEx (across blocks if it spans them) and
    /// hands the plugin one `CLAP_EVENT_MIDI_SYSEX`, `0xF0 … 0xF7`, at its
    /// `END`. Read on this instance only, like
    /// [`param_points_dropped`](Self::param_points_dropped).
    pub fn sysex_dropped(&self) -> u64 {
        self.scratch.input_events.sysex_dropped()
    }

    /// MIDI messages not handed to the plugin since this instance was
    /// activated because they were of a Group it has no input note port for
    /// (Group `n` is note port `n`): each is dropped whole and counted, never
    /// heard on another Group's channels. Read on this instance only, like
    /// [`sysex_dropped`](Self::sysex_dropped).
    pub fn groups_dropped(&self) -> u64 {
        self.scratch.input_events.groups_dropped()
    }

    /// MIDI events not handed to the plugin since this instance was activated,
    /// for lack of the room reserved at activation (a Program Change with a
    /// bank counts its three, all dropped together). Read on this instance
    /// only, like [`sysex_dropped`](Self::sysex_dropped).
    pub fn midi_dropped(&self) -> u64 {
        self.scratch.input_events.midi_dropped()
    }

    /// Parameter edits, note expressions and transport changes not handed to
    /// the plugin since this instance was activated, each past the room
    /// reserved for its source. Read on this instance only, like
    /// [`midi_dropped`](Self::midi_dropped).
    pub fn input_events_refused(&self) -> u64 {
        self.scratch.input_events.refused()
    }

    /// Parameter points the plugin emitted past the room this instance was
    /// activated for (one queue per parameter, each with room for a block of
    /// the block size it was activated for's points): each replaced
    /// its parameter's previous point in the block, or was lost when no queue
    /// was left for its parameter. Counted over this instance's life; zero
    /// while the plugin stays within it.
    pub fn output_param_points_dropped(&self) -> u64 {
        self.scratch.out_param_points_dropped
    }

    /// Output events the plugin pushed past their room, each refused: the
    /// plugin's `try_push` returned `false` rather than the host growing its
    /// list on the audio thread. The rooms are separate: 256 events other
    /// than parameter values (notes, MIDI, expressions), so a parameter flood
    /// never refuses a note-off, and every parameter's points for a block of
    /// the block size it was activated for. Past the parameter room
    /// a value for one of the plugin's own parameters is not refused but kept
    /// as that parameter's latest (the points it displaces are counted in
    /// [`output_param_points_dropped`](Self::output_param_points_dropped));
    /// one for an unknown id is refused. Counted over this instance's life;
    /// zero while the plugin stays within its rooms.
    pub fn output_events_refused(&self) -> u64 {
        self.scratch.output_events.events_refused()
    }

    /// The raw `clap_process_status` the plugin returned on the most recent
    /// block, or `CLAP_PROCESS_CONTINUE` if none has run yet.
    ///
    /// Raw rather than a host enum: CLAP leaves the status space open, so a
    /// plugin built against a newer header can return a value this host has
    /// never heard of, and an enum would have to bucket it away — the one thing
    /// a caller diagnosing that plugin wants. Compare against
    /// `clap_sys::process::CLAP_PROCESS_*`.
    ///
    /// Reads a `Relaxed` atomic: callable from any thread, promising only "the
    /// value from some recent block". Polling from a control thread is the
    /// intended use.
    pub fn last_process_status(&self) -> clap_process_status {
        self.scratch.last_process_status.load(Ordering::Relaxed)
    }

    /// Whether notes go to the plugin as `clap_event_midi`, as read from its
    /// note port at the last (re)activation: see [`crate::ClapLoaded::sends_midi_notes`].
    pub fn sends_midi_notes(&self) -> bool {
        self.scratch.input_events.supports_midi_notes()
    }

    /// Whether the plugin's most recent block reported `CLAP_PROCESS_TAIL` —
    /// it produced no new signal but is still decaying, so the host should keep
    /// calling `process` until the tail runs out.
    pub fn is_tailing(&self) -> bool {
        self.last_process_status() == CLAP_PROCESS_TAIL
    }

    /// Whether the plugin's most recent block reported `CLAP_PROCESS_SLEEP` —
    /// it is idle and produced silence, so the host may stop calling `process`
    /// until it has new input to deliver.
    pub fn is_sleeping(&self) -> bool {
        self.last_process_status() == CLAP_PROCESS_SLEEP
    }

    fn process_impl(
        &mut self,
        audio: Planar<'_, '_>,
        frames: usize,
        midi_events: &[UmpAt],
        param_changes: &ParameterChanges,
        note_expressions: &[ClapNoteExpression],
        transport: Option<&TransportInfo>,
        transport_changes: &[TransportChange],
    ) -> Result<ProcessOutputRef<'_>> {
        // Cheap early out; `AudioSide::process` checks again under the claim.
        if self.audio_side.is_retired() {
            return Err(ClapError::NotActive);
        }

        let num_samples = frames as u32;

        if num_samples > self.max_frames {
            return Err(ClapError::BlockTooLarge {
                requested: num_samples,
                max_frames: self.max_frames,
            });
        }

        self.scratch.input_events.clear();
        if !midi_events.is_empty() {
            self.scratch.input_events.add_midi_events(midi_events);
        }
        // The Controller's parameter edits go in first, at time 0, so where an
        // edit and the block's automation set one parameter at one time the
        // automation has the last word. The range table is lent once for the
        // block (never parked), and the edits are denormalized against it.
        {
            let table = self.ranges.lend();
            let input_events = &mut self.scratch.input_events;
            let drain = &mut self.drain;
            // What a flush left for a block goes first.
            // What a load made stale since a flush held it is dropped: the load
            // moved the Controller's slots' epoch past the one the value kept.
            let epoch = drain.epoch();
            for &(id, value, held) in self.carried.as_slice() {
                if !epoch_before(held, epoch) {
                    input_events.add_param_edit(id, value.get(), &table.ranges, table.claims);
                }
            }
            self.carried.clear();
            input_events.add_param_edits(&table.ranges, table.claims, |each| {
                drain.drain(each);
            });
            if !param_changes.is_empty() {
                input_events.add_param_changes(param_changes, &table.ranges, table.claims);
            }
        }
        if !note_expressions.is_empty() {
            self.scratch
                .input_events
                .add_note_expressions(note_expressions);
        }
        // The transport's changes inside the block, each at its frame, added
        // after the rest so that on one frame they follow it, and sorted in
        // with it below. Only beside a transport: a host that sends none has
        // no timeline to change.
        if transport.is_some() {
            for change in transport_changes
                .iter()
                .take(tutti_graph::MAX_TRANSPORT_CHANGES)
            {
                self.scratch
                    .input_events
                    .add_transport(change.offset, build_clap_transport(&change.transport));
            }
        }
        // A host-supplied offset past the block is clamped to its last sample,
        // and the list is put in time order: CLAP requires both.
        self.scratch.input_events.clamp_times(num_samples);
        self.scratch.input_events.sort_by_time();

        self.scratch.output_events.clear();

        self.do_process(audio, num_samples, transport)
    }

    /// Delivers `changes` between blocks with `clap.params.flush`, the
    /// Controller's writes first, as at the head of a block. A parameter that
    /// requires `process` takes no value from a flush of an active plugin: its
    /// Controller edit and its point in `changes` are kept, and the next block
    /// delivers each parameter's last value at the head of the block, whatever
    /// offset the point had (a value a flush held back lands at frame 0). `false`
    /// when the plugin has no flush entry or is busy.
    pub fn flush_parameters(&mut self, changes: &tutti_plugin_types::ParameterChanges) -> bool {
        self.scratch.input_events.clear();
        self.scratch.output_events.clear();
        // Held values the room could not keep.
        let mut refused = 0u64;
        {
            let table = self.ranges.lend();
            let input_events = &mut self.scratch.input_events;
            let drain = &mut self.drain;
            let carried = &mut self.carried;
            let epoch = drain.epoch();
            input_events.add_param_edits(&table.ranges, table.claims, |each| {
                drain.drain_in_epoch(|id, value, observed| {
                    if ParamBounds::find(&table.ranges, id).is_some_and(|b| b.requires_process) {
                        refused += u64::from(!carry(carried, id, value, observed));
                    } else {
                        each(id, value);
                    }
                });
            });
            if !changes.is_empty() {
                input_events.add_flushable_changes(
                    changes,
                    &table.ranges,
                    table.claims,
                    |id, value| refused += u64::from(!carry(carried, id, value, epoch)),
                );
            }
        }
        self.scratch.input_events.count_refused(refused);
        self.scratch.input_events.sort_by_time();
        let AudioScratch {
            input_events,
            output_events,
            ..
        } = &mut *self.scratch;
        let flushed = self.audio_side.params_flush(&*input_events, output_events);
        if flushed {
            // What the plugin said while it took them (its own edits and
            // gestures) goes where a block's goes: to the Controller's reports.
            self.report_edits();
        }
        flushed
    }

    /// Hands the block to the plugin through the FFI crate, which binds the
    /// channel tables, takes the audio claim, starts processing if need be,
    /// serves a `clap.thread-pool` request for exactly the span of the call and
    /// zeroes the outputs on `CLAP_PROCESS_ERROR`.
    fn do_process(
        &mut self,
        audio: Planar<'_, '_>,
        num_samples: u32,
        transport: Option<&TransportInfo>,
    ) -> Result<ProcessOutputRef<'_>> {
        let clap_transport = transport.map(build_clap_transport);
        let steady_time = self.scratch.steady_time;

        let pool = self.pool.lend();
        let handle = pool.0.as_ref().map(|snapshot| &snapshot.handle);
        let AudioScratch {
            process,
            input_events,
            output_events,
            ..
        } = &mut *self.scratch;
        let outcome = self.audio_side.process(ProcessCall {
            scratch: process,
            audio,
            input_ports: &self.input_ports,
            output_ports: &self.output_ports,
            frames: num_samples,
            steady_time,
            transport: clap_transport.as_ref(),
            in_events: &*input_events,
            out_events: output_events,
            pool: handle,
        });
        drop(pool);

        let status = match outcome {
            Ok(status) => status,
            Err(ProcessError::Retired) => return Err(ClapError::NotActive),
            Err(ProcessError::Busy) => return Err(ClapError::Busy),
            Err(ProcessError::Poisoned) => return Err(ClapError::Poisoned),
            Err(ProcessError::StartFailed) => return Err(ClapError::StartProcessingFailed),
            Err(ProcessError::FormatMismatch) => return Err(ClapError::FormatMismatch),
            Err(ProcessError::BufferTooShort) => return Err(ClapError::BufferTooShort),
            Err(ProcessError::LayoutMismatch | ProcessError::PortsMismatch) => {
                return Err(ClapError::LayoutMismatch);
            }
            Err(ProcessError::BlockTooSmall) => return Err(ClapError::BlockTooSmall),
            Err(ProcessError::BlockTooLarge) => {
                return Err(ClapError::BlockTooLarge {
                    requested: num_samples,
                    max_frames: self.max_frames,
                });
            }
            Err(ProcessError::PluginReturnedError) => {
                self.scratch.steady_time =
                    self.scratch.steady_time.saturating_add(num_samples as i64);
                self.scratch
                    .last_process_status
                    .store(clap_sys::process::CLAP_PROCESS_ERROR, Ordering::Relaxed);
                return Err(ClapError::PluginReturnedError);
            }
        };

        self.scratch.steady_time = self.scratch.steady_time.saturating_add(num_samples as i64);
        self.scratch
            .last_process_status
            .store(status.raw(), Ordering::Relaxed);

        let AudioScratch {
            input_events,
            output_events,
            out_midi,
            out_translator,
            out_midi_lost,
            out_param_changes,
            out_param_points_dropped,
            out_note_expressions,
            ..
        } = &mut *self.scratch;
        // A note the plugin says has ended no longer answers per-note
        // controllers (its id is free for the plugin to reuse).
        for event in output_events.events() {
            if let ClapEvent::NoteEnd(end) = event {
                input_events.note_ended(end.note_id);
            }
        }
        *out_midi_lost = output_events.fill_midi_events(out_translator, out_midi);
        *out_param_points_dropped += output_events.fill_param_changes(out_param_changes);
        output_events.fill_note_expressions(out_note_expressions);

        Ok(ProcessOutputRef {
            midi_events: &self.scratch.out_midi,
            midi_lost: self.scratch.out_midi_lost,
            param_changes: &self.scratch.out_param_changes,
            note_expressions: &self.scratch.out_note_expressions,
        })
    }
}

pub(super) fn build_clap_transport(transport: &TransportInfo) -> clap_event_transport {
    // `bar_start` / `bar_number` get no flag of their own because CLAP defines
    // none: the spec's transport flags are exactly the eight in `clap_sys`
    // (tempo, beats/seconds timeline, time signature, playing, recording, loop
    // active, pre-roll). `bar_start` is a `clap_beattime`, the same type as
    // `song_pos_beats`, so it rides the beats timeline that is already
    // advertised.
    // `HAS_TIME_SIGNATURE` and `HAS_TEMPO` are unconditional: `TimeSignature`
    // and `Bpm` are validated newtypes with no representable invalid value (a
    // tempo is finite and above zero, which plugins divide by), so the claims
    // are always true. The timelines assert that a position is finite, which a
    // `Beat` and a `Seconds` do not guarantee, and asserting it for a field we
    // did not fill is how a host ends up sending a NaN flagged as valid.
    let mut flags: u32 = CLAP_TRANSPORT_HAS_TIME_SIGNATURE | CLAP_TRANSPORT_HAS_TEMPO;

    if transport.position.beat.get().is_finite() {
        flags |= CLAP_TRANSPORT_HAS_BEATS_TIMELINE;
    }
    // The seconds timeline covers the loop's seconds too: with a loop active
    // and no exact seconds for it, the flag is not claimed.
    let loop_seconds = transport
        .loop_region
        .and_then(|l| l.seconds)
        .filter(|(a, b)| a.get().is_finite() && b.get().is_finite());
    if transport.position.seconds.get().is_finite()
        && (transport.loop_region.is_none() || loop_seconds.is_some())
    {
        flags |= CLAP_TRANSPORT_HAS_SECONDS_TIMELINE;
    }

    if transport.state.playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if transport.state.recording {
        flags |= CLAP_TRANSPORT_IS_RECORDING;
    }
    if transport.loop_region.is_some() {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }

    clap_event_transport {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags,
        song_pos_beats: (transport.position.beat.get() * CLAP_BEATTIME_FACTOR as f64) as i64,
        song_pos_seconds: (transport.position.seconds.get() * CLAP_SECTIME_FACTOR as f64) as i64,
        tempo: transport.timing.tempo.get(),
        // BPM per sample until the next transport event, as the host measured
        // it (`0.0` when it cannot describe the block by one slope). Sent
        // only if finite: a bare `f64` has no range check.
        tempo_inc: if transport.timing.tempo_inc.is_finite() {
            transport.timing.tempo_inc
        } else {
            0.0
        },
        loop_start_beats: transport.loop_region.map_or(0, |l| {
            (l.range.start().get() * CLAP_BEATTIME_FACTOR as f64) as i64
        }),
        loop_end_beats: transport.loop_region.map_or(0, |l| {
            (l.range.end().get() * CLAP_BEATTIME_FACTOR as f64) as i64
        }),
        loop_start_seconds: loop_seconds
            .map_or(0, |(a, _)| (a.get() * CLAP_SECTIME_FACTOR as f64) as i64),
        loop_end_seconds: loop_seconds
            .map_or(0, |(_, b)| (b.get() * CLAP_SECTIME_FACTOR as f64) as i64),
        bar_start: (transport.bar.start.get() * CLAP_BEATTIME_FACTOR as f64) as i64,
        // CLAP's bar at song position 0 is number 0 (`events.h`); `BarNumber`
        // is 1-based. Saturating: an `i64` far outside `i32` is no bar a plugin
        // could use.
        bar_number: i32::try_from(transport.bar.number.index()).unwrap_or(
            if transport.bar.number.index() < 0 {
                i32::MIN
            } else {
                i32::MAX
            },
        ),
        // `.into()` rather than `as u16`: a cast would wrap a negative
        // numerator to 65535. `BeatsPerBar`/`NoteValue` are validated on
        // construction, so the conversion is total and lossless.
        tsig_num: transport.timing.signature.beats_per_bar().into(),
        tsig_denom: transport.timing.signature.note_value().into(),
    }
}

/// Keeps an edit a flush could not deliver for the next block: the newest of
/// a parameter, with the epoch of the Controller's slots it was held in (a load
/// moves it past, and the block drops what it made stale). `false` when the room
/// is full, which it cannot be: the room is one entry per declared parameter,
/// ids are distinct, and the list of ids is fixed for an activation (a rescan
/// that adds or removes one reactivates the plugin and builds a new Processor).
/// The scan is over the held parameters, which are those that require
/// `process`: few.
///
/// `epoch` is the one the value was last checked against. An edit from the
/// Controller's slots takes the epoch its drain observed, so a write made right
/// after a load keeps the load's epoch and is delivered. A point from a block's
/// changes was checked by the node against the host's slots and takes the
/// Controller's epoch read here, later: a load that lands between the two gives
/// a stale point a current stamp.
fn carry(
    carried: &mut CappedVec<(ParamId, Normalized, u32)>,
    id: ParamId,
    value: Normalized,
    epoch: u32,
) -> bool {
    match carried
        .as_mut_slice()
        .iter_mut()
        .find(|(known, ..)| *known == id)
    {
        Some(entry) => {
            (entry.1, entry.2) = (value, epoch);
            true
        }
        None => carried.push((id, value, epoch)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_music::LoopRange;
    use tutti_plugin_types::LoopRegion;
    use tutti_units::{Beat, Bpm, SampleRate};

    fn base() -> TransportInfo {
        TransportInfo::new(SampleRate::SR_48K)
    }

    fn cycle(start: f64, end: f64) -> LoopRegion {
        LoopRegion::new(LoopRange::new(Beat::new(start), Beat::new(end)).expect("a loop"))
    }

    /// A timeline flag is a claim that the matching position is finite, so it
    /// must not be set for a NaN. The tempo and the signature are valid by
    /// their types, so their flags stand for every snapshot.
    #[test]
    fn a_timeline_flag_is_not_set_for_a_non_finite_position() {
        let mut t = base();
        assert_ne!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TEMPO,
            0,
            "a tempo is valid by its type and always advertised"
        );

        t.position.beat = Beat::new(f64::NAN);
        assert_eq!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
            0
        );
        t.position.beat = Beat::new(0.0);
        assert_ne!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
            0
        );
        t.position.seconds = tutti_units::Seconds::new(f64::NAN);
        assert_eq!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
            0
        );
    }

    /// The slope rides beside the tempo when it is finite; a NaN one is `0.0`.
    #[test]
    fn the_slope_is_sent_only_when_finite() {
        let mut t = base().tempo(Bpm::of(120.0)).tempo_inc(0.002);
        assert_eq!(build_clap_transport(&t).tempo_inc, 0.002);
        t.timing.tempo_inc = f64::NAN;
        assert_eq!(build_clap_transport(&t).tempo_inc, 0.0);
    }

    /// `events.h`: "bar at song pos 0 has the number 0". `BarNumber` is 1-based,
    /// so the first bar is sent as 0 and bar `n` as `n - 1`; a transport
    /// nobody described is at the first bar.
    #[test]
    fn the_bar_at_song_position_zero_is_number_zero() {
        use tutti_units::BarNumber;
        assert_eq!(build_clap_transport(&base()).bar_number, 0);
        let third = base().bar(Beat::new(8.0), BarNumber::new(3));
        assert_eq!(build_clap_transport(&third).bar_number, 2);
        // Pre-roll: the bar before the first is -1.
        let pickup = base().bar(Beat::new(-4.0), BarNumber::new(0));
        assert_eq!(build_clap_transport(&pickup).bar_number, -1);
    }

    /// `events.h`: `loop_start_seconds` / `loop_end_seconds` are the seconds
    /// counterparts of the loop's beats, in `CLAP_SECTIME_FACTOR` fixed point.
    /// With a loop active they carry the loop's bounds; when the producer knew
    /// no seconds for an active loop the seconds timeline is not claimed.
    #[test]
    fn an_active_loop_carries_its_seconds() {
        use tutti_units::Seconds;
        let looped = base()
            .beat(Beat::new(1.0))
            .seconds(Seconds::new(0.5))
            .loop_region(LoopRegion {
                seconds: Some((Seconds::new(1.0), Seconds::new(4.5))),
                ..cycle(2.0, 8.0)
            });
        let ev = build_clap_transport(&looped);
        assert_ne!(ev.flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, 0);
        assert_eq!(ev.loop_start_seconds, CLAP_SECTIME_FACTOR);
        assert_eq!(ev.loop_end_seconds, CLAP_SECTIME_FACTOR * 9 / 2);

        let unknown = base()
            .beat(Beat::new(1.0))
            .seconds(Seconds::new(0.5))
            .loop_region(cycle(2.0, 8.0));
        assert_eq!(
            build_clap_transport(&unknown).flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
            0,
            "no exact loop seconds, no seconds claim"
        );
    }

    /// The time signature's claim is unconditional, and legitimately so:
    /// `TimeSignature` is a validated newtype with no representable invalid
    /// value, so there is no state in which the flag would be a lie.
    #[test]
    fn the_time_signature_flag_is_always_honest() {
        let mut t = base();
        t.position.beat = Beat::new(f64::NAN);
        t.position.seconds = tutti_units::Seconds::new(f64::NAN);
        assert_ne!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
            0
        );
    }
}
