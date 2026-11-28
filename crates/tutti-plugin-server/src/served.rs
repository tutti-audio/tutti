//! [`Server<L>`]: one plugin of any [`OutOfProcess`] format, in this process.
//!
//! The host crate of each format is the interpreter of its plugin's lifecycle
//! (activation, restarts, requests). This is the server's one shell over them:
//! it loads through [`OutOfProcess::load_served`], activates through
//! [`Loaded::activate`](tutti_plugin_types::Loaded::activate) into the realtime [`Processor`](tutti_plugin_types::Processor) and the main-thread
//! [`Controller`](tutti_plugin_types::Controller), and runs both through
//! [`Lifecycle`], so a format is a type argument and a row of the
//! `served!` table, not a loader.
//!
//! What the formats still differ in, that only this process does, is
//! [`ServerSide`]'s.
//!
//! What the plugin tells the host between blocks is [`Server::poll`]'s answer:
//! each [`Notice`] of the core as the [`News`] that carries it.

use crate::ties::{Patience, Ties};
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_buffers::FloatWidth;
use tutti_plugin_ipc::{ModeOutcome, News};
use tutti_plugin_types::{
    AutomationMode, Control, Delivered, Event, Features, Hand, HostOp, Layout, Lifecycle,
    LoadStage, MainThreadMarker, MainWorkHandle, MaxBlock, MidiAssignment, MidiMapApplied, Notice,
    OutOfProcess, ParamEdit, ParameterInfo, PluginClass, PluginDescriptor, PluginError, PluginIo,
    PluginLearnError, PluginPreset, PluginRef, PluginResult, PluginState, PluginWiring, Prepare,
    PresetError, PresetId, ProcessWake, RateRefusal, RateRefusalCause, Recovery, RenderMode,
    RenderModeSupport, Request, SampleRate, Samples, Seat, ServerLosses, Stage, State, StateError,
    StateUse, TrackInfo, Widths,
};
use tutti_rt::TallyWriter;
use tutti_units::Normalized;
use tutti_units::ParamId;

mod any;
#[cfg(served_au)]
mod au;
mod format;

pub(crate) use any::AnyServer;
pub(crate) use format::ServerSide;

#[cfg(test)]
mod tests;

/// What the plugin is asked to render with: a block's audio, in the width the
/// plugin was activated at.
///
/// The pipeline's one door into a plugin, so its tests drive a stand-in that
/// is not a plugin. Object-safe, so a render seat holds its plugin with the
/// format erased; one method per width the shared memory carries. A plugin not
/// running (a rate it refused left it deactivated) fills the block with
/// silence. `io.out` is the caller's and reused: it is cleared before it is
/// filled.
pub(crate) trait Render {
    /// What the plugin can consume, as it reports now.
    fn features(&self) -> Features;

    /// Renders one 32-bit block.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] (or another variant) when the plugin failed
    /// the block.
    fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()>;

    /// Renders one 64-bit block.
    ///
    /// # Errors
    ///
    /// As [`render_f32`](Self::render_f32).
    fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()>;
}

/// What the session asks of the loaded plugin, with its format erased.
///
/// Object-safe, implemented by [`Server<L>`] alone; the
/// `served!` table's enum lends it as a trait object.
pub(crate) trait ServerPlugin {
    /// The file and plugin the load opened: the id is the plugin's, whatever
    /// the load chose.
    fn origin(&self) -> &PluginRef;
    /// The plugin's catalog identity.
    fn descriptor(&self) -> &PluginDescriptor;
    /// The plugin's wiring as last read: buses, latency, tail, capabilities.
    fn loaded(&self) -> &PluginWiring;
    /// The sample format the session renders this plugin in: 64-bit only when
    /// it was asked for **and** the plugin runs at it ([`Features::F64_AUDIO`]),
    /// never the request alone.
    fn negotiated(&self, requested: FloatWidth) -> FloatWidth {
        Widths::new(self.loaded().features.contains(Features::F64_AUDIO)).negotiate(requested)
    }
    /// The plugin's control surface, which is also its editor's window calls
    /// ([`EditorWindow`](tutti_plugin_types::EditorWindow)): the editor of a
    /// served plugin opens on the instance itself.
    /// `None` when the plugin is lost.
    fn control_mut(&mut self) -> Option<&mut dyn Control>;
    /// Acts on what the plugin asked of the host since the last call, and
    /// returns what the host should be told. Between blocks, on the thread
    /// that loaded the plugin.
    fn poll(&mut self, main: MainThreadMarker) -> Vec<News>;
    /// Whether the plugin is lost: a call into it panicked, and nothing of it
    /// runs any more.
    fn is_lost(&self) -> bool;
    /// Brings the Processor home from the render thread and deactivates the
    /// plugin, so it can be dropped. `false` when the Processor did not come
    /// (a render thread stuck inside the plugin): dropping the plugin then
    /// leaks it.
    fn quiesce(&mut self, main: MainThreadMarker) -> bool;
    /// Follows the host's rate.
    ///
    /// # Errors
    ///
    /// The plugin is not running at `rate`: it keeps the rate it already
    /// accepted, or renders silence until a rate it takes.
    fn set_sample_rate(
        &mut self,
        main: MainThreadMarker,
        rate: SampleRate,
    ) -> Result<(), RateRefusal>;
    /// Tells the plugin its render mode, and what it made of it.
    fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> ModeOutcome;
    /// How many parameters the plugin has.
    fn parameter_count(&self, main: MainThreadMarker) -> usize;
    /// Every parameter.
    fn parameters(&self, main: MainThreadMarker) -> Vec<ParameterInfo>;
    /// A parameter's value, normalized.
    fn parameter(&self, main: MainThreadMarker, id: ParamId) -> Option<Normalized>;
    /// The host's automation mode.
    fn set_automation_state(&mut self, main: MainThreadMarker, mode: AutomationMode);
    /// What the host says of the plugin's track.
    fn set_track_info(&mut self, main: MainThreadMarker, info: &TrackInfo);
    /// The plugin's wiring read now (its declared controller table included).
    fn wiring(&mut self, main: MainThreadMarker) -> PluginWiring;
    /// The app's controller entries; who applies them.
    fn set_app_midi_map(
        &mut self,
        main: MainThreadMarker,
        app: &[MidiAssignment],
    ) -> MidiMapApplied;
    /// Arms or disarms the plugin's own MIDI learn.
    fn arm_plugin_midi_learn(
        &mut self,
        main: MainThreadMarker,
        armed: bool,
    ) -> Result<(), PluginLearnError>;
    /// The plain value of `value`, by the plugin's format.
    fn parameter_plain(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64>;
    /// The normalized value of a plain one, by the plugin's format.
    fn parameter_normalized(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized>;
    /// The plugin's own text for `value`.
    fn parameter_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String>;
    /// The value the plugin parses from `text`.
    fn parameter_value_from_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized>;
    /// The plugin's presets.
    fn presets(&mut self, main: MainThreadMarker) -> Vec<PluginPreset>;
    /// Loads a preset: what the plugin's control answered, `Lost` when it has none.
    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> Result<(), PresetError>;
    /// The preset the plugin is on.
    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId>;
    /// The plugin's state, and beside it the writes its control holds that no
    /// block has applied (a preset load's, say), read before the bytes. Never
    /// brings the Processor home and never waits for a block.
    ///
    /// # Errors
    ///
    /// The plugin cannot save, or is lost.
    fn save_state(
        &mut self,
        main: MainThreadMarker,
        state_use: StateUse,
    ) -> PluginResult<PluginState>;
    /// Restores a saved state.
    ///
    /// # Errors
    ///
    /// The plugin rejects the bytes, or is lost.
    fn load_state(
        &mut self,
        main: MainThreadMarker,
        data: &[u8],
        state_use: StateUse,
    ) -> PluginResult<()>;
}

/// A plugin of format `L` that the server runs: its [`Lifecycle`], the
/// Processor's seat, the core's state, and what the host has not yet been told.
pub(crate) struct Server<L: OutOfProcess> {
    lifecycle: Lifecycle<L>,
    state: State,
    hand: Hand<L::Processor>,
    /// The file and plugin the load opened.
    origin: PluginRef,
    descriptor: PluginDescriptor,
    wiring: PluginWiring,
    /// The buses the host was last told: at load, or by `PluginIoChanged`.
    announced: Layout,
    /// What the plugin asked of the host since the last pass, reused.
    requests: Vec<Request>,
    /// The parameter edits the plugin made since the last pass, reused.
    edits: Vec<ParamEdit>,
    /// What the host has not yet been told.
    events: Vec<News>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
    /// What wakes the render thread, and what it wakes. Its bell is rung by
    /// the plugin's request to be processed again (any thread), and taken by
    /// `poll`, which tells the host.
    ties: Ties,
    /// Adds to [`Losses::edits_refused`](crate::render::Losses): one-way edits
    /// the plugin refused.
    edits_refused: TallyWriter<u64>,
    /// The loss counts last told to the host ([`News::ServerLosses`]).
    told_losses: ServerLosses,
}

/// How long a plan that waits for the Processor to come home from the render
/// thread is given: the render thread returns it at its next poll (a few
/// milliseconds), so this is a render thread that is stuck inside the plugin.
const HOME_PATIENCE: Duration = Duration::from_secs(3);

/// A Processor that is still on the render thread when the plugin is dropped
/// must not outlive the Controller that destroys the plugin, so the plugin is
/// leaked instead: only a render thread stuck inside the plugin reaches it.
impl<L: OutOfProcess> Drop for Server<L> {
    fn drop(&mut self) {
        if matches!(
            self.lifecycle,
            Lifecycle::Active {
                seat: Seat::Lent,
                ..
            }
        ) {
            tracing::error!(
                "plugin-server: the render thread still holds the processor; leaking the plugin"
            );
            crate::leak::note();
            std::mem::forget(std::mem::replace(&mut self.lifecycle, Lifecycle::Poisoned));
        }
    }
}

fn lost() -> PluginError {
    PluginError::StateFailed(StateError::PluginCrashed)
}

fn load_failed(stage: LoadStage, reason: String) -> PluginError {
    PluginError::Load { stage, reason }
}

impl<L: ServerSide> Server<L> {
    /// Loads the plugin `plugin` names, not activated.
    ///
    /// # Errors
    ///
    /// [`PluginError::Load`], with how far the load got.
    pub(crate) fn open(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<L> {
        let mut loaded = L::load_served(main, work, plugin)?;
        loaded.adopt();
        Ok(loaded)
    }

    /// [`open`](Self::open) and [`activate`](Self::activate).
    ///
    /// # Errors
    ///
    /// As those.
    #[allow(clippy::too_many_arguments)] // one load's parameters, as the wire carries them
    pub(crate) fn load(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
        sample_rate: SampleRate,
        block_size: usize,
        requested: FloatWidth,
        state: Option<(&[u8], StateUse)>,
        before_restore: &mut dyn FnMut(),
        hand: Hand<L::Processor>,
        ties: Ties,
    ) -> PluginResult<(Self, Option<PluginError>)> {
        Self::activate(
            main,
            Self::open(main, work, plugin)?,
            sample_rate,
            block_size,
            requested,
            state,
            before_restore,
            hand,
            ties,
        )
    }

    /// Activates an opened plugin at `sample_rate` for blocks of up to
    /// `block_size`, restoring `state` **before** the activation: a plugin may
    /// take its mode from the state, and the activation, and the wiring this
    /// instance reports, are then the state's. The second value is the plugin's
    /// refusal of the state, if it refused: the plugin is then activated at its
    /// defaults (a plugin at its defaults beats none).
    ///
    /// With a `requested` of 64-bit the plugin is activated at 64-bit when
    /// [`Loaded::widths`](tutti_plugin_types::Loaded::widths) says it takes it just before the activation, else at
    /// 32-bit; a plugin that refuses the width it declared fails the activation.
    /// What it runs at is its wiring's [`Features::F64_AUDIO`], never the request.
    ///
    /// `hand` is the Processor's way to the thread that renders: the server's
    /// is [`Hand::threaded`], whose other end is the render thread's seat
    /// ([`crate::render::End`]); a [`Hand::local`] keeps the Processor in this
    /// value.
    ///
    /// # Errors
    ///
    /// [`PluginError::Load`] at [`LoadStage::Activation`] when no activation
    /// took.
    pub(crate) fn activate(
        main: MainThreadMarker,
        mut loaded: L,
        sample_rate: SampleRate,
        block_size: usize,
        requested: FloatWidth,
        state: Option<(&[u8], StateUse)>,
        before_restore: &mut dyn FnMut(),
        hand: Hand<L::Processor>,
        ties: Ties,
    ) -> PluginResult<(Self, Option<PluginError>)> {
        let activation_failed = |reason: String| {
            load_failed(
                LoadStage::Activation,
                format!("the activation failed: {reason}"),
            )
        };
        let prepare = MaxBlock::new(Samples::new(block_size))
            .map(|block| Prepare::new(sample_rate, block))
            .ok_or_else(|| {
                activation_failed(format!(
                    "the plugin cannot run in blocks of {block_size} frames"
                ))
            })?;

        // The width is chosen from the plugin's widths now, once: a plugin that
        // refuses the width it declared fails the activation.
        let format = loaded.widths(main).negotiate(requested);
        let mut this = Self {
            origin: loaded.origin().clone(),
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<L>::initial_state(),
            hand,
            descriptor: PluginDescriptor::new("", PluginClass::Unknown),
            wiring: PluginWiring::default(),
            announced: Layout::default(),
            requests: Vec::new(),
            edits: Vec::new(),
            events: Vec::new(),
            refused: false,
            edits_refused: ties.edits_refused.writer(),
            told_losses: ServerLosses::default(),
            ties,
        };
        // The wake outlives every activation: the plugin's host state is the
        // loaded plugin's.
        if let Some(control) = this.lifecycle.control() {
            let bell = this.ties.bell.clone();
            control.bind_wake(ProcessWake::new(move || bell.ring()));
        }
        // Before the activation: see above.
        let state_error = state.and_then(|(data, state_use)| {
            before_restore();
            match this.lifecycle.control() {
                Some(control) => control.load_state(main, data, state_use).err(),
                None => Some(lost()),
            }
        });
        let refusal = this.drive(
            main,
            Event::Op(HostOp::Activate(prepare.with_format(format))),
        );
        if this.state.stage() != Stage::Active {
            return Err(activation_failed(
                refusal.map_or_else(|| "refused".to_string(), |r| r.reason.to_string()),
            ));
        }
        this.refresh_wiring(main);
        this.announced = Layout::from(&this.wiring);
        // Whatever the activation told the host is what the load reply carries.
        this.events.clear();
        Ok((this, state_error))
    }

    /// Steps one event through the core, applying its effects. Returns the
    /// reason of a refusal it told, if any.
    fn drive(&mut self, main: MainThreadMarker, event: Event) -> Option<Failure> {
        let mut told = Vec::new();
        // Taken, not cloned: a clone copies the layout's `Vec`s. The state put
        // in its place is built without allocating and is replaced below.
        let state = std::mem::replace(&mut self.state, Lifecycle::<L>::initial_state());
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, main, |n| told.push(n));
        // The Processor may have been lent or recalled: the render thread
        // trades it at its next wake.
        self.ties.wake.ring();
        let mut refusal = None;
        for notice in told {
            if let Some(reason) = self.absorb(main, notice) {
                refusal = Some(reason);
            }
        }
        refusal
    }

    /// [`drive`](Self::drive), then, while the plan is parked for the Processor
    /// to come home from the render thread, ticks until it has: the render
    /// thread returns it at its next wake and says so ([`Settle`](crate::ties::Settle)),
    /// which ends this wait. Bounded by [`HOME_PATIENCE`]: a render thread stuck
    /// inside the plugin leaves the plan parked and the refusal says so.
    fn drive_settled(&mut self, main: MainThreadMarker, event: Event) -> Option<Failure> {
        let mut refusal = self.drive(main, event);
        let patience = Patience::new(Instant::now(), HOME_PATIENCE);
        while self.state.is_parked() {
            if patience.left(Instant::now()).is_none() {
                return refusal.or_else(|| {
                    Some(Failure {
                        reason: Arc::new(PluginError::Other(
                            "the render thread did not hand the processor back".to_string(),
                        )),
                        recovery: Recovery::Inactive,
                    })
                });
            }
            // Read before the tick that collects: a return after it moves the
            // count past this and ends the wait at once.
            let seen = self.ties.settle.seen();
            let ticked = self.drive(main, Event::Tick);
            refusal = refusal.or(ticked);
            if self.state.is_parked() {
                self.ties.settle.wait_past(seen, patience);
            }
        }
        refusal
    }

    /// Turns a notice into what the host hears, keeping the cached wiring in
    /// step. Returns the reason when the notice is a refusal.
    fn absorb(&mut self, main: MainThreadMarker, notice: Notice) -> Option<Failure> {
        match notice {
            Notice::LatencyChanged(samples) => {
                self.wiring.latency_samples = samples;
                self.events.push(News::LatencyChanged { samples });
            }
            Notice::TailChanged(tail) => {
                self.wiring.tail = tail;
                self.events.push(News::TailChanged { tail });
            }
            Notice::IoChanged => {
                self.refresh_wiring(main);
                let io = self.io_changed();
                self.events.push(io);
            }
            Notice::ParamValuesChanged => {
                self.events.push(News::PluginParamValuesChanged);
            }
            Notice::ParamTitlesChanged => {
                self.events.push(News::PluginParamTitlesChanged);
            }
            Notice::MidiMapChanged => {
                self.events.push(News::PluginMidiMapChanged);
            }
            Notice::Reloaded => {
                // A reload may change the buses: the host is told them first,
                // so its picture is current when it re-reads everything.
                self.refresh_wiring(main);
                let io = self.io_changed();
                if matches!(io, News::PluginIoChanged { layout: Some(_) }) {
                    self.events.push(io);
                }
                self.events.push(News::PluginReloaded);
            }
            Notice::Transport(request) => {
                self.events.push(News::TransportRequested { request });
            }
            // The wire carries no note-expression change, and nothing here acts
            // on one: the host reads the list when it opens a session.
            Notice::NoteExpressionChanged => {}
            Notice::Refused {
                rate,
                reason,
                recovery,
            } => {
                tracing::warn!("plugin refused {} Hz: {reason}", rate.get());
                // Whether the host must re-read the instance depends on whether
                // a rollback revives it: `finish_refusals` decides.
                self.refresh_wiring(main);
                self.refused = true;
                return Some(Failure { reason, recovery });
            }
            Notice::Stuck { reason } => {
                tracing::warn!("plugin would not stop: {reason}");
                self.events.push(News::PluginWouldNotStop {
                    reason: reason.to_string(),
                });
            }
            // The mode's reply says it.
            Notice::ModeRefused { .. } => {}
            Notice::StateNotRestored { reason } => {
                tracing::warn!("plugin did not restore its state after a reload: {reason}");
                self.events.push(News::PluginStateNotRestored {
                    reason: reason.to_string(),
                });
            }
            Notice::Lost { reason } => {
                // The wire has no "lost" message, and the buses are not what
                // changed: nothing is sent here. The loop ends the server on
                // `is_lost` (see `main_loop::serve_every`), so the host sees its
                // process exit as a crash.
                tracing::error!("plugin lost: {reason}");
                return Some(Failure {
                    reason: Arc::new(reason),
                    recovery: Recovery::Inactive,
                });
            }
        }
        None
    }

    /// Tells the host a plugin that stayed inactive after a refusal now renders
    /// silence, so it re-reads it. A refusal a rollback undid tells nothing.
    fn finish_refusals(&mut self) {
        if std::mem::take(&mut self.refused) && !self.is_active() {
            let io = self.io_changed();
            self.events.push(io);
        }
    }

    /// The `PluginIoChanged` the host is sent now: the buses the plugin has
    /// when they are not the ones the host was told last, and none otherwise.
    fn io_changed(&mut self) -> News {
        let now = Layout::from(&self.wiring);
        let layout = (now != self.announced).then(|| {
            self.announced = now.clone();
            now
        });
        News::PluginIoChanged { layout }
    }

    /// Reads the plugin's wiring again into what the host is given. A plugin
    /// with no control (lost) keeps the wiring it last had, so the host is
    /// told no new buses for it.
    fn refresh_wiring(&mut self, main: MainThreadMarker) {
        if let Some(control) = self.lifecycle.control() {
            self.descriptor = control.info().clone();
            self.wiring = control.wiring(main);
            // A bypass that is a property cannot be read back across the wire
            // (a read is a round trip), so a served plugin does not claim it: the
            // executor's bypass serves the node.
            if self.wiring.own_bypass == Some(tutti_plugin_types::OwnBypass::Property) {
                self.wiring.own_bypass = None;
            }
        }
    }

    /// Whether the plugin is active: a refused rate leaves it loaded and
    /// silent.
    fn is_active(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Active { .. })
    }

    /// [`ServerPlugin::poll`]'s pass, leaving what the host should be told in
    /// `events`.
    ///
    /// The plugin's main-thread work runs first (`take_requests` serves what
    /// the format has: CLAP's callbacks, timers and descriptors), since it may
    /// ask for the rest; a later pass takes only what was raised meanwhile
    /// ([`ServerSide::take_raised`]). Then the core decides what the requests need (a
    /// restart, a re-read) and the lifecycle performs it. A pass in which
    /// nothing was asked and the core has nothing pending (the idle pass every
    /// dispatch takes) does not enter the lifecycle and allocates nothing; a
    /// pass with work allocates, as a rate change does.
    fn serve(&mut self, main: MainThreadMarker) {
        // What a pass does may itself make the plugin ask for more (an
        // activation that raises an I/O change), so the word is taken again
        // until a pass finds it empty. Bounded, so a plugin that asks from
        // every callback cannot hold the thread.
        const MAX_PASSES: usize = 8;
        let mut requests = std::mem::take(&mut self.requests);
        for pass in 0..MAX_PASSES {
            requests.clear();
            if pass == 0 {
                if let Some(control) = self.lifecycle.control() {
                    control.take_requests(main, &mut requests);
                }
            } else {
                L::take_raised(&mut self.lifecycle, main, &mut requests);
            }
            let asked = !requests.is_empty();
            if !asked && self.state.is_quiet() {
                // The idle pass: all a tick would do is retry what a full edit
                // ring refused.
                if let Some(control) = self.lifecycle.control() {
                    control.flush_edits(main);
                }
                break;
            }
            if !asked && pass > 0 {
                break;
            }
            // A restart a request plans needs the Processor home, which the
            // render thread returns within a poll: the plan is waited for here,
            // so what it tells the host (a latency that moved) is told in this
            // pass, in order with the message that caused it.
            for request in requests.drain(..) {
                self.drive_settled(main, Event::Request(request));
            }
            self.drive_settled(main, Event::Tick);
        }
        self.requests = requests;
        self.finish_refusals();
    }

    /// Queues what the plugin edited of its own parameters since the last pass as
    /// [`News::ParameterChanged`], one per [`ParamEdit`], in the order
    /// the plugin made them: a gesture's `Begin`, its changes and its `End`
    /// reach the host in that order, since the wire and the host's inbox keep
    /// the order they were queued in.
    fn forward_edits(&mut self, main: MainThreadMarker) {
        let mut edits = std::mem::take(&mut self.edits);
        edits.clear();
        L::take_edits(&mut self.lifecycle, main, &mut edits);
        self.events
            .extend(edits.iter().map(|&edit| News::ParameterChanged { edit }));
        self.edits = edits;
    }

    /// Deactivates and activates at `wanted`, as a rate or mode change does.
    /// The core activates again at the configuration the plugin accepted when
    /// `wanted` is refused, and the failure says whether that took.
    fn reconfigure(&mut self, main: MainThreadMarker, wanted: Prepare) -> Result<(), Failure> {
        let result = match self.drive_settled(main, Event::Op(HostOp::Reconfigure(wanted))) {
            None => {
                // The plugin may take another layout, or another latency, at
                // another rate or in another mode.
                self.refresh_wiring(main);
                Ok(())
            }
            Some(failure) => Err(failure),
        };
        self.finish_refusals();
        result
    }
}

/// What a refusal or a loss told: why, and where the plugin stands.
struct Failure {
    reason: Arc<PluginError>,
    recovery: Recovery,
}

impl<L: ServerSide> ServerPlugin for Server<L> {
    fn origin(&self) -> &PluginRef {
        &self.origin
    }

    fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    fn loaded(&self) -> &PluginWiring {
        &self.wiring
    }

    fn control_mut(&mut self) -> Option<&mut dyn Control> {
        self.lifecycle.control()
    }

    fn poll(&mut self, main: MainThreadMarker) -> Vec<News> {
        self.serve(main);
        self.forward_edits(main);
        if self.ties.bell.take() {
            self.events.push(News::ProcessRequested);
        }
        // A poisoned plugin has no control surface: its last count stands.
        let losses = ServerLosses {
            commands_refused: self.ties.commands_refused.get(),
            edits_refused: self.ties.edits_refused.get(),
            detached_tasks: self.ties.detached.get(),
            lanes_lost: self.ties.lanes_lost.get(),
            refused_off_thread_calls: self
                .lifecycle
                .control()
                .map_or(self.told_losses.refused_off_thread_calls, |c| {
                    c.refused_off_thread_calls()
                }),
        };
        if losses != self.told_losses {
            self.told_losses = losses;
            self.events.push(News::ServerLosses {
                commands_refused: losses.commands_refused,
                edits_refused: losses.edits_refused,
                refused_off_thread_calls: losses.refused_off_thread_calls,
                detached_tasks: losses.detached_tasks,
                lanes_lost: losses.lanes_lost,
            });
        }
        std::mem::take(&mut self.events)
    }

    fn is_lost(&self) -> bool {
        self.lifecycle.is_poisoned()
    }

    fn quiesce(&mut self, main: MainThreadMarker) -> bool {
        self.drive_settled(main, Event::Op(HostOp::Deactivate));
        !matches!(
            self.lifecycle,
            Lifecycle::Active {
                seat: Seat::Lent,
                ..
            }
        )
    }

    /// The rate the plugin already runs at is a no-op, so a per-frame caller
    /// does not tear it down sixty times a second.
    ///
    /// A refusal is rolled back by the core: the plugin is activated again at
    /// the configuration it already accepted, and the error says it is running
    /// there. (The session still renders nothing after an error, because its
    /// clock follows the rate the host asked for.) When the rollback is refused
    /// too the plugin is left loaded and silent, and the host is told with an
    /// I/O change; the next rate revives it.
    fn set_sample_rate(
        &mut self,
        main: MainThreadMarker,
        rate: SampleRate,
    ) -> Result<(), RateRefusal> {
        let Some(previous) = self.state.applied() else {
            return Err(RateRefusal::new(rate, RateRefusalCause::Unconfigured));
        };
        if self.is_active() && previous.rate() == rate {
            return Ok(());
        }
        self.reconfigure(main, previous.with_rate(rate))
            .map_err(|failure| {
                let reason = failure.reason.to_string();
                RateRefusal::new(
                    rate,
                    match failure.recovery {
                        Recovery::RolledBack { running } => RateRefusalCause::RolledBack {
                            running: running.rate(),
                            reason,
                        },
                        Recovery::Inactive | Recovery::RollbackRefused { .. } => {
                            RateRefusalCause::Deactivated { reason }
                        }
                    },
                )
            })
    }

    /// What happens is the plugin's [`Control::render_mode_support`]: a `Live`
    /// plugin (CLAP's `render.set`, VST2's callback) is told; an
    /// `OnActivation` one (VST3's `setupProcessing`, AU's
    /// `AudioUnitInitialize`) is activated again in the mode, keeping its
    /// instance and state, and a refusal is rolled back to the mode it was
    /// running in; an `Unsupported` one is left running and answers
    /// [`Unsupported`](ModeOutcome::Unsupported) every time it is asked,
    /// whatever mode it was last asked. The mode is kept for every later
    /// activation (a restart, a rate change) either way. A plugin not prepared
    /// has nothing to tell: [`Refused`](ModeOutcome::Refused).
    fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> ModeOutcome {
        let Some(previous) = self.state.applied() else {
            return ModeOutcome::Refused;
        };
        let unchanged = previous.mode() == mode && self.is_active();
        let Some(control) = self.lifecycle.control() else {
            return ModeOutcome::Refused;
        };
        match control.render_mode_support(main) {
            RenderModeSupport::Unsupported => {
                self.state.want(previous.with_mode(mode));
                ModeOutcome::Unsupported
            }
            _ if unchanged => ModeOutcome::Took,
            RenderModeSupport::Live => {
                let _ = self.drive(main, Event::Op(HostOp::SetMode(mode)));
                let running = self.state.applied().map(|applied| applied.mode());
                if running == Some(mode) {
                    ModeOutcome::Took
                } else {
                    // Refused, or not told (the plugin is not active): the
                    // plugin runs as it did, and what the server wants goes
                    // back with the answer.
                    if let (Some(wanted), Some(running)) = (self.state.wanted(), running) {
                        self.state.want(wanted.with_mode(running));
                    }
                    ModeOutcome::Refused
                }
            }
            RenderModeSupport::OnActivation => {
                match self.reconfigure(main, previous.with_mode(mode)) {
                    Ok(()) => ModeOutcome::Took,
                    Err(_) => ModeOutcome::Refused,
                }
            }
        }
    }

    fn parameter_count(&self, main: MainThreadMarker) -> usize {
        self.lifecycle
            .control_ref()
            .map_or(0, |control| control.parameter_count(main))
    }

    fn parameters(&self, main: MainThreadMarker) -> Vec<ParameterInfo> {
        self.lifecycle
            .control_ref()
            .map(|control| control.parameters(main))
            .unwrap_or_default()
    }

    fn parameter(&self, main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
        self.lifecycle.control_ref()?.parameter(main, id)
    }

    /// Whether the plugin took the mode is not counted: `false` is the format
    /// having no such call (AU's every time), not a refusal.
    fn set_automation_state(&mut self, main: MainThreadMarker, mode: AutomationMode) {
        if let Some(control) = self.lifecycle.control() {
            let _ = control.set_automation_state(main, mode);
        }
    }

    /// The wire has no reply to this, so what the plugin's `Control` answers
    /// is not sent back: a track info that did not reach the plugin is counted
    /// ([`Losses::edits_refused`](crate::render::Losses)).
    fn set_track_info(&mut self, main: MainThreadMarker, info: &TrackInfo) {
        if let Some(control) = self.lifecycle.control() {
            let delivered = control.set_track_info(main, info);
            self.edits_refused
                .add(u64::from(delivered != Delivered::Yes));
        }
    }

    fn wiring(&mut self, main: MainThreadMarker) -> PluginWiring {
        match self.lifecycle.control() {
            Some(control) => control.wiring(main),
            None => self.wiring.clone(),
        }
    }

    fn set_app_midi_map(
        &mut self,
        main: MainThreadMarker,
        app: &[MidiAssignment],
    ) -> MidiMapApplied {
        self.lifecycle
            .control()
            .map_or(MidiMapApplied::Host, |c| c.set_app_midi_map(main, app))
    }

    fn arm_plugin_midi_learn(
        &mut self,
        main: MainThreadMarker,
        armed: bool,
    ) -> Result<(), PluginLearnError> {
        self.lifecycle
            .control()
            .ok_or(PluginLearnError::Lost)?
            .arm_plugin_midi_learn(main, armed)
    }

    fn parameter_plain(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64> {
        self.lifecycle
            .control_ref()?
            .parameter_plain(main, id, value)
    }

    fn parameter_normalized(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        self.lifecycle
            .control_ref()?
            .parameter_normalized(main, id, plain)
    }

    fn parameter_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String> {
        self.lifecycle
            .control_ref()?
            .parameter_text(main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.lifecycle
            .control_ref()?
            .parameter_value_from_text(main, id, text)
    }

    fn presets(&mut self, main: MainThreadMarker) -> Vec<PluginPreset> {
        self.lifecycle
            .control()
            .map(|control| control.presets(main))
            .unwrap_or_default()
    }

    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> Result<(), PresetError> {
        self.lifecycle
            .control()
            .ok_or(PresetError::Lost)?
            .load_preset(main, id)
    }

    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId> {
        self.lifecycle.control()?.current_preset(main)
    }

    fn save_state(
        &mut self,
        main: MainThreadMarker,
        state_use: StateUse,
    ) -> PluginResult<PluginState> {
        let control = self.lifecycle.control().ok_or_else(lost)?;
        let pending = control.pending();
        let bytes = control.save_state(main, state_use)?;
        Ok(PluginState { bytes, pending })
    }

    fn load_state(
        &mut self,
        main: MainThreadMarker,
        data: &[u8],
        state_use: StateUse,
    ) -> PluginResult<()> {
        self.lifecycle
            .control()
            .ok_or_else(lost)?
            .load_state(main, data, state_use)
    }
}
