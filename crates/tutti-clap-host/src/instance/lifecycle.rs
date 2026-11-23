//! Activation: `ClapLoaded`'s metadata accessors, `activate_now` and the
//! FFI pair `activate_plugin` / `deactivate_plugin` the transitions are built
//! from, and the `Drop` teardown.
//!
//! A transition consumes `self`, which is what makes a handle to a deactivated
//! plugin unrepresentable. This file owns the part the type system cannot
//! express: the FFI ordering. A plugin that refuses `activate` comes back whole
//! in [`Refused`].

use std::sync::atomic::AtomicU64;

use super::ClapLoaded;
use super::config::{AudioScratch, INPUT_NOTE_EXPRESSIONS, RangeTable};
use super::controller::ClapController;
use super::processor::{ClapProcessor, ProcessorParts};
use super::thread_pool::PoolCell;
use crate::error::{ClapError, LoadStage, Result};
use crate::events::{InputRooms, OutputRooms};
use crate::types::{ParameterChanges, ParameterQueue, PluginInfo};
use std::path::PathBuf;
use std::sync::Arc;
use tutti_buffers::FloatWidth;
use tutti_buffers::MaxBlock;
use tutti_clap_ffi::ActivateError;
#[cfg(test)]
use tutti_plugin_types::fake::{at_frame, unit_value};
use tutti_plugin_types::{
    Control, Loaded, MIDI_STACK_BOUND, PluginError, Prepare, Refused, Request, taken,
};
use tutti_plugin_types::{MainThreadMarker, SampleRate};
use tutti_rt::Publisher;
use tutti_units::ParamId;

/// The events the plugin's output list holds besides parameter values, in one
/// block: notes, MIDI, note expressions, SysEx and gestures. A push past it is
/// refused and counted ([`ClapProcessor::output_events_refused`]).
const OUTPUT_EVENT_CAPACITY: usize = 256;
/// Floor of the queues reserved for the parameter changes the plugin's
/// output fills (one per distinct param_id it emits in a block). The
/// reservation is the plugin's parameter count when that is larger.
const PARAM_QUEUE_CAPACITY: usize = 16;
/// How many of the plugin's own parameter edits (its output events) the ring
/// to the Controller holds between two drains: a preset that touches every
/// parameter, with a gesture's two ends around each, in one burst.
pub(crate) const REPORT_CAPACITY: usize = 2048;

/// A `ClapError` as the shared vocabulary's error.
pub(crate) fn plugin_error(error: ClapError) -> PluginError {
    match error {
        ClapError::LoadFailed { stage, reason, .. } => PluginError::Load { stage, reason },
        ClapError::StateError(reason) => PluginError::State(reason),
        ClapError::EditorError(reason) => {
            PluginError::Editor(tutti_plugin_types::EditorError::PluginError(reason))
        }
        other => PluginError::Other(other.to_string()),
    }
}

impl ClapLoaded {
    /// Whether every one of the plugin's audio ports, inputs and outputs,
    /// advertises 64-bit support (and it has at least one). Activating as
    /// `f64` requires this, because the host hands one sample width for every
    /// buffer.
    #[must_use]
    pub fn supports_f64(&self) -> bool {
        self.audio.supports_f64
    }

    /// Metadata read from the plugin's descriptor at load time.
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// The format an activation runs at when `requested` is asked of this
    /// plugin: 64-bit only when asked for and every port advertises it, else
    /// 32-bit ([`Widths::negotiate`](tutti_plugin_types::Widths::negotiate)). The audio ports are read
    /// again first, so the answer is the plugin's capability now, not as of
    /// load. A caller that wants the fallback negotiates here and puts the
    /// result in [`Prepare::format`]; [`Loaded::activate`] reads the ports once
    /// more and refuses a 64-bit request the plugin can no longer take rather
    /// than quietly changing it, and hands the plugin back for a retry.
    ///
    /// # Errors
    ///
    /// [`ClapError::NotSupported`] while the plugin is active (the ports are
    /// `[!active]`).
    ///
    /// [`Loaded::activate`]: tutti_plugin_types::Loaded::activate
    /// [`ClapError::NotSupported`]: crate::ClapError::NotSupported
    pub fn negotiate_format(
        &mut self,
        main: MainThreadMarker,
        requested: FloatWidth,
    ) -> crate::Result<FloatWidth> {
        self.rescan_audio_ports(main)?;
        Ok(self.widths().negotiate(requested))
    }

    /// Activates the plugin at the sample format `prepare` names, splitting it
    /// into the realtime [`ClapProcessor`] and the main-thread
    /// [`ClapController`]. The format is a value of the processor, not a type:
    /// it chooses the scratch the processor is built with, and each block is
    /// matched against it once.
    ///
    /// The plugin's audio ports are read again first (they can change while it
    /// is deactivated, and the 64-bit flag is one of them), the render mode is
    /// applied, and the scratch is sized from what the plugin now declares.
    ///
    /// # Errors
    ///
    /// [`Refused`] carries the **unconsumed** `ClapLoaded` beside the reason, so
    /// a refusal costs the configuration and not the instance: a plugin that
    /// declines 64-bit audio is retried at `f32` (see
    /// [`negotiate_format`](Self::negotiate_format)) against the same mapped
    /// library and the same instance. Both failure paths preserve it: the
    /// `f64` check returns before any FFI runs, and a plugin-side refusal
    /// leaves the instance untouched and not active.
    // The `Err` variant deliberately hands `self` (a large `ClapLoaded`) back so
    // the caller can retry or fall back; boxing it would defeat that ownership
    // return and add a heap alloc on the (rare) failure path.
    #[allow(clippy::result_large_err)]
    pub(crate) fn activate_now(
        mut self,
        main: MainThreadMarker,
        prepare: Prepare,
    ) -> std::result::Result<(ClapProcessor, ClapController), Refused<Self>> {
        let wide = prepare.format() == FloatWidth::F64;
        let refuse = |plugin: Self, error: ClapError| Refused {
            plugin: Some(plugin),
            reason: plugin_error(error),
        };

        let max_block = prepare.max_block();
        // CLAP's render mode is `[main-thread]` and carries no activation
        // constraint; a plugin without the extension has declined, which is
        // not a failure.
        let _ = Control::set_render_mode(&mut self, main, prepare.mode());

        // The plugin may have changed its ports since it was loaded or last
        // deactivated, and the 64-bit flag is part of them.
        if let Err(e) = self.rescan_audio_ports(main) {
            return Err(refuse(self, e));
        }

        if wide && !self.audio.supports_f64 {
            let err = ClapError::NotSupported(format!(
                "Plugin '{}' does not support 64-bit audio processing",
                self.info.name
            ));
            return Err(refuse(self, err));
        }

        if let Err(e) = self.activate_plugin(main, prepare.rate(), max_block) {
            return Err(refuse(self, e));
        }
        self.audio.f64_active = wide;

        let table = self.params.ranges().clone();
        let max_frames = max_block.get();
        let mut scratch = AudioScratch::new(
            prepare.format(),
            input_rooms(&table, max_frames),
            output_rooms(&table, max_frames),
        );
        let port_channels = |ports: &tutti_plugin_types::BusChannels| -> Vec<u32> {
            ports.iter().map(|c| u32::from(c.count())).collect()
        };
        scratch.process.resize_for(
            &port_channels(&self.ports.inputs),
            &port_channels(&self.ports.outputs),
            max_frames,
        );

        // Pre-allocate the growable event scratch so steady-state `process`
        // calls never touch the allocator.
        //
        // `out_midi` / `out_note_expressions` are absent here on purpose: they
        // are `RtVec`, whose capacity is inline and fixed at the type level.
        //
        // The host delivers parameter changes only (the plugin node's
        // `ParamDelivery`), so a block can carry one change for every
        // parameter at once: the input list has room for that on top of the
        // MIDI scratch, and the output queue list has one queue per parameter.
        //
        // Each parameter can carry `ParameterQueue::chunk_room` points in a
        // block of `max_frames` (what the plugin node reserves per queue),
        // and one more when the bridge prepends a value carried from a block
        // it skipped; the node's chunk holds what its point budget
        // (`ParameterChanges::point_budget`) allows of them, so a few for
        // every parameter and the full room for `POOLED_QUEUES` of them. The
        // list is reserved for that budget, and bounded to it, so a block
        // carrying more is counted rather than grown into on the audio
        // thread (`InputEventList::param_points_dropped`). The output side is
        // reserved the same way, one queue per parameter drawing on a shared
        // pool of a chunk's room, and bounded the same way
        // (`ClapProcessor::output_param_points_dropped`).
        reserve_output_queues(&mut scratch, &table, max_frames);
        // A UMP Group is an input note port: each port's own dialect.
        let ports = self
            .midi_note_ports(main)
            .and_then(|ports| Ok((ports, self.midi_capable_ports(main)?)));
        let (ports, capable) = match ports {
            Ok(both) => both,
            Err(e) => return Err(refuse(self, ClapError::NotSupported(e.to_string()))),
        };
        if let Err(e) = scratch.input_events.set_note_ports(&ports) {
            return Err(refuse(self, ClapError::NotSupported(e.to_string())));
        }
        // `clap_event_midi` only to a port that declares the MIDI dialect.
        scratch.input_events.set_midi_capable_ports(&capable);
        scratch
            .in_note_expressions
            .reserve_exact(INPUT_NOTE_EXPRESSIONS);

        let declared: Vec<_> = table.ranges.iter().map(|b| b.id).collect();
        let ranges = Publisher::new(table);
        let pools = Publisher::new(PoolCell(self.pool_snapshot()));
        let channels = |ports: &tutti_plugin_types::BusChannels| -> Vec<u32> {
            ports.iter().map(|c| u32::from(c.count())).collect()
        };
        let input_ports = channels(&self.ports.inputs);
        let output_ports = channels(&self.ports.outputs);
        let audio_side = self
            .plugin
            .audio_side()
            .expect("an activated plugin has an audio half");
        // One slot per declared parameter: the list changes only across a
        // reactivation, so the table cannot go stale while this activation lasts.
        let (sink, drain) = tutti_plugin_types::edit_slots(declared);
        let (reports, reported) = tutti_plugin_types::reports(REPORT_CAPACITY);
        let reports_dropped = Arc::new(AtomicU64::new(0));
        let processor = ClapProcessor::new(ProcessorParts {
            audio_side,
            pool: pools.reader(),
            input_ports,
            output_ports,
            max_frames: max_block.get_u32(),
            scratch: Box::new(scratch),
            ranges: ranges.reader(),
            drain,
            reports,
            reports_dropped: Arc::clone(&reports_dropped),
        });
        let controller =
            ClapController::new(self, sink, ranges, pools, (reported, reports_dropped));
        Ok((processor, controller))
    }

    /// Calls the plugin's `activate` FFI. Shared by `activate_now` and the
    /// transitions built on it.
    pub(crate) fn activate_plugin(
        &mut self,
        main: MainThreadMarker,
        rate: SampleRate,
        max_block: MaxBlock,
    ) -> Result<()> {
        if let Err(e) = self.plugin.activate(main, rate, max_block) {
            return Err(ClapError::LoadFailed {
                path: PathBuf::new(),
                stage: LoadStage::Activation,
                reason: match e {
                    ActivateError::ProcessorStillAlive => {
                        "the audio half of the previous activation is still alive".to_string()
                    }
                    ActivateError::AlreadyActive => "the plugin is already active".to_string(),
                    ActivateError::Refused => "Activate failed".to_string(),
                },
            });
        }
        self.flags.active = true;
        // `clap.latency` and `clap.tail` let a plugin call `changed()` from
        // inside `activate`. The figures are read right after every
        // activation, so the announcement is answered; left raised it would ask
        // for another activation, which would announce again. Everything else
        // raised meanwhile is kept.
        for request in taken(&self.host_state.requests) {
            if !matches!(request, Request::LatencyChanged | Request::TailChanged) {
                request.raise(&self.host_state.requests);
            }
        }
        Ok(())
    }

    /// Whether the plugin is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.flags.active
    }

    /// Calls the plugin's `deactivate` FFI. `deactivate` is
    /// `[main-thread & active]`, so an instance already inactive is left alone.
    pub(crate) fn deactivate_plugin(&mut self, main: MainThreadMarker) {
        if !self.flags.active {
            return;
        }
        self.plugin.deactivate(main);
        self.flags.active = false;
        self.audio.f64_active = false;
    }
}

/// The rooms of the input event list for a block of `max_frames`: the MIDI
/// the node is declared to send ([`InputRooms::for_port`] of
/// [`MIDI_STACK_BOUND`], the node's own intake), the parameter points the
/// host's chunk can arrive with (`ParameterChanges::point_budget` for the
/// plugin's parameters with a room of `ParameterQueue::chunk_room`: the
/// chunk's pooled points, and one more in every queue for the value a skipped
/// block carries, which the bridge prepends), and the edits the Controller
/// can have waiting (one per declared parameter), the note expressions
/// ([`INPUT_NOTE_EXPRESSIONS`]) and the transport changes
/// (`MAX_TRANSPORT_CHANGES`), each source with a room of its own that the
/// MIDI and the parameter points do not count. So a chunk the host's pool
/// bounded reaches the plugin whole; past it (a set nothing bounded) the
/// list's own overflow policy applies (`InputEventList::add_param_changes`).
fn input_rooms(table: &RangeTable, max_frames: usize) -> InputRooms {
    let param_points =
        ParameterChanges::point_budget(table.ranges.len(), ParameterQueue::chunk_room(max_frames));
    InputRooms::for_port(MIDI_STACK_BOUND)
        .param_points(param_points)
        .sources(
            table.ranges.len(),
            INPUT_NOTE_EXPRESSIONS,
            tutti_graph::MAX_TRANSPORT_CHANGES,
        )
}

/// Reserve the queues the plugin's output parameter events are grouped into
/// (`OutputEventList::fill_param_changes`): one per parameter (at least
/// [`PARAM_QUEUE_CAPACITY`]), each with room for the points a block of
/// `max_frames` can carry for one parameter (`ParameterQueue::chunk_room`,
/// and the one more every set keeps) drawn from the set's shared pool
/// (`ParameterChanges::reserved`), so the audio thread opens and clears them
/// without allocating or freeing. Off the audio thread.
///
/// The output event list the plugin pushes into has two rooms, each reserved
/// and bounded: [`OUTPUT_EVENT_CAPACITY`] events for notes, MIDI, note
/// expressions and the rest, and the parameter points the queues can hold
/// (`ParameterChanges::point_budget`). A parameter flood fills only its own room,
/// so it never refuses a note-off; past it each of the plugin's parameters
/// keeps its latest value. A push past either room is refused
/// (`ClapProcessor::output_events_refused`).
fn output_rooms(table: &RangeTable, max_frames: usize) -> OutputRooms {
    let queues = table.ranges.len().max(PARAM_QUEUE_CAPACITY);
    let budget = ParameterChanges::point_budget(queues, ParameterQueue::chunk_room(max_frames));
    let ids: Vec<ParamId> = table.ranges.iter().map(|b| b.id).collect();
    OutputRooms::new(OUTPUT_EVENT_CAPACITY).params(budget, &ids)
}

/// The queues the plugin's parameter output is grouped into: one per
/// parameter, each with room for a block's points of one. Grows only: a
/// smaller block keeps the room.
fn reserve_output_queues(scratch: &mut AudioScratch, table: &RangeTable, max_frames: usize) {
    let queues = table.ranges.len().max(PARAM_QUEUE_CAPACITY);
    let points = ParameterQueue::chunk_room(max_frames);
    let budget = ParameterChanges::point_budget(queues, points);
    let out = &scratch.out_param_changes;
    if out.capacity() < queues || out.point_storage() < budget {
        scratch.out_param_changes = ParameterChanges::reserved(queues, points);
    }
}

impl Drop for ClapLoaded {
    fn drop(&mut self) {
        // Destroy GUI before tearing down the plugin — CLAP spec requires
        // gui.destroy() before plugin.destroy(). A `ClapLoaded` that is dropped
        // is never active (the Controller deactivates the plugin before it lets
        // go of it), so there is no stop/deactivate to do here.
        if let Some(main) = tutti_plugin_types::MainThreadMarker::new() {
            self.destroy_editor(main);
        }

        // The FFI `Plugin`'s drop destroys the instance, then drops the host
        // vtable; the library unloads last.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParamBounds;
    use tutti_midi_protocol::Midi1Translator;

    /// **The input list built for a declared bound reports that bound as what
    /// it holds**: the Processor's `event_input`, which the node declares, is
    /// the list's MIDI room over `Midi1Translator::MAX_MESSAGES`, at any block.
    #[test]
    fn the_input_list_reports_the_bound_it_was_built_from() {
        let table = RangeTable {
            ranges: Vec::new(),
            claims: true,
        };
        for frames in [64usize, 4096] {
            let rooms = input_rooms(&table, frames);
            let list = crate::InputEventList::new(rooms);
            assert_eq!(
                list.event_input(),
                Some(MIDI_STACK_BOUND),
                "{frames} frames"
            );
        }
    }

    /// **Activation reserves the pool budget, not parameters × room**: for
    /// 2 000 and 16 384 parameters at 512- and 4096-frame blocks, the input
    /// event list's parameter room is `ParameterChanges::point_budget` of
    /// the parameters with a room of `chunk_room` (a floor of
    /// `FLOOR_POINTS + 1` each and `POOLED_QUEUES` blocks of `chunk_room +
    /// 1`), the output list's is the same, and the output queues hold
    /// exactly that budget; each under a third of a full room per
    /// parameter.
    #[test]
    fn activation_reserves_the_pool_budget_not_params_times_room() {
        for params in [2_000u32, 16_384] {
            for frames in [512usize, 4096] {
                let table = RangeTable {
                    ranges: (0..params)
                        .map(|id| ParamBounds::continuous(ParamId::new(id), 0.0, 1.0))
                        .collect(),
                    claims: true,
                };
                let mut scratch = AudioScratch::new(
                    FloatWidth::F32,
                    input_rooms(&table, frames),
                    output_rooms(&table, frames),
                );
                reserve_output_queues(&mut scratch, &table, frames);
                let n = params as usize;
                let room = ParameterQueue::chunk_room(frames);
                let input = ParameterChanges::point_budget(n, room);
                let output = input;
                assert_eq!(
                    input,
                    n * (ParameterChanges::FLOOR_POINTS + 1)
                        + ParameterChanges::POOLED_QUEUES * (room + 1)
                );
                assert_eq!(
                    scratch.input_events.param_room, input,
                    "{params} at {frames}"
                );
                assert!(
                    scratch.input_events.events.capacity()
                        >= MIDI_STACK_BOUND.get() as usize * Midi1Translator::MAX_MESSAGES
                            + input
                            + params as usize
                );
                assert!(3 * input < n * (room + 1));
                assert_eq!(scratch.output_events.param_room, output);
                assert_eq!(scratch.out_param_changes.point_storage(), output);
                assert!(3 * output < n * room);
            }
        }
    }

    /// **A chunk the host's pool bounded reaches the plugin whole, carried
    /// values and all.** 2 000 parameters at a 512-frame block, the input
    /// list reserved as activation reserves it. The host's chunk at its
    /// budget (`POOLED_QUEUES` parameters at full room, every other one at
    /// its floor, built `reserved_bridged` as the host builds it), then a
    /// skipped block's carried value prepended to every queue, as the bridge
    /// does (`prepend_carried`, into the slot the set kept, so nothing
    /// grows): every point is handed on, none counted, each parameter's
    /// points in order.
    #[test]
    fn a_chunk_with_carried_values_reaches_the_plugin_whole() {
        use tutti_plugin_types::ParameterPoint;
        const PARAMS: u32 = 2_000;
        const FRAMES: usize = 512;
        let table = RangeTable {
            ranges: (0..PARAMS)
                .map(|id| ParamBounds::continuous(ParamId::new(id), 0.0, 1.0))
                .collect(),
            claims: true,
        };
        let mut scratch = AudioScratch::new(
            FloatWidth::F32,
            input_rooms(&table, FRAMES),
            output_rooms(&table, FRAMES),
        );
        let room = ParameterQueue::chunk_room(FRAMES);
        let mut chunk = ParameterChanges::reserved_bridged(PARAMS as usize, room);
        for id in 0..PARAMS {
            assert!(chunk.open_queue(ParamId::new(id)));
        }
        for k in 0..room {
            for i in 0..PARAMS as usize {
                if k >= ParameterChanges::FLOOR_POINTS && i >= ParameterChanges::POOLED_QUEUES {
                    continue;
                }
                assert!(chunk.make_room(i));
                chunk.queues[i].add_point(at_frame(1 + 3 * k as i32), unit_value(0.5));
            }
        }
        for i in 0..PARAMS as usize {
            assert!(chunk.prepend_carried(
                i,
                ParameterPoint {
                    sample_offset: at_frame(0),
                    value: tutti_units::Normalized::of(0.25),
                },
            ));
        }
        let total: usize = chunk.queues.iter().map(|q| q.points.len()).sum();
        let list = &mut scratch.input_events;
        list.add_param_changes(&chunk, &[], false);
        list.sort_by_time();
        assert_eq!(list.param_points_dropped(), 0);
        assert_eq!(list.events().len(), total);
        let mut per: Vec<Vec<u32>> = vec![Vec::new(); PARAMS as usize];
        for e in list.events() {
            if let crate::ClapEvent::ParamValue(v) = e {
                per[v.param_id as usize].push(v.header.time);
            }
        }
        for (q, got) in chunk.queues.iter().zip(&per) {
            let want: Vec<u32> = q.points.iter().map(|p| p.sample_offset.get()).collect();
            assert_eq!(got, &want);
        }
    }
}
