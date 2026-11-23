//! The main-thread half of an activation, and the transitions that split a
//! [`Vst3Loaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use tutti_plugin_types::MainThreadMarker;

use tutti_rt::{DrainMark, DrainWatch};

use tutti_buffers::FloatWidth;
use tutti_plugin_types::{
    Control, Controller, EditWriter, Loaded, ParamAccess, ParamEdit, ParamWriteError, PluginError,
    PluginResult, PluginTail, Prepare, PresetId, Processor, Rebuilt, Refused, ReportDrain, Request,
    RestartRules, Samples, Stuck, Widths, Wraps,
};
use tutti_units::Normalized;
use tutti_units::ParamId;

use crate::types::{ProcessMode, Vst3Sample};

/// Reports of the processor's `outputParameterChanges` held for
/// `take_reported_edits` before the oldest go (a control tick drains them).
const PROCESSOR_EDITS_ROOM: usize = 4096;

/// The reports the Processor can have waiting for a tick.
const REPORT_CAPACITY: usize = 256;

use super::control::plugin_error;
use super::instance::Staged;
use super::loaded::{Activation, Vst3Loaded};
use super::processor::{ProcessorAny, ProcessorParts, Vst3Processor};
use super::shell::Shell;

/// The main-thread half of an activation: the whole control surface of a
/// [`Vst3Loaded`], reached through `Deref`, with parameter writes also queued
/// to the Processor.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it releases its
/// share of the activation; the plugin is destroyed when the Processor's goes
/// too (see the invariant on the shell).
///
/// It does not `Deref` to [`Vst3Loaded`]: the control surface is the
/// [`Control`] trait and the methods here, and the format's own surface is
/// [`loaded`](Self::loaded) and [`loaded_mut`](Self::loaded_mut).
///
/// ```compile_fail
/// // No `Deref`: a `&Vst3Controller` is not a `&Vst3Loaded`.
/// fn loaded(c: &tutti_vst3_host::Vst3Controller) -> &tutti_vst3_host::Vst3Loaded {
///     c
/// }
/// ```
pub struct Vst3Controller {
    /// The plugin of the activation. `None` only once `deactivate` has taken it,
    /// or the drop has handed it on.
    loaded: Option<Vst3Loaded>,
    /// The identity of the pair, and the hand-off to the Processor.
    shell: Arc<Shell>,
    /// The writes to the Processor: one slot per declared parameter.
    edits: EditWriter,
    /// What the Controller handed the Processor, latest per parameter, until a
    /// block has surely taken it: what a save must add, because the
    /// component's stream does not hold it yet.
    handed: Vec<Handed>,
    /// Reads the Processor's drain count.
    watch: DrainWatch,
    /// The plugin's `outputParameterChanges`, the last value of each parameter
    /// per block, applied to the controller on a tick
    /// ([`flush_edits`](Control::flush_edits)).
    reports: ReportDrain,
    /// What `apply_reports` applied, for `take_reported_edits`.
    processor_edits: Vec<ParamEdit>,
    /// The processor's reports that were dropped for want of room in
    /// `processor_edits`.
    processor_edits_dropped: u64,
    _not_sync: PhantomData<Cell<()>>,
}

/// A write on its way to the Processor.
#[derive(Debug, Clone, Copy)]
struct Handed {
    id: ParamId,
    value: Normalized,
    /// The drain count read after the write entered its slot.
    sent_at: DrainMark,
}

impl Handed {
    /// Whether the write has left the ring for the plugin's `process`: two
    /// drains have begun since it entered (`tutti_rt::DrainWatch::two_since`
    /// says why two). That is the ring read, not the plugin's word that it took
    /// the point. A drain that overflows `pending` (more than the reserve's
    /// distinct parameters in one drain, which only a burst from a
    /// parameter-rich plugin can reach) drops the point and counts it in
    /// `edits_dropped`, and the entry goes with the rest: that write is then in
    /// no save. A bus bind that fails after the drain would lose its points
    /// too, but it fails only on a sample-width mismatch, which a typed
    /// Processor cannot have; a `process` that fails was handed the points
    /// first.
    fn is_drained(&self, watch: &DrainWatch) -> bool {
        watch.two_since(self.sent_at)
    }
}

impl std::fmt::Debug for Vst3Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Controller")
            .field("edits", &self.edits)
            .finish_non_exhaustive()
    }
}

impl Vst3Controller {
    fn new(
        loaded: Vst3Loaded,
        shell: Arc<Shell>,
        edits: EditWriter,
        handed: Vec<Handed>,
        watch: DrainWatch,
        reports: ReportDrain,
    ) -> Self {
        Self {
            loaded: Some(loaded),
            shell,
            edits,
            handed,
            watch,
            reports,
            processor_edits: Vec::new(),
            processor_edits_dropped: 0,
            _not_sync: PhantomData,
        }
    }

    fn shell(&self) -> &Arc<Shell> {
        &self.shell
    }

    /// The loaded plugin, for the surface the [`Control`] impl and this type's
    /// own methods do not cover. The Controller does not `Deref` to it.
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    #[must_use]
    pub fn loaded(&self) -> &Vst3Loaded {
        self.loaded
            .as_ref()
            .expect("a live Controller holds its plugin")
    }

    /// [`loaded`](Self::loaded), mutably.
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    pub fn loaded_mut(&mut self) -> &mut Vst3Loaded {
        self.loaded
            .as_mut()
            .expect("a live Controller holds its plugin")
    }

    /// Writes a parameter to the Processor's slot, which puts it in the next
    /// block's `inputParameterChanges`.
    fn queue_edit(&mut self, id: ParamId, value: Normalized) {
        // The write policy refused an id the plugin does not declare before
        // this call, so the slot exists.
        let written = self.edits.write(id, value);
        debug_assert!(written.is_ok(), "{written:?}");
        let sent_at = self.watch.mark();
        self.hand(id, value, sent_at);
    }

    /// Applies the values the Processor reported from the plugin's
    /// `outputParameterChanges` to the controller. They are not queued back to
    /// the Processor: the plugin made them.
    fn apply_reports(&mut self, main: MainThreadMarker) {
        let mut reported = Vec::new();
        self.reports.drain(|edit| {
            if let ParamEdit::Change { id, value, .. } = edit {
                reported.push((id, value));
            }
        });
        for (id, value) in reported {
            // The plugin made the value, so a read-only parameter (a meter) is
            // still the plugin's to report: the host's write policy is for
            // host writes, and the controller takes the value as it was told.
            // Not `set_parameter`, so no `carried` entry: that holds the host's
            // own writes a reactivation must deliver, and this is not one.
            self.loaded_mut().write_normalized(main, id, value);
            // Kept for `take_reported_edits`, up to a bound: nobody may be
            // asking, and then the oldest go and are counted.
            if self.processor_edits.len() == PROCESSOR_EDITS_ROOM {
                self.processor_edits.remove(0);
                self.processor_edits_dropped += 1;
            }
            self.processor_edits.push(ParamEdit::change(id, value));
        }
    }

    /// Records the write as handed to the Processor, replacing an earlier one
    /// to the same parameter.
    fn hand(&mut self, id: ParamId, value: Normalized, sent_at: DrainMark) {
        let handed = Handed { id, value, sent_at };
        match self.handed.iter_mut().find(|h| h.id == id) {
            Some(slot) => *slot = handed,
            None => self.handed.push(handed),
        }
    }

    /// The writes handed to the Processor that no block has surely taken yet,
    /// and so are in no component stream. Forgets the ones that have been.
    fn undrained(&mut self) -> Vec<(ParamId, Normalized)> {
        let watch = &self.watch;
        self.handed.retain(|h| !h.is_drained(watch));
        self.handed.iter().map(|h| (h.id, h.value)).collect()
    }

    /// Makes every write made so far stale (see [`EditWriter::reset`]).
    fn reset_edits(&mut self) {
        self.handed.clear();
        self.edits.reset();
    }
}

impl Drop for Vst3Controller {
    fn drop(&mut self) {
        // Dropped without a rejoin: the plugin is destroyed here, on the
        // control side (`Plugin`'s drop defers the teardown to the owner's
        // main-thread collector when this is not the thread it was made on),
        // unless the Processor still renders, in which case it is
        // parked on the shell for the Processor's drop to retire.
        if let Some(loaded) = self.loaded.take() {
            drop(self.shell.controller_gone(loaded));
        }
    }
}

impl Wraps for Vst3Controller {
    type Plugin = Vst3Loaded;

    fn plugin(&self) -> &Vst3Loaded {
        self.loaded()
    }

    fn plugin_mut(&mut self) -> &mut Vst3Loaded {
        self.loaded_mut()
    }

    /// Applied to the edit controller by the plugin, and queued to the
    /// Processor, which puts it in the next block's `inputParameterChanges`: a
    /// plugin whose component is separate from its controller has no other way
    /// to hear of it.
    fn queue_write(&mut self, _main: MainThreadMarker, id: ParamId, value: Normalized) {
        self.queue_edit(id, value);
    }

    /// Applied to the edit controller alone (`setParamNormalized`): the
    /// Processor has the value through the block's parameter changes, and
    /// queueing it again would put it back over a value a later block moved.
    fn inform_controller(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized) {
        // A write the controller refuses (undeclared, read-only) has nothing to
        // follow, as the Processor's own refusal.
        let _ = self.loaded_mut().set_parameter(main, id, value);
    }

    /// Looks the program up once and refuses, without touching the plugin, an
    /// id that is no program or a program parameter the host may not write;
    /// otherwise runs the restore hooks around the load itself.
    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        let loaded = self.loaded();
        let Some((param, value)) = loaded.program_write(main, id) else {
            return false;
        };
        if loaded.parameter_writable(main, param).is_err() {
            return false;
        }
        self.before_restore(main);
        let selected = self.loaded_mut().select_program(main, param, value);
        if selected {
            self.after_restore(main);
        }
        selected
    }

    /// The writes queued before the load are stale, voided before it runs.
    fn before_restore(&mut self, _main: MainThreadMarker) {
        self.reset_edits();
    }

    /// The writes the state or the program carried (the plugin keeps them in
    /// `carried`) are queued to the Processor, which the component's own stream
    /// did not hold, under the epoch the reset made current.
    fn after_restore(&mut self, _main: MainThreadMarker) {
        for (id, value) in std::mem::take(&mut self.loaded_mut().carried) {
            self.queue_edit(id, value);
        }
    }

    /// Saves the state, with the writes no block has taken yet beside the
    /// component's stream, which does not hold them.
    fn save_with_pending(
        &mut self,
        main: MainThreadMarker,
        _state_use: tutti_plugin_types::StateUse,
    ) -> PluginResult<Vec<u8>> {
        let undrained = self.undrained();
        self.loaded_mut().save_state_with(main, &undrained)
    }

    /// The edits the plugin's own editor made (already in `out`) are queued to
    /// the Processor too: a plugin whose component is separate from its
    /// controller has no other way to hear of them. Then what the plugin's
    /// processor wrote to `outputParameterChanges` since the last call (each a
    /// change with no gesture around it: the processor is not a hand on a knob),
    /// which is not queued back: the plugin made it.
    fn collect_reports(&mut self, main: MainThreadMarker, out: &mut Vec<ParamEdit>, from: usize) {
        let reported: Vec<_> = out[from..]
            .iter()
            .filter_map(|edit| match *edit {
                ParamEdit::Change { id, value, .. } => Some((id, value)),
                _ => None,
            })
            .collect();
        for (id, value) in reported {
            self.queue_edit(id, value);
        }
        self.apply_reports(main);
        out.append(&mut self.processor_edits);
    }

    fn lost_reports(&self) -> u64 {
        self.processor_edits_dropped
    }

    /// Applies what the plugin's `outputParameterChanges` reported to the edit
    /// controller (`setParamNormalized`, on this thread, as the portal's
    /// "Processor and Controller" section has it).
    fn tick(&mut self, main: MainThreadMarker) {
        self.apply_reports(main);
        // A tick is a control-side moment: plugins whose last holder was a
        // Processor are destroyed here, not on its thread.
        super::shell::reap();
    }
}

impl Controller for Vst3Controller {
    type Loaded = Vst3Loaded;
    type Processor = ProcessorAny;

    fn latency(&self, main: MainThreadMarker) -> Samples {
        Samples::new(self.loaded().latency_samples(main) as usize)
    }

    fn tail(&self, main: MainThreadMarker) -> PluginTail {
        PluginTail::from_vst3_samples(self.loaded().tail_samples(main))
    }
}

impl Vst3Loaded {
    /// Activates the plugin at a sample format fixed by `T`, splitting it into
    /// the realtime [`Vst3Processor<T>`] and the main-thread
    /// [`Vst3Controller`]. This is [`Loaded::activate`] for a user who fixes the
    /// sample type at compile time; the trait's version picks `T` from
    /// [`Prepare::format`] and wraps the processor in [`ProcessorAny`].
    ///
    /// Runs `setBusArrangements`, `setupProcessing` (the render mode is
    /// [`Prepare::mode`]: VST3 delivers it here and nowhere else, so a change
    /// of mode is a new activation), `activateBus` and `setActive(1)`, and sizes
    /// the scratch from what the plugin settled on.
    ///
    /// # Errors
    ///
    /// [`Refused`] carries the **unconsumed** `Vst3Loaded` beside the reason, so
    /// a refusal costs the configuration and not the instance: a plugin that
    /// declines 64-bit audio is retried at `f32` against the same instance.
    // The `Err` variant deliberately hands `self` (a large `Vst3Loaded`) back so
    // the caller can retry or fall back; boxing it would defeat that ownership
    // return and add a heap alloc on the (rare) failure path.
    #[allow(clippy::result_large_err)]
    pub fn activate_typed<T: Vst3Sample>(
        mut self,
        main: MainThreadMarker,
        prepare: Prepare,
    ) -> std::result::Result<(Vst3Processor<T>, Vst3Controller), Refused<Self>> {
        // `maxSamplesPerBlock` is an `i32`, narrower than a `MaxBlock`'s `u32`.
        if i32::try_from(prepare.max_block().get_u32()).is_err() {
            return Err(Refused {
                plugin: Some(self),
                reason: PluginError::Other(format!(
                    "a block of {} frames exceeds VST3's maxSamplesPerBlock (i32)",
                    prepare.max_block().get()
                )),
            });
        }
        super::shell::warm_retirement();
        super::shell::reap();
        let mode = if prepare.mode().is_offline() {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        // What an earlier rejoin carried over reaches the plugin in the first
        // block, unless this activation is refused, which keeps it.
        let carried = std::mem::take(&mut self.carried);
        let staged =
            match Staged::<T>::stage(main, self, prepare.rate(), prepare.max_block().get(), mode) {
                Ok(staged) => staged,
                Err((mut plugin, error)) => {
                    plugin.carried = carried;
                    return Err(Refused {
                        plugin: Some(plugin),
                        reason: plugin_error(error),
                    });
                }
            };
        let Staged {
            mut loaded, audio, ..
        } = staged;
        // The plugin may announce its latency from inside `setActive`; the
        // figure is read right after every activation, so the announcement is
        // answered. Everything else raised meanwhile is kept.
        for request in tutti_plugin_types::taken(loaded.plugin.handler().requests()) {
            if request != Request::LatencyChanged {
                let _ = request.raise(loaded.plugin.handler().requests());
            }
        }
        let Some(audio_side) = loaded.plugin.audio_side() else {
            // The plugin is active and no Processor will run it: it is put back as
            // it was, and what an earlier rejoin carried stays with it.
            loaded.deactivate_plugin(main);
            loaded.carried = carried;
            return Err(Refused {
                plugin: Some(loaded),
                reason: PluginError::Other(
                    "the plugin is not set up and active, or its audio half is out".into(),
                ),
            });
        };
        let context_requirements = loaded.context_requirements();
        // What the plugin declares now, for the write policy on every route while
        // this activation lasts: the Controller answers writes from it and the
        // Processor drops against it.
        let mut declared: Vec<(ParamId, ParamAccess)> = loaded
            .parameter_list(main)
            .iter()
            .map(|info| (info.id(), ParamAccess::of(info)))
            .collect();
        declared.sort_unstable_by_key(|&(id, _)| id);
        let declared: Arc<[(ParamId, ParamAccess)]> = declared.into();
        loaded.declared = Some(Arc::clone(&declared));
        let shell = Arc::new(Shell::new());
        // One slot per declared parameter: the list changes only across a
        // reactivation, so the table cannot go stale while this activation lasts.
        let (sink, drain) = tutti_plugin_types::edit_slots(declared.iter().map(|&(id, _)| id));
        let (reports_in, reports_out) = tutti_plugin_types::reports(REPORT_CAPACITY);
        let (beat, watch) = tutti_rt::drain_clock();
        // The Processor takes the carried writes the plugin declares; the
        // Controller keeps a record of what it holds.
        let handed = carried
            .iter()
            .filter(|&&(id, _)| ParamWriteError::check_sorted(id, &declared).is_ok())
            .map(|&(id, value)| Handed {
                id,
                value,
                sent_at: watch.mark(),
            })
            .collect();
        let processor = Vst3Processor::new(ProcessorParts {
            shell: shell.clone(),
            audio,
            audio_side,
            context_requirements,
            drain,
            reports: reports_in,
            beat,
            carried,
            declared: Arc::clone(&declared),
        });
        Ok((
            processor,
            Vst3Controller::new(loaded, shell, sink, handed, watch, reports_out),
        ))
    }

    /// Rejoins the halves of one activation of any sample type: checks they are
    /// a pair, stops the Processor, drops it, and hands back the plugin
    /// deactivated, with the writes no block took waiting for the next
    /// activation.
    #[allow(clippy::result_large_err)]
    fn rejoin<P>(
        main: MainThreadMarker,
        mut processor: P,
        mut controller: Vst3Controller,
        shell_of: impl Fn(&P) -> &Arc<Shell>,
        stop: impl FnOnce(&mut P),
        take_edits: impl FnOnce(&mut P, &mut Vec<(ParamId, Normalized)>),
    ) -> Result<Vst3Loaded, (P, Vst3Controller, PluginError)> {
        if !Arc::ptr_eq(shell_of(&processor), controller.shell()) {
            return Err((
                processor,
                controller,
                PluginError::Other("the halves are of two activations".into()),
            ));
        }
        // The pair and nothing else: one share each.
        if Arc::strong_count(controller.shell()) != 2 {
            return Err((
                processor,
                controller,
                PluginError::Other("the activation has a third handle".into()),
            ));
        }
        stop(&mut processor);
        // Writes no block has taken would die with the slots: read each slot
        // once, nothing is rendering, so none is contended.
        let mut pending = Vec::new();
        take_edits(&mut processor, &mut pending);
        drop(processor);
        let mut loaded = controller
            .loaded
            .take()
            .expect("a Controller holds its plugin until it is consumed");
        controller.shell.controller_rejoined();
        loaded.deactivate_plugin(main);
        loaded.carried = latest_per_parameter(pending);
        Ok(loaded)
    }

    /// [`Loaded::deactivate`] for a processor of a fixed sample type, as
    /// [`activate_typed`](Self::activate_typed) made it.
    ///
    /// # Errors
    ///
    /// The halves come back, with the reason, when they are of two activations.
    #[allow(clippy::result_large_err)]
    pub fn deactivate_typed<T: Vst3Sample>(
        main: MainThreadMarker,
        processor: Vst3Processor<T>,
        controller: Vst3Controller,
    ) -> Result<Vst3Loaded, (Vst3Processor<T>, Vst3Controller, PluginError)> {
        Self::rejoin(
            main,
            processor,
            controller,
            |p| p.shell(),
            Vst3Processor::stop,
            Vst3Processor::take_edits,
        )
    }

    /// `setActive(0)`, if the plugin is active. The Processor has stopped
    /// processing by now (`rejoin` stops it, and sends `setProcessing(0)` only
    /// if it was on), so it is not told twice.
    fn deactivate_plugin(&mut self, main: MainThreadMarker) {
        if !self.active.is_active() {
            return;
        }
        // A refusal of `setActive(0)` leaves nothing this host could act on:
        // the interfaces are still valid and every loaded-state method is still
        // legal. (The asymmetry with `setActive(1)`, where a refusal is a
        // failure.)
        let _ = self.plugin.set_active(main, false);
        self.active = Activation::default();
        self.declared = None;
    }
}

/// `pending` with each parameter once, at its latest value, in the order each
/// parameter was first written.
pub(super) fn latest_per_parameter(
    pending: Vec<(ParamId, Normalized)>,
) -> Vec<(ParamId, Normalized)> {
    let mut out: Vec<(ParamId, Normalized)> = Vec::with_capacity(pending.len());
    for (id, value) in pending {
        match out.iter_mut().find(|(seen, _)| *seen == id) {
            Some(slot) => slot.1 = value,
            None => out.push((id, value)),
        }
    }
    out
}

impl Loaded for Vst3Loaded {
    type Processor = ProcessorAny;
    type Controller = Vst3Controller;

    /// VST3 restarts for latency and for the buses (`kIoChanged`).
    const RESTARTS: RestartRules = RestartRules::VST3;

    /// 64-bit when the plugin answers `canProcessSampleSize(kSample64)`.
    fn widths(&self) -> Widths {
        Widths::new(self.info.supports_f64)
    }

    fn activate(
        self,
        main: MainThreadMarker,
        prepare: Prepare,
    ) -> Result<(ProcessorAny, Vst3Controller), Refused<Self>> {
        match prepare.format() {
            FloatWidth::F64 => self
                .activate_typed::<f64>(main, prepare)
                .map(|(p, c)| (ProcessorAny::F64(p), c)),
            FloatWidth::F32 => self
                .activate_typed::<f32>(main, prepare)
                .map(|(p, c)| (ProcessorAny::F32(p), c)),
        }
    }

    fn deactivate(
        main: MainThreadMarker,
        processor: ProcessorAny,
        controller: Vst3Controller,
    ) -> Result<Self, Stuck<ProcessorAny, Vst3Controller>> {
        Self::rejoin(
            main,
            processor,
            controller,
            |p| p.shell(),
            Processor::stop,
            ProcessorAny::take_edits,
        )
        .map_err(|(processor, controller, reason)| Stuck {
            processor,
            controller,
            reason,
        })
    }

    /// Loads the plugin again from where it came, state and all, and drops the
    /// old instance (`kReloadComponent`). The new instance is built first, so a
    /// refusal leaves the old one in place. An old instance that will not save
    /// its state, or a new one that refuses it, leaves the new instance at its
    /// defaults and is [`Rebuilt::restored`]'s error.
    fn rebuild(mut self, main: MainThreadMarker) -> Result<Rebuilt<Self>, Refused<Self>> {
        let state = self.save_state(main, tutti_plugin_types::StateUse::Project);
        let options = match self.origin.id() {
            Some(tutti_plugin_types::PluginId::Vst3(class)) => {
                super::loaded::LoadOptions::new().class_id(*class)
            }
            _ => super::loaded::LoadOptions::new(),
        };
        let mut fresh = match Vst3Loaded::load(main, &self.work, self.origin.path(), options) {
            Ok(fresh) => fresh,
            Err(error) => {
                return Err(Refused {
                    plugin: Some(self),
                    reason: plugin_error(error),
                });
            }
        };
        let restored = state.and_then(|state| {
            fresh.load_state(main, &state, tutti_plugin_types::StateUse::Project)
        });
        fresh.carried = std::mem::take(&mut self.carried);
        Ok(Rebuilt {
            plugin: fresh,
            restored,
        })
    }
}
