//! The traits a plugin format implements, and the values its transitions
//! exchange.
//!
//! A plugin is [`Loaded`] (configured, not rendering). [`Loaded::activate`]
//! splits it into a [`Processor`], which only the realtime thread holds and
//! which only renders, and a [`Controller`], which the main thread holds and
//! which carries the control surface. [`Loaded::deactivate`] rejoins the
//! halves. The traits are deliberately not object-safe: a transition takes its
//! values by move, so a stale handle cannot be named.
//!
//! [`Control`] is the part of the surface that exists in every state that has a
//! plugin, so `Loaded` and `Controller` both implement it and a caller reaches
//! parameters and state in either.
//!
//! # Threads
//!
//! Every method of [`Control`] and [`Controller`] runs on the main thread, one
//! at a time. The entries that must (all of [`Control`] but `info`, and the
//! transitions of [`Loaded`]) take a [`MainThreadMarker`]: holding
//! one is the proof, and no check runs. [`Processor::process`] and [`Processor::stop`] run on whichever
//! thread holds the Processor and are the only calls that thread makes. An
//! implementation's `unsafe` states, per format, why the two halves may reach
//! one plugin instance at once.

use std::num::{NonZeroU32, NonZeroUsize};
use tutti_platform::Instant;

use dpi::Size;
use tutti_buffers::{Float, InputOutput};

use crate::{
    AutomationMode, BypassWrite, ChannelTable, ContentScale, EditorCapabilities, EditorWindow,
    KeyEvent, KeyUse, LoadedPlugin, ParamWriteError, ParameterInfo, PluginDescriptor, PluginError,
    PluginResult, PluginTail, Preset, PresetId, ProcessContext, ProcessOutput, ProcessWake,
    SampleRate, Samples, WindowHandle,
};
use crate::{Delivered, ParamEdit, ParameterChanges, bypass_parameter, bypass_value, is_bypassed};
use crate::{
    MainThreadMarker, Prepare, RenderMode, RenderModeSupport, Request, RescanScope, RestartRules,
    StateUse, TimerId, TrackInfo, Wakeups, Widths,
};
use tutti_plugin_ipc_ffi::{FdId, Ready};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// Writes the plugin's bypass parameter, if it lists one: the default of
/// [`Control::set_bypassed`] and what a wrapped plugin's write goes through, so
/// an active plugin's reaches its Processor.
fn write_bypass_parameter<C: Control + ?Sized>(
    plugin: &mut C,
    main: MainThreadMarker,
    bypassed: bool,
) -> Option<BypassWrite> {
    let id = bypass_parameter(&plugin.parameters(main))?;
    Some(BypassWrite::of_write(plugin.set_parameter(
        main,
        id,
        bypass_value(bypassed),
    )))
}

/// What the main thread asks of a plugin, in any state that has one.
///
/// Object-safe: [`Lifecycle::control`](crate::Lifecycle::control) lends it as
/// `&mut dyn Control`.
pub trait Control: EditorWindow {
    /// The plugin's catalog identity.
    fn info(&self) -> &PluginDescriptor;

    /// The wiring read from the plugin now, not a load-time cache.
    fn wiring(&self, main: MainThreadMarker) -> LoadedPlugin;

    /// Every parameter the plugin declares.
    fn parameters(&self, main: MainThreadMarker) -> Vec<ParameterInfo>;

    /// The parameters a control surface lists: [`parameters`](Self::parameters)
    /// without the plugin's bypass parameter (VST3 `kIsBypass`, CLAP
    /// `CLAP_PARAM_IS_BYPASS`), which is the node's mode and is written through
    /// [`set_bypassed`](Self::set_bypassed).
    fn controls(&self, main: MainThreadMarker) -> Vec<ParameterInfo> {
        let mut list = self.parameters(main);
        let bypass = bypass_parameter(&list);
        list.retain(|p| Some(p.id()) != bypass);
        list
    }

    /// Writes the plugin's own bypass (`true` bypassed, `false` processing).
    /// The result says whether it has one and whether the write was handed to
    /// it; the plugin may still refuse, and
    /// [`LoadedPlugin::own_bypass`] and a read back tell.
    ///
    /// The default writes the parameter flagged
    /// [`BYPASS`](crate::ParamFlags::BYPASS) (VST3, CLAP) to its maximum or its
    /// minimum; a format whose bypass is not a parameter (AU, VST2) overrides
    /// it. The host calls it when the node's mode changes, never per block.
    fn set_bypassed(&mut self, main: MainThreadMarker, bypassed: bool) -> BypassWrite {
        write_bypass_parameter(self, main, bypassed).unwrap_or(BypassWrite::NoOwnBypass)
    }

    /// Whether the plugin's own bypass is on now, read from the plugin: `None`
    /// when it has none. The default reads the bypass parameter; AU's property
    /// overrides it.
    fn bypassed(&self, main: MainThreadMarker) -> Option<bool> {
        let id = bypass_parameter(&self.parameters(main))?;
        self.parameter(main, id).map(is_bypassed)
    }

    /// How many parameters the plugin declares: at least `parameters().len()`
    /// (a format whose list skips a parameter it declines to describe still
    /// counts it). The default builds the list; a format with a count query
    /// overrides it, for a caller between blocks that sizes storage by it and
    /// should not build every descriptor to do so.
    fn parameter_count(&self, main: MainThreadMarker) -> usize {
        self.parameters(main).len()
    }

    /// The parameter's current value, or `None` if the plugin has no such
    /// parameter.
    fn parameter(&self, main: MainThreadMarker, id: ParamId) -> Option<Normalized>;

    /// Whether the host may write the parameter `id`: [`ParamWriteError::check`]
    /// on what the plugin declares of it. The default reads
    /// [`parameters`](Self::parameters); a format that finds one parameter
    /// without building the list overrides it, and keeps the policy by
    /// calling [`ParamWriteError::check`].
    ///
    /// # Errors
    ///
    /// As [`ParamWriteError::check`].
    fn parameter_writable(
        &self,
        main: MainThreadMarker,
        id: ParamId,
    ) -> Result<(), ParamWriteError> {
        ParamWriteError::check_info(id, self.parameters(main).iter().find(|p| p.id() == id))
    }

    /// Sets a parameter. Loaded: applied now. Active: a write to the
    /// Processor's slot for the parameter, applied at the head of its next block
    /// ([`edit_slots`](crate::edit_slots())).
    ///
    /// **The value is [`Normalized`], the shared seam's domain.** The four
    /// formats disagree on what a parameter value is: VST3 and VST2 speak
    /// normalized `0..=1`, AU and CLAP the plugin's plain units. Each format
    /// converts at its own edge, so a plain value such as `20_000.0` for a
    /// cutoff is not a valid write here; convert it first with
    /// [`parameter_normalized`](Self::parameter_normalized), and read a
    /// value back in the plugin's units with
    /// [`parameter_plain`](Self::parameter_plain).
    ///
    /// # Errors
    ///
    /// [`ParamWriteError`], with nothing written, for an id the plugin does
    /// not declare and for a parameter it declares read-only: one policy for
    /// every format ([`parameter_writable`](Self::parameter_writable)).
    fn set_parameter(
        &mut self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Result<(), ParamWriteError>;

    /// Tells the plugin's edit controller of a value the host wrote and the
    /// Processor was handed another way (in the block's parameter changes), so
    /// the controller's own value and the plugin's editor follow it. VST3 hosts
    /// inform both halves of a plugin on a host-side change: the component
    /// through the block, the controller with `setParamNormalized` on the main
    /// thread, and that is the only format that has a controller of its own to
    /// tell (CLAP, VST2 and AU answer a read from the plugin the Processor
    /// drives). The default does nothing, so a format with one controller
    /// leaves it so.
    fn inform_controller(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized) {
        let _ = (main, id, value);
    }

    /// The plain value `value` stands for, in the unit
    /// [`ParameterInfo::unit`](crate::ParameterInfo::unit) names: the
    /// **format's** conversion, never a formula over the declared range (VST3
    /// asks the controller's `normalizedParamToPlain`, whose shape is the
    /// plugin's; CLAP and AU convert against their own declared range). `None`
    /// when the parameter does not exist or the format has no plain value for
    /// it (a VST2 parameter with no integer range).
    fn parameter_plain(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64>;

    /// The inverse of [`parameter_plain`](Self::parameter_plain): the
    /// [`Normalized`] value a plain one stands for, clamped onto `0..=1` where
    /// the plain value is outside what the plugin declares. `None` as for
    /// `parameter_plain`.
    fn parameter_normalized(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized>;

    /// The plugin's display text for `value`, if it has any.
    fn parameter_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String> {
        let _ = (main, id, value);
        None
    }

    /// The value the plugin parses from `text`, if it can.
    fn parameter_value_from_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        let _ = (main, id, text);
        None
    }

    /// The plugin's state as bytes, saved for `state_use`. A format with one
    /// state for every use ignores it.
    ///
    /// # Errors
    ///
    /// [`PluginError::State`] when the plugin cannot save.
    fn save_state(&mut self, main: MainThreadMarker, state_use: StateUse) -> PluginResult<Vec<u8>>;

    /// Restores state saved by [`save_state`](Self::save_state) with the same
    /// `state_use`.
    ///
    /// # Errors
    ///
    /// [`PluginError::State`] when the plugin rejects the bytes.
    fn load_state(
        &mut self,
        main: MainThreadMarker,
        bytes: &[u8],
        state_use: StateUse,
    ) -> PluginResult<()>;

    /// The plugin's factory presets. Empty when the format has none.
    fn presets(&mut self, main: MainThreadMarker) -> Vec<Preset> {
        let _ = main;
        Vec::new()
    }

    /// Loads a preset; `false` when the format or the plugin cannot.
    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        let _ = (main, id);
        false
    }

    /// The preset the plugin is on, if it says.
    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId> {
        let _ = main;
        None
    }

    /// Tells the plugin how the host is automating it. `true` when the format's
    /// channel carries the mode (the plugin has it:
    /// [`Features::AUTOMATION_STATE`](crate::Features)); `false` when it has
    /// none, and nothing was sent. Per format:
    ///
    /// - **VST3:** `IAutomationState::setAutomationState` was called on the
    ///   controller.
    /// - **CLAP:** `param-indication.set_automation` was called for at least one
    ///   automatable parameter.
    /// - **VST2:** the host answers `audioMasterGetAutomationState` with the mode
    ///   from now on; the plugin learns it only when it asks.
    /// - **AU:** never `true`: the format has no such call.
    fn set_automation_state(&mut self, main: MainThreadMarker, mode: AutomationMode) -> bool {
        let _ = (main, mode);
        false
    }

    /// Tells the plugin's host what the host says of the track the plugin sits
    /// on. A format that can answer the plugin's reads (CLAP `track-info`)
    /// keeps it and tells the plugin when it differs from the last; a format
    /// without one keeps this default, which takes it and drops it, and
    /// answers [`Yes`](Delivered::Yes): there is nowhere it should have gone.
    ///
    /// [`PluginDead`](Delivered::PluginDead) when the plugin is gone,
    /// [`Dropped`](Delivered::Dropped) when a served plugin's queue was full
    /// and a later call may succeed.
    fn set_track_info(&mut self, main: MainThreadMarker, info: &TrackInfo) -> Delivered {
        let _ = (main, info);
        Delivered::Yes
    }

    /// Reads parameters again after a rescan the plugin asked for.
    fn rescan_parameters(&mut self, main: MainThreadMarker, scope: RescanScope) {
        let _ = (main, scope);
    }

    /// Rebuilds the host's MIDI controller-to-parameter mapping from the plugin
    /// (VST3 `kMidiCCAssignmentChanged`).
    fn rebuild_midi_mapping(&mut self, main: MainThreadMarker) {
        let _ = main;
    }

    /// The control-side tick of an active plugin: applies what its Processor
    /// reported and lets retired published values go.
    /// [`Lifecycle::drive`](crate::Lifecycle::drive) calls it on every
    /// [`Event::Tick`](crate::Event::Tick), so the host must keep ticking: with
    /// no ticks a plugin's reports are never applied to its controller.
    fn flush_edits(&mut self, main: MainThreadMarker) {
        let _ = main;
    }

    /// The plugin's own main-thread work (CLAP `on_main_thread`).
    fn run_main_thread(&mut self, main: MainThreadMarker) {
        let _ = main;
    }

    /// Runs a plugin timer that is due.
    fn run_timer(&mut self, main: MainThreadMarker, id: TimerId) {
        let _ = (main, id);
    }

    /// Services a plugin file descriptor that is ready.
    fn service_fd(&mut self, main: MainThreadMarker, id: FdId) {
        let _ = (main, id);
    }

    /// The shell's one pass over what the plugin asked for since the last call,
    /// appended to `out` in the order [`taken`](crate::taken) yields them, for
    /// [`Lifecycle::drive`](crate::Lifecycle::drive). A format with nothing to
    /// ask says so by pushing nothing.
    fn take_requests(&mut self, main: MainThreadMarker, out: &mut Vec<Request>) {
        let _ = (main, out);
    }

    /// Returns how this plugin takes a change of render mode: told live,
    /// read at its next activation, or not at all.
    fn render_mode_support(&self, main: MainThreadMarker) -> RenderModeSupport;

    /// Tells a [`Live`](RenderModeSupport::Live) plugin its render mode now,
    /// and returns whether it took it. A host calls this only where
    /// [`render_mode_support`](Self::render_mode_support) is `Live`; elsewhere
    /// the mode reaches the plugin as the [`Prepare::mode`](crate::Prepare::mode)
    /// of its next activation, and this answers `false` and changes nothing.
    fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> bool {
        let _ = (main, mode);
        false
    }

    /// Appends the parameter edits the plugin reported of its own accord (its
    /// editor moving a knob, its own automation), oldest first, to `out`: each
    /// a [`ParamEdit`], so a gesture's `Begin`, its changes and its `End` come
    /// in the order the plugin made them.
    ///
    /// Every format reports them here: VST2's `audioMasterBeginEdit`,
    /// `audioMasterAutomate` and `audioMasterEndEdit`, VST3's `beginEdit`,
    /// `performEdit` and `endEdit` through the host's component handler (and
    /// what the processor wrote to `outputParameterChanges`), CLAP's gesture
    /// and value events from `process` or `params.flush`, AU's parameter
    /// listener. A plugin's echo of a value the host just set is not an edit.
    ///
    /// A served plugin (`Ipc`) reports none here: its server forwards them over
    /// the wire, and the bridge's listener hears each as it arrives.
    fn take_reported_edits(&mut self, main: MainThreadMarker, out: &mut Vec<ParamEdit>) {
        let _ = (main, out);
    }

    /// How many of the edits [`take_reported_edits`](Self::take_reported_edits)
    /// delivers the host lost, over the plugin's life: the reports a bounded
    /// queue refused because nobody drained it. Monotonic. Required, because
    /// zero is a claim: a format that reports edits in the block's
    /// [`ProcessOutput`] says `0` in its own impl.
    fn dropped_reported_edits(&self) -> u64;

    /// Gives the plugin's host the [`ProcessWake`] to raise when the plugin
    /// asks to be processed again (CLAP's `request_process`, which a plugin
    /// that returned `CLAP_PROCESS_SLEEP` calls from any thread). Any thread;
    /// the plugin's first binding stands, and a host binds one handle for the
    /// plugin's life. A format with no sleep status keeps the default, which
    /// drops it.
    fn bind_wake(&self, wake: ProcessWake) {
        let _ = wake;
    }
}

/// A plugin that is configured but not rendering.
pub trait Loaded: Control + Sized + Send {
    /// The realtime half.
    type Processor: Processor;
    /// The main-thread half.
    type Controller: Controller<Loaded = Self, Processor = Self::Processor>;

    /// What this format needs a restart for. A reload restarts on every format.
    const RESTARTS: RestartRules;

    /// The float widths the plugin processes audio in, as of now. An
    /// activation at a width outside it is refused, and
    /// [`Widths::negotiate`] turns a request into the width one accepts.
    ///
    /// An out-of-process plugin answers the width its load settled on, not
    /// every width the plugin could process: its slab runs one width until a
    /// reload.
    fn widths(&self) -> Widths;

    /// Splits the plugin. On failure the whole plugin comes back in the error.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the plugin will not run at `prepare`.
    fn activate(
        self,
        main: MainThreadMarker,
        prepare: Prepare,
    ) -> Result<(Self::Processor, Self::Controller), Refused<Self>>;

    /// Rejoins the halves of one activation.
    ///
    /// # Errors
    ///
    /// [`Stuck`], with both halves, for a pair from two activations or a plugin
    /// that will not stop.
    fn deactivate(
        main: MainThreadMarker,
        processor: Self::Processor,
        controller: Self::Controller,
    ) -> Result<Self, Stuck<Self::Processor, Self::Controller>>;

    /// Rebuilds the instance from its load parameters (VST3
    /// `kReloadComponent`).
    ///
    /// # Errors
    ///
    /// [`Refused`], holding the plugin if it survived, when the instance cannot
    /// be rebuilt. A rebuilt plugin that refuses the state it was saved with is
    /// not a refusal: it is [`Rebuilt::restored`] (the field).
    fn rebuild(self, main: MainThreadMarker) -> Result<Rebuilt<Self>, Refused<Self>> {
        let _ = main;
        Ok(Rebuilt::whole(self))
    }
}

/// The realtime half of an active plugin: rendering blocks.
///
/// `Send` to move to the realtime thread, not `Sync`.
pub trait Processor: Send {
    /// Renders one block. Realtime: no allocation, no lock, no blocking. The
    /// error is built only on the failed-block path.
    ///
    /// An implementation drains its edit ring at the head of the block and
    /// merges the edits with `io.ctx`'s automation in scratch it preallocated at
    /// activation, so each Processor owns its own and nothing is built here.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] when the plugin failed the block.
    fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()>;

    /// Ends processing (CLAP `stop_processing`, VST3 `setProcessing(false)`) so
    /// the realtime thread can hand the Processor back. Realtime. Processing
    /// starts again lazily in the first [`process`](Self::process).
    fn stop(&mut self);

    /// Asks the plugin to forget its signal history, as a seek or a transport
    /// reset needs: a delay line, a reverb tail, a filter's memory. Realtime,
    /// between blocks.
    ///
    /// Per format: CLAP `plugin.reset`; VST3 `setProcessing(false)`, started
    /// again in the next block (the SDK calls it a way to reset buffers, a
    /// request a plugin may ignore); VST2 `effStopProcess`, started again in the next
    /// block (likewise a request). Every format drops a SysEx open across the reset.
    fn reset(&mut self);

    /// Delivers `changes` to the plugin between blocks, by the format's own
    /// route for parameters outside a block, so a caller that must see them
    /// applied (a state save, a read of the plugin) has them in before it asks.
    /// Realtime, between blocks, on the thread that renders them. What the
    /// Processor's own slots hold (the Controller's writes) goes in first, then
    /// `changes`, as at the head of a block, so a later write is the last word.
    ///
    /// Per format: CLAP `params.flush` (an audio-thread entry, never beside
    /// `process`); VST3 `process` with `numSamples = 0`, which the SDK
    /// allows for flushing parameters; VST2 and AU a direct set of each
    /// parameter's last value.
    ///
    /// The default refuses: a Processor takes parameters in a block only.
    ///
    /// # Errors
    ///
    /// [`PluginError::Block`] when the Processor has no such route, or the
    /// plugin refused the call.
    fn flush_parameters(&mut self, changes: &ParameterChanges) -> PluginResult<()> {
        let _ = changes;
        Err(PluginError::Block(
            "this Processor takes parameters in a block only",
        ))
    }

    /// The frames this Processor renders in, if it renders in fixed chunks
    /// that may begin in the middle of a graph block and span several
    /// [`process`](Self::process) calls (the served pipeline's: it ships a
    /// whole chunk to another process and plays the one before). `None`, the
    /// default, for a Processor that renders whatever block it is handed.
    ///
    /// With `Some(n)` the graph node cuts each block at the Processor's chunk
    /// edges, counted in frames from `prepare` and from every
    /// [`reset`](Self::reset), and calls `process` once per piece: each call
    /// is inside one chunk. What a call carries:
    ///
    /// - **Per piece, on the piece's frames** (from its start): the audio,
    ///   the MIDI ([`ProcessContext::midi_events`]) and the harmony
    ///   ([`ProcessContext::expressive`]). The Processor accumulates these
    ///   into its chunk itself, at the piece's place in it.
    /// - **Per chunk, so far, on the chunk's frames** (from the chunk's first):
    ///   the parameter changes ([`ProcessContext::param_changes`]) and the
    ///   transport. [`ProcessContext::transport`] is the transport at the
    ///   **chunk's** first frame in every piece, and
    ///   [`transport_changes`](ProcessContext::transport_changes) are the
    ///   chunk's changes up to and including the piece, one walk across its
    ///   pieces: a stop, a seek or a tempo-map point on a block boundary inside
    ///   the chunk is a change at its chunk frame. Both are final in the
    ///   piece that **completes** the chunk (the transport's `tempo_inc` is the
    ///   slope to the chunk's end, known only then), which is the call a
    ///   Processor that ships the chunk there reads them in.
    ///
    /// Read each block, on the realtime thread: it must not change while the
    /// Processor renders.
    fn chunk(&self) -> Option<NonZeroUsize> {
        None
    }

    /// The most events per block this Processor takes from the node's event
    /// input, as it was built: the room of its own input storage (a CLAP
    /// list, a chunk's MIDI, the node's intake for a format that stages into
    /// the plugin's own). The node declares at most this as its
    /// `Shape::event_input_capacity`, so the graph cuts what the Processor
    /// would otherwise refuse, in frame order and keeping note-offs. Read
    /// from the node's `prepare`.
    ///
    /// **A served plugin has a second cut.** A chunk spans several blocks, and
    /// its MIDI is one list of [`MIDI_STACK_CAPACITY`](crate::MIDI_STACK_CAPACITY) events that every block
    /// of the chunk adds to, so a chunk can overflow with every block within
    /// the declared bound. That cut keeps note-offs (it displaces a note-on
    /// for one) and is counted as MIDI lost, once.
    fn event_input(&self) -> NonZeroU32;

    /// The frames of latency this Processor adds to the plugin's own (the
    /// [`Controller::latency`] figure): the served pipeline's chunk, which it
    /// ships to another process and plays the one before. `0`, the default,
    /// for a Processor that renders in place. Read from the node's `prepare`,
    /// after [`retune`](Self::retune), so a graph compiled from the node's
    /// shape compensates for it.
    fn latency(&self) -> Samples {
        Samples::new(0)
    }

    /// Follows the graph's rate, block ceiling and device quantum without a
    /// restart, for a Processor that can (the served pipeline: it tells its
    /// server the rate by a queued command and settles its chunk from the
    /// block and quantum). `true` when it took them, so the graph node does
    /// not deactivate and activate the plugin for them; `false`, the default,
    /// when only a [`Lifecycle`](crate::Lifecycle) reconfiguration can.
    ///
    /// Called from the node's `prepare`, which runs off the audio thread with
    /// the node out of any executor, so it may allocate; it starts the
    /// Processor over (its chunk phase and any chunk in flight), as a
    /// [`reset`](Self::reset) does. Needs no [`MainThreadMarker`],
    /// which is why the served hosting can take it from any thread.
    fn retune(&mut self, rate: SampleRate, max_block: Samples, quantum: Option<Samples>) -> bool {
        let _ = (rate, max_block, quantum);
        false
    }
}

/// The main-thread side of a plugin that is rendering.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once.
pub trait Controller: Control + Send {
    /// The loaded plugin this rejoins into.
    type Loaded: Loaded<Controller = Self, Processor = Self::Processor>;
    /// The realtime half of the same activation.
    type Processor: Processor;

    /// The latency the plugin reports now.
    fn latency(&self, main: MainThreadMarker) -> Samples;

    /// The tail the plugin reports now.
    fn tail(&self, main: MainThreadMarker) -> PluginTail;
}

/// A [`Control`] that is a loaded plugin's own, with what an activation adds.
///
/// An in-process format's [`Controller`] holds the [`Loaded`] it was split from
/// and answers the whole control surface through it. What an activation adds is
/// one concern: the Controller owns the pipeline of edits to and from the
/// Processor ([`edit_slots`](crate::edit_slots()) for host writes, a report drain for
/// what the plugin reported, the ranges the Processor denormalizes against).
/// This trait names the plugin and the few points where that pipeline joins the
/// plugin's answers; [`Control`] and [`EditorWindow`] are implemented once for
/// every `Wraps`, here, so a format writes only those points. Every point
/// defaults to nothing, so a format with no such pipeline (VST2) writes none.
///
/// A served controller holds no plugin (its plugin is across the wire) and does
/// not implement this: it implements [`Control`] itself.
pub trait Wraps {
    /// The plugin whose surface this one answers.
    type Plugin: Control;

    /// Whether a host write reaches the plugin itself. `false` where the
    /// activated plugin must not be written from the main thread (CLAP's
    /// `params.flush` belongs to the audio thread), so the write only goes to
    /// the Processor ([`queue_write`](Self::queue_write)).
    const WRITES_PLUGIN: bool = true;

    /// The plugin.
    fn plugin(&self) -> &Self::Plugin;

    /// The plugin, mutably.
    fn plugin_mut(&mut self) -> &mut Self::Plugin;

    /// A host write the Processor was handed through a block: tells the
    /// plugin's edit controller where the format has one of its own
    /// ([`Control::inform_controller`]).
    fn inform_controller(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized) {
        let _ = (main, id, value);
    }

    /// A host write, after the plugin took it (or instead of it): hands it to
    /// the Processor.
    fn queue_write(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized) {
        let _ = (main, id, value);
    }

    /// A state or a preset load is about to run: the writes queued before it
    /// are stale, so they are voided first, before the plugin's own load.
    ///
    /// A block that began draining before this may still apply what it had
    /// already collected: it overlapped the load, as a block overlaps any host
    /// call. Every block that starts after this returns sees the new epoch and
    /// applies no write from before it.
    fn before_restore(&mut self, main: MainThreadMarker) {
        let _ = main;
    }

    /// Loads a preset. The default is [`restore_preset`](Self::restore_preset);
    /// a format overrides it to refuse a preset up front, without touching the
    /// plugin.
    ///
    /// A refusal that never reaches the plugin voids no write. A preset that
    /// reaches the plugin and then fails still voids the pending writes, since
    /// the plugin may be half-applied.
    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        self.restore_preset(main, id)
    }

    /// The generic preset load: [`before_restore`](Self::before_restore), the
    /// plugin's own load, then [`after_restore`](Self::after_restore) if it
    /// loaded.
    fn restore_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        self.before_restore(main);
        let loaded = self.plugin_mut().load_preset(main, id);
        if loaded {
            self.after_restore(main);
        }
        loaded
    }

    /// A state or a preset load took: whatever it carried is owed to the
    /// Processor.
    fn after_restore(&mut self, main: MainThreadMarker) {
        let _ = main;
    }

    /// The state, with the writes the Processor has not surely taken yet beside
    /// it. Defaults to the plugin's own.
    ///
    /// # Errors
    ///
    /// As [`Control::save_state`].
    fn save_with_pending(
        &mut self,
        main: MainThreadMarker,
        state_use: StateUse,
    ) -> PluginResult<Vec<u8>> {
        self.plugin_mut().save_state(main, state_use)
    }

    /// Appends what the Processor reported to `out`, after the plugin's own,
    /// which start at `from`.
    fn collect_reports(&mut self, main: MainThreadMarker, out: &mut Vec<ParamEdit>, from: usize) {
        let _ = (main, out, from);
    }

    /// The reports the Controller's own pipeline lost, beside the plugin's.
    fn lost_reports(&self) -> u64 {
        0
    }

    /// A rescan changed what the parameters are: the ranges the Processor
    /// denormalizes against are read again and republished.
    fn republish_ranges(&mut self, main: MainThreadMarker) {
        let _ = main;
    }

    /// The control-side tick (every [`Event::Tick`](crate::Event::Tick)): apply
    /// what the Processor reported, and let retired published values go.
    fn tick(&mut self, main: MainThreadMarker) {
        let _ = main;
    }
}

impl<W: Wraps> Control for W {
    fn info(&self) -> &PluginDescriptor {
        self.plugin().info()
    }

    fn wiring(&self, main: MainThreadMarker) -> LoadedPlugin {
        self.plugin().wiring(main)
    }

    fn parameters(&self, main: MainThreadMarker) -> Vec<ParameterInfo> {
        self.plugin().parameters(main)
    }

    fn controls(&self, main: MainThreadMarker) -> Vec<ParameterInfo> {
        self.plugin().controls(main)
    }

    /// A bypass that is a parameter is written as every parameter is, so an
    /// active plugin's reaches its Processor
    /// ([`queue_write`](Wraps::queue_write)); one that is not (AU's unit
    /// property) is the plugin's own to write.
    fn set_bypassed(&mut self, main: MainThreadMarker, bypassed: bool) -> BypassWrite {
        write_bypass_parameter(self, main, bypassed)
            .unwrap_or_else(|| self.plugin_mut().set_bypassed(main, bypassed))
    }

    fn bypassed(&self, main: MainThreadMarker) -> Option<bool> {
        self.plugin().bypassed(main)
    }

    fn parameter_count(&self, main: MainThreadMarker) -> usize {
        self.plugin().parameter_count(main)
    }

    fn parameter(&self, main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
        self.plugin().parameter(main, id)
    }

    fn parameter_writable(
        &self,
        main: MainThreadMarker,
        id: ParamId,
    ) -> Result<(), ParamWriteError> {
        self.plugin().parameter_writable(main, id)
    }

    fn inform_controller(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized) {
        Wraps::inform_controller(self, main, id, value);
    }

    fn set_parameter(
        &mut self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Result<(), ParamWriteError> {
        if W::WRITES_PLUGIN {
            self.plugin_mut().set_parameter(main, id, value)?;
        } else {
            // The plugin is not written from here, but the policy still
            // answers at this call: a write the plugin would refuse is not
            // queued to the Processor either.
            self.plugin().parameter_writable(main, id)?;
        }
        self.queue_write(main, id, value);
        Ok(())
    }

    fn parameter_plain(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64> {
        self.plugin().parameter_plain(main, id, value)
    }

    fn parameter_normalized(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        self.plugin().parameter_normalized(main, id, plain)
    }

    fn parameter_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String> {
        self.plugin().parameter_text(main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.plugin().parameter_value_from_text(main, id, text)
    }

    fn save_state(&mut self, main: MainThreadMarker, state_use: StateUse) -> PluginResult<Vec<u8>> {
        self.save_with_pending(main, state_use)
    }

    fn load_state(
        &mut self,
        main: MainThreadMarker,
        bytes: &[u8],
        state_use: StateUse,
    ) -> PluginResult<()> {
        self.before_restore(main);
        let result = self.plugin_mut().load_state(main, bytes, state_use);
        self.after_restore(main);
        result
    }

    fn presets(&mut self, main: MainThreadMarker) -> Vec<Preset> {
        self.plugin_mut().presets(main)
    }

    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        Wraps::load_preset(self, main, id)
    }

    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId> {
        self.plugin_mut().current_preset(main)
    }

    fn set_automation_state(&mut self, main: MainThreadMarker, mode: AutomationMode) -> bool {
        self.plugin_mut().set_automation_state(main, mode)
    }

    fn rescan_parameters(&mut self, main: MainThreadMarker, scope: RescanScope) {
        self.plugin_mut().rescan_parameters(main, scope);
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.republish_ranges(main);
        }
    }

    fn set_track_info(&mut self, main: MainThreadMarker, info: &TrackInfo) -> Delivered {
        self.plugin_mut().set_track_info(main, info)
    }

    fn rebuild_midi_mapping(&mut self, main: MainThreadMarker) {
        self.plugin_mut().rebuild_midi_mapping(main);
    }

    fn flush_edits(&mut self, main: MainThreadMarker) {
        self.plugin_mut().flush_edits(main);
        self.tick(main);
    }

    fn run_main_thread(&mut self, main: MainThreadMarker) {
        self.plugin_mut().run_main_thread(main);
    }

    fn run_timer(&mut self, main: MainThreadMarker, id: TimerId) {
        self.plugin_mut().run_timer(main, id);
    }

    fn service_fd(&mut self, main: MainThreadMarker, id: FdId) {
        self.plugin_mut().service_fd(main, id);
    }

    fn take_requests(&mut self, main: MainThreadMarker, out: &mut Vec<Request>) {
        self.plugin_mut().take_requests(main, out);
    }

    fn render_mode_support(&self, main: MainThreadMarker) -> RenderModeSupport {
        self.plugin().render_mode_support(main)
    }

    fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> bool {
        self.plugin_mut().set_render_mode(main, mode)
    }

    fn take_reported_edits(&mut self, main: MainThreadMarker, out: &mut Vec<ParamEdit>) {
        let from = out.len();
        self.plugin_mut().take_reported_edits(main, out);
        self.collect_reports(main, out, from);
    }

    fn dropped_reported_edits(&self) -> u64 {
        self.plugin().dropped_reported_edits() + self.lost_reports()
    }

    fn bind_wake(&self, wake: ProcessWake) {
        self.plugin().bind_wake(wake);
    }
}

impl<W: Wraps> EditorWindow for W {
    fn has_editor(&self, main: MainThreadMarker) -> bool {
        self.plugin().has_editor(main)
    }

    fn open_editor(
        &mut self,
        main: MainThreadMarker,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<Size> {
        self.plugin_mut().open_editor(main, parent, scale)
    }

    fn open_floating_editor(&mut self, main: MainThreadMarker) -> PluginResult<()> {
        self.plugin_mut().open_floating_editor(main)
    }

    fn set_editor_scale(
        &mut self,
        main: MainThreadMarker,
        scale: ContentScale,
    ) -> PluginResult<()> {
        self.plugin_mut().set_editor_scale(main, scale)
    }

    fn editor_key(&mut self, main: MainThreadMarker, event: KeyEvent) -> PluginResult<KeyUse> {
        self.plugin_mut().editor_key(main, event)
    }

    fn close_editor(&mut self, main: MainThreadMarker) {
        self.plugin_mut().close_editor(main);
    }

    fn editor_idle(&mut self, main: MainThreadMarker) {
        self.plugin_mut().editor_idle(main);
    }

    fn wakeups(&mut self, main: MainThreadMarker, out: &mut Wakeups) {
        self.plugin_mut().wakeups(main, out);
    }

    fn wake(&mut self, main: MainThreadMarker, ready: &[Ready], now: Instant) {
        self.plugin_mut().wake(main, ready, now);
    }

    fn editor_capabilities(&mut self, main: MainThreadMarker) -> EditorCapabilities {
        self.plugin_mut().editor_capabilities(main)
    }

    fn set_editor_size(&mut self, main: MainThreadMarker, requested: Size) -> PluginResult<Size> {
        self.plugin_mut().set_editor_size(main, requested)
    }

    fn poll_editor_resize_request(&mut self, main: MainThreadMarker) -> Option<Size> {
        self.plugin_mut().poll_editor_resize_request(main)
    }
}

/// A rebuilt instance ([`Loaded::rebuild`]) and whether it took the state the
/// old one was saved with.
#[derive(Debug)]
pub struct Rebuilt<L> {
    /// The new instance, loaded. When [`restored`](Self::restored) is an
    /// error it holds its defaults.
    pub plugin: L,
    /// `Ok` when the new instance took the state the old one was saved with
    /// (an empty state is taken). Otherwise why it did not: the old instance
    /// would not save its state, or the new one refused it. A format with no
    /// state to carry over reports `Ok` ([`whole`](Rebuilt::whole)).
    pub restored: Result<(), PluginError>,
}

impl<L> Rebuilt<L> {
    /// `plugin`, rebuilt with nothing to restore.
    #[must_use]
    pub fn whole(plugin: L) -> Self {
        Self {
            plugin,
            restored: Ok(()),
        }
    }
}

/// An activation or rebuild that did not take, and the plugin it left.
#[derive(Debug)]
pub struct Refused<L> {
    /// The plugin, loaded. `None` only where a format cannot hand it back (AU,
    /// when both the activation and its compensating uninitialize fail);
    /// [`Lifecycle`](crate::Lifecycle) then poisons.
    pub plugin: Option<L>,
    /// Why.
    pub reason: PluginError,
}

/// A deactivation that did not take: both halves are as they were.
#[derive(Debug)]
pub struct Stuck<P, C> {
    /// The realtime half.
    pub processor: P,
    /// The main-thread half.
    pub controller: C,
    /// Why.
    pub reason: PluginError,
}

/// Runs one block of `inputs` and `outputs` through `processor`. The outputs are
/// borrowed again into a table of this call's own, because [`PluginIo`] holds
/// one lifetime for tables and data and a `&mut [&mut _]` is invariant in the
/// data's. The tables are stack arrays ([`ChannelTable`]), so this is callable
/// on the audio thread. No inputs is an empty `inputs`; no outputs an empty
/// `outputs`; the block has its `frames` either way.
///
/// # Errors
///
/// [`PluginError::Block`] when the channels differ in length, and
/// [`PluginError::Process`] when the plugin failed the block.
pub fn process_block<P: Processor, S: Float>(
    processor: &mut P,
    inputs: &[&[S]],
    outputs: &mut [&mut [S]],
    frames: Samples,
    ctx: &ProcessContext<'_>,
    out: &mut ProcessOutput,
) -> PluginResult<()> {
    let mut table: ChannelTable<&mut [S]> = outputs.iter_mut().map(|o| &mut **o).collect();
    let buffer = InputOutput::from_slices(frames, inputs, &mut table)
        .map_err(|_| PluginError::Block("the block's channels differ in length"))?;
    processor.process(&mut PluginIo { buffer, ctx, out })
}

/// One block's buffers at the sample width `S`, inputs and outputs.
#[derive(Debug)]
pub struct PluginIo<'a, S: Float> {
    /// The channel tables.
    pub buffer: InputOutput<'a, S>,
    /// The block's MIDI, transport and automation.
    pub ctx: &'a ProcessContext<'a>,
    /// What the plugin emitted: MIDI, parameter changes, note expression.
    pub out: &'a mut ProcessOutput,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateUse;
    use crate::fake::{Call, FakeLoaded, FakePlugin};

    /// A [`Wraps`] over the fake that records, at each restore hook, how many
    /// state loads the plugin had run.
    struct Rig {
        plugin: FakeLoaded,
        handle: FakePlugin,
        hooks: Vec<(&'static str, usize)>,
        reaches: bool,
    }

    impl Rig {
        fn new(reaches: bool) -> Self {
            let (handle, plugin) = FakePlugin::new();
            Self {
                plugin,
                handle,
                hooks: Vec::new(),
                reaches,
            }
        }

        fn loads(&self) -> usize {
            self.handle
                .calls()
                .iter()
                .filter(|call| matches!(call, Call::LoadState(_)))
                .count()
        }
    }

    impl Wraps for Rig {
        type Plugin = FakeLoaded;

        fn plugin(&self) -> &FakeLoaded {
            &self.plugin
        }

        fn plugin_mut(&mut self) -> &mut FakeLoaded {
            &mut self.plugin
        }

        fn before_restore(&mut self, _main: MainThreadMarker) {
            let loads = self.loads();
            self.hooks.push(("before", loads));
        }

        fn after_restore(&mut self, _main: MainThreadMarker) {
            let loads = self.loads();
            self.hooks.push(("after", loads));
        }

        fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
            self.reaches && self.restore_preset(main, id)
        }
    }

    /// **A state load voids before the plugin's load runs, and takes after.**
    /// `before_restore` sees no load yet and `after_restore` sees it: moving the
    /// void behind the load fails here.
    #[test]
    fn a_state_load_runs_before_restore_ahead_of_the_plugins_load() {
        let main = tutti_fixtures::main_thread();
        let mut rig = Rig::new(true);
        Control::load_state(&mut rig, main, &[1, 2, 3], StateUse::Project).unwrap();
        assert_eq!(rig.hooks, [("before", 0), ("after", 1)]);
    }

    /// **A preset the format refuses up front voids nothing**: no restore hook
    /// runs, so the writes queued stay.
    #[test]
    fn a_refused_preset_runs_no_restore_hook() {
        let main = tutti_fixtures::main_thread();
        let mut rig = Rig::new(false);
        assert!(!Control::load_preset(&mut rig, main, &PresetId::Number(1)));
        assert!(rig.hooks.is_empty());
    }
}
