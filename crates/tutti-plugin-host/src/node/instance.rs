//! The in-process plugin's control side: its [`Lifecycle`], with the
//! Processor lent to the node's realtime end through a threaded [`Hand`].
//!
//! The audio thread never reaches this: it renders through the Processor the
//! node's [`Rt`] holds, and takes no lock. The control side (the handle, the
//! node's `shape` and `prepare`, a fork) reaches it under one mutex that only
//! control threads take. A rate change is a recall, a rejoin and an
//! activation ([`HostOp::Reconfigure`]): the node's `prepare` runs it to the
//! end with its own realtime end, which is not in an executor then; a rate
//! pending for the main thread is run by the pump, and the Processor comes
//! back at the audio thread's next poll.

use std::sync::{Arc, OnceLock};
use tutti_plugin_types::{MainThreadMarker, MainWorkHandle};

use tutti_buffers::FloatWidth;
use tutti_plugin_types::{
    Event, Hand, HostOp, InProcess, IpcError, Layout, Lifecycle, Loaded, MaxBlock, Notice,
    PluginError, PluginFailure, PluginInvalidation, PluginRef, Prepare, Processor, RateRefusal,
    RateRefusalCause, RenderMode, RenderModeSupport, SampleRate, Samples, State, Successor, Widths,
    standing,
};
use tutti_rt::Mutex;
use tutti_rt::Rt;

use super::rebind::{RebindError, Settlement};
use crate::MidiMapSlot;

use tutti_plugin_types::ParamEdit;
use tutti_plugin_types::PluginTail;
use tutti_plugin_types::{IpcResult, StateError};

/// What the plugin changed about itself since the last
/// [`Instance::pump`]: each field is a re-read the core did after the
/// plugin's own request.
#[derive(Debug, Clone, Default)]
pub(super) struct Changes {
    /// The new latency, when it moved.
    pub(super) latency: Option<Samples>,
    /// The new tail, when it moved.
    pub(super) tail: Option<PluginTail>,
    /// The plugin raised [`Request::IoChanged`](tutti_plugin_types::Request::IoChanged): its pin counts or latency
    /// may differ.
    pub(super) io: bool,
    /// What the plugin asked the transport to do, in the order it asked.
    pub(super) transport: Vec<tutti_plugin_types::TransportRequest>,
    /// What the instance has to say of the plugin itself since the last
    /// pump, in the order it happened: a refused rate, the rate taken again
    /// after one, a restart that would not stop, the plugin's death.
    pub(super) reports: Vec<PluginInvalidation>,
}

/// What the lifecycle reported that stopped an event from taking effect.
#[derive(Debug, thiserror::Error)]
pub(super) enum Trouble {
    /// The plugin would not run at `rate`.
    #[error("{reason}")]
    Refused {
        /// The rate that was refused.
        rate: SampleRate,
        /// Why.
        reason: Arc<PluginError>,
    },
    /// The plugin would not stop: it runs as it was.
    #[error("{reason}")]
    Stuck {
        /// Why.
        reason: Arc<PluginError>,
    },
    /// A call into the plugin panicked: the plugin is lost.
    #[error("{reason}")]
    Lost {
        /// Why.
        reason: Arc<PluginError>,
    },
}

impl Trouble {
    fn reason(&self) -> &Arc<PluginError> {
        match self {
            Self::Refused { reason, .. } | Self::Stuck { reason } | Self::Lost { reason } => reason,
        }
    }
}

/// Whether the plugin is lost, and why: set once, by the instance, when its
/// lifecycle poisons, and readable with no lock by the nodes over the
/// instance and the handle's [`Health`](tutti_plugin_types::Health). A latch:
/// nothing else is published through it, so a node dropped while the control
/// side holds the instance reads it directly.
#[derive(Debug, Default)]
pub(super) struct Lost(OnceLock<PluginFailure>);

impl Lost {
    /// Whether the plugin is lost.
    pub(super) fn is_lost(&self) -> bool {
        self.0.get().is_some()
    }

    /// Why the plugin is lost, once it is.
    pub(super) fn cause(&self) -> Option<PluginFailure> {
        self.0.get().cloned()
    }

    /// Latches `cause`; `false` when the plugin was lost already.
    fn latch(&self, cause: PluginFailure) -> bool {
        self.0.set(cause).is_ok()
    }
}

/// The latency and tail the control side last read, readable while the
/// instance is locked: what a node's `shape` declares to the graph compile.
/// Copied in and out under a lock that is never held across a plugin call.
#[derive(Debug, Default)]
pub(super) struct Figures(Mutex<(Samples, PluginTail)>);

impl Figures {
    /// The latency and tail as last published.
    pub(super) fn get(&self) -> (Samples, PluginTail) {
        *self.0.lock()
    }

    fn publish(&self, latency: Samples, tail: PluginTail) {
        *self.0.lock() = (latency, tail);
    }
}

/// A plugin of hosting `L` and where it is in its life.
pub(super) struct Instance<L: Loaded> {
    lifecycle: Lifecycle<L>,
    state: State,
    /// The Processor's way to the node's realtime end.
    hand: Hand<L::Processor>,
    /// Where the plugin came from, for an in-process one: what a fork loads
    /// again. `None` for a hosting that reopens another way.
    origin: Option<PluginRef>,
    /// The widths the plugin processes audio in, as read before its
    /// activation (for a plugin its shell activated, the width it runs at).
    widths: Widths,
    /// What the plugin runs at, or was last asked to. Its `mode` follows the
    /// live render mode ([`set_render_mode`](Self::set_render_mode)), so
    /// a rate change activates in the mode the plugin was told.
    prepare: Prepare,
    /// What the pump has not yet reported.
    changes: Changes,
    /// What the control side has not yet been told about the plugin, in the
    /// order it happened: a refused rate, a rate taken again after one, a
    /// restart that would not stop, the plugin's death. Read by
    /// [`take_reports`](Self::take_reports).
    reports: Vec<PluginInvalidation>,
    /// The last rate change was refused (the reports say so once).
    rate_refused: bool,
    /// A successor was committed ([`commit`](Self::commit)).
    committed: bool,
    /// Whether the plugin is lost, and why, readable with no lock.
    lost: Arc<Lost>,
    /// The latency and tail as of the last step, for a node's `shape`.
    figures: Arc<Figures>,
    /// The controller map the node translates by: the plugin's declared table
    /// and the app's entries, published to every node over this instance.
    midi_map: MidiMapSlot,
    /// The plugin re-read its declared table (or was rebuilt) since the map
    /// last took it.
    midi_map_stale: bool,
}

impl<L: InProcess> Instance<L> {
    /// Loads and activates the plugin at `sample_rate` and blocks of up to
    /// `block_size`, at the width [`Widths::negotiate`] settles `width` on, in
    /// realtime mode, and lends its Processor to the realtime end this returns.
    /// Main thread.
    ///
    /// # Errors
    ///
    /// The stage that failed and the reason.
    ///
    /// `restore` runs on the plugin **before it is activated**: a plugin may take
    /// its mode from the state at activation, and the activation then is the
    /// state's. Its outcome is returned beside the instance; a refusal does not
    /// fail the load (a plugin at its defaults beats none).
    pub(super) fn load(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
        sample_rate: SampleRate,
        block_size: usize,
        width: FloatWidth,
        restore: Option<&mut dyn FnMut(&mut L) -> Result<(), StateError>>,
    ) -> IpcResult<(Self, Rt<L::Processor>, Option<Result<(), StateError>>)> {
        let mut loaded = L::load_in_process(main, work, plugin).map_err(|e| match e {
            PluginError::Load { stage, reason } => IpcError::LoadFailed {
                path: Some(plugin.path().to_path_buf()),
                stage,
                reason,
            },
            other => IpcError::from(other),
        })?;
        let origin = loaded.origin().clone();
        let restored = restore.map(|restore| restore(&mut loaded));
        let (this, rt) = Self::start(main, loaded, Some(origin), sample_rate, block_size, width)?;
        Ok((this, rt, restored))
    }
}

impl<L: Loaded> Instance<L> {
    /// Activates `loaded` at `sample_rate` and blocks of up to `block_size`,
    /// at the width [`Widths::negotiate`] settles `width` on, in realtime mode,
    /// and lends its Processor to the realtime end this returns. Main thread.
    ///
    /// # Errors
    ///
    /// [`IpcError::BlockUnsupported`] for a block of no frames, and
    /// [`IpcError::ActivationFailed`] with the loader's own failure when the
    /// plugin refuses the activation.
    pub(super) fn start(
        main: MainThreadMarker,
        loaded: L,
        origin: Option<PluginRef>,
        sample_rate: SampleRate,
        block_size: usize,
        width: FloatWidth,
    ) -> IpcResult<(Self, Rt<L::Processor>)> {
        let widths = loaded.widths();
        let wiring = tutti_plugin_types::Control::wiring(&loaded, main);
        let prepare = MaxBlock::new(Samples::new(block_size))
            .map(|block| Prepare::new(sample_rate, block).with_format(widths.negotiate(width)))
            .ok_or(IpcError::BlockUnsupported { frames: block_size })?;
        let (hand, rt) = Hand::threaded();
        let mut this = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<L>::initial_state(),
            hand,
            origin,
            widths,
            prepare,
            changes: Changes::default(),
            reports: Vec::new(),
            rate_refused: false,
            committed: false,
            lost: Arc::default(),
            figures: Arc::default(),
            midi_map: MidiMapSlot::new(&wiring),
            midi_map_stale: false,
        };
        let refused = this.drive(main, Event::Op(HostOp::Activate(prepare)));
        if let Some(trouble) = refused {
            return Err(IpcError::ActivationFailed(Arc::clone(trouble.reason())));
        }
        // What the activation read is the baseline, not a change.
        this.changes = Changes::default();
        this.reports.clear();
        Ok((this, rt))
    }

    /// A plugin its shell activated itself, on any thread, at `prepare`
    /// (`Lifecycle::adopt`): the Processor is lent to the realtime end this
    /// returns. For a hosting whose activation needs no main thread.
    ///
    /// The [`widths`](Self::widths) of an adopted instance are the width it
    /// runs at, not the plugin's capability: an out-of-process plugin's capability is
    /// the server's `Loaded::widths`, which this side is not told.
    pub(super) fn adopt(
        processor: L::Processor,
        controller: L::Controller,
        prepare: Prepare,
        wiring: &tutti_plugin_types::PluginWiring,
    ) -> (Self, Rt<L::Processor>) {
        let (mut hand, rt) = Hand::threaded();
        let (lifecycle, state) =
            Lifecycle::adopt(processor, controller, prepare, wiring, &mut hand);
        let this = Self {
            lifecycle,
            state,
            hand,
            origin: None,
            widths: Widths::new(prepare.format() == FloatWidth::F64),
            prepare,
            changes: Changes::default(),
            reports: Vec::new(),
            rate_refused: false,
            committed: false,
            lost: Arc::default(),
            figures: Arc::default(),
            midi_map: MidiMapSlot::new(wiring),
            midi_map_stale: false,
        };
        this.publish_figures();
        (this, rt)
    }

    /// The widths the plugin processes audio in: read before its activation
    /// when this loaded it, the width it runs at when its shell activated it.
    pub(super) fn widths(&self) -> Widths {
        self.widths
    }

    /// The width the Processor runs at: it stays through a rate or a mode
    /// change, which reactivate with the same [`Prepare`] but for those.
    pub(super) fn width(&self) -> FloatWidth {
        self.prepare.format()
    }

    /// Steps `event` and keeps what it told; what stopped it from taking
    /// effect, if something did.
    ///
    /// A lifecycle that poisons latches the plugin lost, and reports its
    /// death once.
    fn drive(&mut self, main: MainThreadMarker, event: Event) -> Option<Trouble> {
        let mut trouble = None;
        let changes = &mut self.changes;
        let reports = &mut self.reports;
        let midi_map_stale = &mut self.midi_map_stale;
        // The lifecycle takes the state by value and hands the next back; a
        // fresh state is no allocation, so the one held is moved, not cloned.
        let state = std::mem::replace(&mut self.state, Lifecycle::<L>::initial_state());
        self.state =
            self.lifecycle
                .drive(state, event, &mut self.hand, main, |notice| match notice {
                    Notice::LatencyChanged(latency) => changes.latency = Some(latency),
                    Notice::TailChanged(tail) => changes.tail = Some(tail),
                    Notice::IoChanged => changes.io = true,
                    Notice::Transport(request) => changes.transport.push(request),
                    Notice::Refused { rate, reason } => {
                        trouble = Some(Trouble::Refused {
                            rate,
                            reason: Arc::new(reason),
                        });
                    }
                    Notice::Stuck { reason } => {
                        trouble = Some(Trouble::Stuck {
                            reason: Arc::new(reason),
                        });
                    }
                    Notice::StateNotRestored { reason } => {
                        reports.push(PluginInvalidation::StateNotRestored {
                            reason: reason.to_string(),
                        });
                    }
                    Notice::Lost { reason } => {
                        trouble = Some(Trouble::Lost {
                            reason: Arc::new(reason),
                        });
                    }
                    Notice::MidiMapChanged | Notice::Reloaded => *midi_map_stale = true,
                    Notice::ParamValuesChanged
                    | Notice::NoteExpressionChanged
                    | Notice::ParamTitlesChanged => {}
                });
        self.publish_figures();
        if std::mem::take(&mut self.midi_map_stale) {
            self.declare_midi_map(main);
            self.reports.push(PluginInvalidation::MidiMapChanged);
        }
        if self.lifecycle.is_poisoned() {
            let source = trouble.as_ref().map_or_else(
                || {
                    Arc::new(PluginError::Other(
                        "a call into the plugin panicked".to_string(),
                    ))
                },
                |trouble| Arc::clone(trouble.reason()),
            );
            let cause = PluginFailure::Panicked { source };
            if self.lost.latch(cause.clone()) {
                self.reports.push(PluginInvalidation::Crashed { cause });
            }
        }
        trouble
    }

    /// Publishes the latency and tail the state holds.
    fn publish_figures(&self) {
        let (latency, tail) = self.figures();
        self.figures.publish(latency, tail);
    }

    /// The plugin's `Control` surface (parameters, state, presets), wherever
    /// it is in its life. `None` once it is lost.
    pub(super) fn control(&mut self) -> Option<&mut dyn tutti_plugin_types::Control> {
        self.lifecycle.control()
    }

    /// The controller map the nodes over this instance translate by.
    pub(super) fn midi_map(&self) -> MidiMapSlot {
        self.midi_map.clone()
    }

    /// Takes the table the plugin declares now. Main thread.
    fn declare_midi_map(&mut self, main: MainThreadMarker) {
        if let Some(control) = self.lifecycle.control() {
            self.midi_map.declare(&control.wiring(main));
        }
    }

    /// The latency and tail as the state last read them.
    fn figures(&self) -> (Samples, PluginTail) {
        (
            self.state.latency().unwrap_or_default(),
            self.state.tail().unwrap_or_default(),
        )
    }

    /// The cell the latency and tail are published in, shared with the nodes
    /// over this instance: their `shape` reads it with the instance locked.
    pub(super) fn figure_cell(&self) -> Arc<Figures> {
        Arc::clone(&self.figures)
    }

    /// The ports the plugin last reported: what a node rebuilt over it
    /// declares. `None` before the lifecycle has read a layout, and once the
    /// plugin is lost.
    pub(super) fn layout(&self) -> Option<&Layout> {
        if self.lifecycle.is_poisoned() {
            return None;
        }
        self.state.layout()
    }

    /// Whether the plugin can take a successor realtime end now.
    ///
    /// # Errors
    ///
    /// [`RebindError::NotRunning`] for a plugin that is not active (loaded,
    /// refused or lost): there is no Processor to move.
    /// [`RebindError::Restarting`] while the plugin's own restart or a request
    /// still to be read is under way: the layout the lifecycle holds is not the
    /// one the plugin will run with, so the caller pumps and tries again.
    pub(super) fn ready_to_rehand(&self) -> Result<(), RebindError> {
        if !self.state.renders() {
            return Err(RebindError::NotRunning);
        }
        if !self.state.is_quiet() || self.state.is_parked() {
            return Err(RebindError::Restarting);
        }
        Ok(())
    }

    /// Names `successor` as the Processor's way and brings the Processor back to
    /// be lent to it: a [`HostOp::Relend`], so the plugin keeps running and is
    /// neither deactivated nor activated ([`Hand::install`]). Main thread.
    ///
    /// It does not fail: the node it serves is in the graph already and the
    /// node it replaces is on its way out, so whatever the plugin's state is
    /// now, the successor is where its Processor goes next.
    ///
    /// - An active plugin with nothing under way plans a recall (the op), which
    ///   raises the recall on the end that holds the Processor; that node hands
    ///   it back at its next poll or when it is dropped, and the pump's tick
    ///   lends it to the successor.
    /// - A plugin with a restart under way (its own, or a rate change's) has a
    ///   recall up or its Processor held; the plan ends in a lend, which goes to
    ///   the successor.
    /// - A plugin that is not active has no Processor lent; its next activation
    ///   lends to the successor.
    ///
    /// Until the Processor arrives the successor's end is empty and a node over it
    /// renders silence; [`settlement`](Self::settlement) says when the way is
    /// the successor's. A plugin that refuses the restart renders nothing until
    /// one it takes: the refusal is kept for the control side, as a rate's is
    /// ([`take_reports`](Self::take_reports)).
    pub(super) fn commit(&mut self, main: MainThreadMarker, successor: Successor<L::Processor>) {
        self.hand.install(successor);
        if let Some(trouble) = self.drive(main, Event::Op(HostOp::Relend)) {
            self.note(trouble);
        }
        self.committed = true;
    }

    /// Where the Processor's way is, after a [`commit`](Self::commit).
    pub(super) fn settlement(&self) -> Settlement {
        if self.lifecycle.is_poisoned() {
            Settlement::Lost
        } else if self.hand.pending() {
            Settlement::Pending
        } else if self.committed {
            Settlement::Moved
        } else {
            Settlement::Idle
        }
    }

    /// The latch set when the lifecycle poisons, shared with the nodes over
    /// this instance and the handle's health.
    pub(super) fn lost_latch(&self) -> Arc<Lost> {
        Arc::clone(&self.lost)
    }

    /// Reports that `mode` was not applied because the node was prepared off
    /// the main thread ([`PluginInvalidation::RenderModeNotApplied`]).
    pub(super) fn note_mode_not_applied(&mut self, mode: RenderMode) {
        self.reports
            .push(PluginInvalidation::RenderModeNotApplied { mode });
    }

    /// The render mode the plugin runs in, or was last told.
    pub(super) fn render_mode(&self) -> RenderMode {
        self.prepare.mode()
    }

    /// Tells the plugin its render mode, and keeps the mode a rate change
    /// activates it in. Main thread.
    ///
    /// What happens is the plugin's
    /// [`render_mode_support`](tutti_plugin_types::Control::render_mode_support):
    /// a `Live` plugin is told; an `OnActivation` one is reconfigured through
    /// the lifecycle, the activation carrying the mode and the Processor
    /// changing at the pump, as a rate change's does; an `Unsupported` one is
    /// left running, the mode kept for its next activation. Returns whether
    /// the plugin now runs in `mode` (or is on its way to it).
    ///
    /// With `rt`, the node's own realtime end out of any executor (a fork
    /// being built), a reconfiguration runs here to the end, as
    /// [`set_sample_rate`](Self::set_sample_rate)'s does: the Processor is
    /// lent again on return, so the node's first block has one. Without, the
    /// recall waits for the audio thread's next poll and a later
    /// [`pump`](Self::pump) finishes it.
    pub(super) fn set_render_mode(
        &mut self,
        main: MainThreadMarker,
        mode: RenderMode,
        rt: Option<&mut Rt<L::Processor>>,
    ) -> bool {
        self.prepare = self.prepare.with_mode(mode);
        self.state.set_mode(mode);
        if let Some(control) = self.lifecycle.control() {
            match control.render_mode_support(main) {
                RenderModeSupport::Live => return control.set_render_mode(main, mode),
                RenderModeSupport::Unsupported => return false,
                RenderModeSupport::OnActivation => {}
            }
        }
        if self.lifecycle.is_poisoned() {
            return false;
        }
        let mut refused = self.drive(main, Event::Op(HostOp::Reconfigure(self.prepare)));
        if let Some(rt) = rt {
            // The recall raised above: the realtime end hands the Processor
            // back, the tick runs the parked plan, and the reactivated
            // Processor is lent and taken.
            let _ = rt.poll(Processor::stop);
            refused = refused.or(self.drive(main, Event::Tick));
            let _ = rt.poll(Processor::stop);
        }
        match refused {
            None => {
                self.settle();
                true
            }
            Some(trouble) => {
                self.note(trouble);
                false
            }
        }
    }

    /// Tells the plugin its rate: a recall, a rejoin and an activation at the
    /// new [`Prepare`], which is how VST2 takes one (`effSetSampleRate`
    /// between `effMainsChanged` brackets). Main thread.
    ///
    /// With `rt`, the node's own realtime end out of any executor (its
    /// `prepare`), the whole exchange runs here and the new Processor is in
    /// `rt` on return. Without, the recall waits for the audio thread's next
    /// poll and a later [`pump`](Self::pump) finishes it.
    ///
    /// A plugin that refuses the rate renders nothing until a rate it takes, as every format does.
    /// The figures it recomputes are read after the activation.
    ///
    /// # Errors
    ///
    /// Why the plugin did not take the rate, worded as the other formats word
    /// it (`standing`: where the plugin stands). What went wrong is also kept
    /// for [`take_reports`](Self::take_reports), by its kind: a refusal of the
    /// rate, a restart that would not stop, or the plugin's death. That is how
    /// the control side hears of one the audio thread's pending rate caused.
    ///
    /// A rate that is taken after a refusal is reported taken
    /// (`PluginInvalidation::RateRestored`) when the plugin renders at it: at
    /// once with `rt`, and at the pump that finishes the parked plan without.
    pub(super) fn set_sample_rate(
        &mut self,
        main: MainThreadMarker,
        rate: SampleRate,
        rt: Option<&mut Rt<L::Processor>>,
    ) -> Result<(), String> {
        let wanted = self.prepare.with_rate(rate);
        let mut outcome = self.drive(main, Event::Op(HostOp::Reconfigure(wanted)));
        if let Some(rt) = rt {
            // The recall raised above: the realtime end hands the Processor
            // back, the tick runs the parked plan, and the new Processor is
            // lent and taken.
            let _ = rt.poll(Processor::stop);
            let ticked = self.drive(main, Event::Tick);
            outcome = outcome.or(ticked);
            let _ = rt.poll(Processor::stop);
        }
        match outcome {
            Some(trouble) => {
                // A plugin that died on the way is lost, whatever its last
                // reply said: the caller hears what the reports say.
                let trouble = match trouble {
                    Trouble::Refused { reason, .. } | Trouble::Stuck { reason }
                        if self.lost.is_lost() =>
                    {
                        Trouble::Lost { reason }
                    }
                    trouble => trouble,
                };
                let reason = self.worded(rate, &trouble);
                self.note(trouble);
                Err(reason)
            }
            None => {
                self.prepare = wanted;
                self.settle();
                Ok(())
            }
        }
    }

    /// Why the plugin did not run at `rate`, as the formats word a refusal.
    fn worded(&self, rate: SampleRate, trouble: &Trouble) -> String {
        match trouble {
            Trouble::Refused { reason, .. } => format!(
                "plugin refused sample rate {} ({}): {reason}",
                rate.get(),
                standing(self.state.renders(), self.prepare.rate())
            ),
            Trouble::Stuck { reason } => format!(
                "plugin would not stop for sample rate {} ({}): {reason}",
                rate.get(),
                standing(self.state.renders(), self.prepare.rate())
            ),
            Trouble::Lost { reason } => format!(
                "plugin was lost changing to sample rate {}: {reason}",
                rate.get()
            ),
        }
    }

    /// What the plugin did not do, kept for the control side by its kind.
    /// A death is reported by [`drive`](Self::drive), where the latch is set.
    fn note(&mut self, trouble: Trouble) {
        // A lost plugin is reported as lost, once, by `drive`: what it
        // refused on the way is not a refusal of a rate.
        if self.lost.is_lost() {
            return;
        }
        match trouble {
            Trouble::Refused { rate, reason } => {
                self.rate_refused = true;
                self.reports
                    .push(PluginInvalidation::RateRefused(RateRefusal::new(
                        rate,
                        self.refusal_cause(&reason),
                    )));
            }
            Trouble::Stuck { reason } => {
                self.reports.push(PluginInvalidation::Stuck {
                    reason: reason.to_string(),
                });
            }
            Trouble::Lost { .. } => {}
        }
    }

    /// What a refused rate left the plugin as: a refusal a served plugin
    /// already typed keeps its cause, and a loader's own failure is the plugin
    /// running at the rate it had when it renders still, deactivated when it
    /// does not.
    fn refusal_cause(&self, reason: &PluginError) -> RateRefusalCause {
        if let PluginError::Bridge(bridge) = reason
            && let IpcError::RateRefused(refusal) = &**bridge
        {
            return refusal.cause.clone();
        }
        let reason = reason.to_string();
        if self.state.renders() {
            RateRefusalCause::RolledBack {
                running: self.prepare.rate(),
                reason,
            }
        } else {
            RateRefusalCause::Deactivated { reason }
        }
    }

    /// The plugin runs again after refusing a rate: reported once.
    fn restored(&mut self) {
        if self.rate_refused {
            self.rate_refused = false;
            self.reports.push(PluginInvalidation::RateRestored);
        }
    }

    /// What the control side has not yet been told about the plugin, once: a
    /// refused rate, the rate taken again after one, a restart that would not
    /// stop, the plugin's death. [`pump`](Self::pump) returns them with the
    /// plugin's changes.
    fn take_reports(&mut self) -> Vec<PluginInvalidation> {
        std::mem::take(&mut self.reports)
    }

    /// The main thread's tick: delivers what the plugin asked for, collects a
    /// Processor the audio thread returned (running a pending rate change), and
    /// reports what the plugin changed about itself since the last call.
    ///
    /// Re-reads on `audioMasterIOChanged` (the plugin's own signal: its pin
    /// counts and latency may both have moved), and reports what an activation
    /// read. Main thread: a re-read dispatches `effGetTailSize`.
    pub(super) fn pump(&mut self, main: MainThreadMarker) -> Changes {
        self.midi_map.reclaim();
        let mut asked = Vec::new();
        if let Some(control) = self.lifecycle.control() {
            control.take_requests(main, &mut asked);
        }
        // The plugin's own signal, reported as it was raised: its pin counts or
        // latency may have moved, and the handle's `Io` says so even when the
        // re-read finds the same layout (the core reports a layout change
        // only).
        if asked.contains(&tutti_plugin_types::Request::IoChanged) {
            self.changes.io = true;
        }
        for request in asked {
            if let Some(trouble) = self.drive(main, Event::Request(request)) {
                self.note(trouble);
            }
        }
        match self.drive(main, Event::Tick) {
            Some(trouble) => self.note(trouble),
            None => self.settle(),
        }
        let mut changes = std::mem::take(&mut self.changes);
        changes.reports = self.take_reports();
        changes
    }

    /// A plugin that refused a rate has taken one when it renders again with
    /// no plan under way: a parked change that was waiting for the audio
    /// thread's Processor has finished, or the plugin's own restart ran.
    fn settle(&mut self) {
        if self.rate_refused && self.state.renders() && !self.state.is_busy() {
            self.restored();
        }
    }

    /// The plugin's parameter edits since the last call.
    pub(super) fn drain_param_changes(&mut self, main: MainThreadMarker) -> Vec<ParamEdit> {
        let mut edits = Vec::new();
        if let Some(control) = self.lifecycle.control() {
            control.take_reported_edits(main, &mut edits);
        }
        edits
    }

    /// Whether the plugin is split and rendering.
    #[cfg(test)]
    pub(super) fn renders(&self) -> bool {
        self.state.renders()
    }

    /// The rate the plugin runs at, or was last asked to.
    #[cfg(test)]
    pub(super) fn rate(&self) -> SampleRate {
        self.prepare.rate()
    }

    /// Where the plugin came from, for an in-process one: what a fork loads
    /// again.
    pub(super) fn origin(&self) -> Option<&PluginRef> {
        self.origin.as_ref()
    }

    /// The rate a Processor that follows the graph (`Processor::retune`) took
    /// live: the activation a later reconfiguration starts from is at it, and
    /// a refusal of the rate before it is over.
    pub(super) fn retuned(&mut self, rate: SampleRate) {
        self.prepare = self.prepare.with_rate(rate);
        self.restored();
    }
}

#[cfg(test)]
mod tests {
    use tutti_fixtures::main_thread;
    use tutti_vst2_host::Vst2Loaded;

    use super::*;

    /// **A load with no block is refused** as an unsupported block (a rate is a
    /// `SampleRate`, so it cannot be none).
    #[test]
    fn a_load_with_no_block_is_refused() {
        match Instance::<Vst2Loaded>::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            &tutti_plugin_types::PluginRef::file(tutti_fixtures::vst2_probe()),
            SampleRate::SR_48K,
            0,
            FloatWidth::F32,
            None,
        ) {
            Err(IpcError::BlockUnsupported { frames: 0 }) => {}
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a block of no frames must be refused"),
        }
    }

    /// **A plugin that refuses its first activation fails the load with the
    /// loader's own error**, not its text.
    #[test]
    fn a_refused_first_activation_carries_the_loaders_error() {
        let (fake, loaded) = tutti_plugin_types::fake::FakePlugin::new();
        fake.refuse_rate(Some(SampleRate::SR_48K));
        let refused = Instance::start(
            main_thread(),
            loaded,
            None,
            SampleRate::SR_48K,
            64,
            FloatWidth::F32,
        );
        match refused {
            Err(IpcError::ActivationFailed(cause)) => {
                assert!(matches!(*cause, PluginError::Other(_)), "{cause:?}");
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("the fake refuses 48 kHz"),
        }
    }

    /// **A plugin that processes 32-bit audio only, started for a 64-bit graph,
    /// is activated at 32-bit**: [`Widths::negotiate`] lowers the request, the
    /// instance reports the plugin's widths as `Widths::F32` and the width it
    /// runs at as 32-bit, and the plugin was sent a 32-bit activation. The load
    /// then returns it in a `WidthError` (`load_client`) rather than hosting it.
    #[test]
    fn a_32_bit_plugin_started_for_64_bit_runs_at_32() {
        let (fake, loaded) = tutti_plugin_types::fake::FakePlugin::new();
        fake.only_32_bit();
        let (instance, _rt) = Instance::start(
            main_thread(),
            loaded,
            None,
            SampleRate::SR_48K,
            64,
            FloatWidth::F64,
        )
        .expect("the plugin activates at 32-bit");
        assert_eq!(instance.widths(), Widths::F32);
        assert_eq!(instance.width(), FloatWidth::F32);
        assert_eq!(fake.activated_width(), Some(FloatWidth::F32));
        assert!(instance.renders());
    }

    /// **A plugin that processes both widths, started for 64-bit, is activated
    /// at 64-bit.**
    #[test]
    fn a_64_bit_plugin_started_for_64_bit_runs_at_64() {
        let (fake, loaded) = tutti_plugin_types::fake::FakePlugin::new();
        let (instance, _rt) = Instance::start(
            main_thread(),
            loaded,
            None,
            SampleRate::SR_48K,
            64,
            FloatWidth::F64,
        )
        .expect("the plugin activates at 64-bit");
        assert_eq!(instance.widths(), Widths::BOTH);
        assert_eq!(instance.width(), FloatWidth::F64);
        assert_eq!(fake.activated_width(), Some(FloatWidth::F64));
    }

    /// **A rate the plugin takes is the instance's new configuration, and a
    /// plain change reports nothing** (`PluginInvalidation::RateRefused` and
    /// `RateRestored` are for a refusal and the recovery from one).
    #[test]
    fn a_rate_the_plugin_takes_reports_nothing() {
        let (mut instance, _rt, _) = Instance::<Vst2Loaded>::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            &tutti_plugin_types::PluginRef::file(tutti_fixtures::vst2_probe()),
            SampleRate::SR_48K,
            64,
            FloatWidth::F32,
            None,
        )
        .expect("loads");
        assert!(instance.take_reports().is_empty(), "nothing to report yet");
        instance
            .set_sample_rate(main_thread(), SampleRate::SR_96K, None)
            .expect("a real rate is taken");
        assert_eq!(instance.prepare.rate(), SampleRate::SR_96K);
        assert!(instance.take_reports().is_empty(), "a plain change");
        assert!(instance.renders());
        assert!(!instance.lost_latch().is_lost());
    }

    /// **A rate change made off the realtime thread completes at the pump,
    /// once the audio thread hands the Processor back.** The change recalls
    /// it; the realtime end, at its next poll, stops it and returns it; the
    /// pump runs the parked plan and lends the new Processor, which the next
    /// poll takes.
    #[test]
    fn a_parked_rate_change_completes_at_the_pump() {
        let main = main_thread();
        let (mut instance, mut rt, _) = Instance::<Vst2Loaded>::load(
            main,
            &tutti_fixtures::detached_work(),
            &tutti_plugin_types::PluginRef::file(tutti_fixtures::vst2_probe()),
            SampleRate::SR_48K,
            64,
            FloatWidth::F32,
            None,
        )
        .expect("loads");
        assert!(rt.poll(Processor::stop).is_some(), "lent at the load");
        instance
            .set_sample_rate(main, SampleRate::SR_96K, None)
            .expect("the change starts");
        assert!(rt.poll(Processor::stop).is_none(), "handed back");
        assert!(instance.state.is_parked(), "waits for the Processor");
        let _ = instance.pump(main);
        assert!(instance.renders());
        assert_eq!(instance.rate(), SampleRate::SR_96K);
        assert!(rt.poll(Processor::stop).is_some(), "the new Processor");
    }

    /// An active fake taking the render mode as `support` says, adopted with
    /// its realtime end, its log cleared.
    fn adopted(
        support: RenderModeSupport,
    ) -> (
        Instance<tutti_plugin_types::fake::FakeLoaded>,
        Rt<tutti_plugin_types::fake::FakeProcessor>,
        tutti_plugin_types::fake::FakePlugin,
    ) {
        use tutti_plugin_types::{Control, Loaded};
        let (fake, loaded) = tutti_plugin_types::fake::FakePlugin::new();
        fake.take_render_mode(support);
        let main = main_thread();
        let wiring = loaded.wiring(main);
        let prepare = Prepare::new(
            SampleRate::SR_48K,
            tutti_buffers::MaxBlock::new(Samples::new(64)).expect("a block"),
        );
        let Ok((processor, controller)) = loaded.activate(main, prepare) else {
            panic!("the fake activates");
        };
        let (instance, rt) = Instance::adopt(processor, controller, prepare, &wiring);
        fake.clear_calls();
        (instance, rt, fake)
    }

    /// Whether the fake was deactivated or activated since its log was cleared.
    fn cycled(fake: &tutti_plugin_types::fake::FakePlugin) -> bool {
        use tutti_plugin_types::fake::Call;
        fake.calls()
            .iter()
            .any(|c| matches!(c, Call::Deactivate | Call::Activate(..)))
    }

    /// **A plugin that reads the mode at activation is activated again in
    /// it**: the exchange, run to the end with the realtime end in hand,
    /// deactivates and activates the plugin, offline.
    #[test]
    fn a_mode_read_at_activation_cycles_the_plugin() {
        let (mut instance, mut rt, fake) = adopted(RenderModeSupport::OnActivation);
        assert!(instance.set_render_mode(main_thread(), RenderMode::Offline, Some(&mut rt)));
        assert!(cycled(&fake), "{:?}", fake.calls());
        assert_eq!(instance.render_mode(), RenderMode::Offline);
        assert!(instance.renders());
    }

    /// **A plugin with no render mode is left running**: the change answers
    /// `false`, the plugin is neither told nor deactivated, and the mode is
    /// kept for its next activation.
    #[test]
    fn a_plugin_without_a_render_mode_is_not_cycled() {
        let (mut instance, mut rt, fake) = adopted(RenderModeSupport::Unsupported);
        assert!(!instance.set_render_mode(main_thread(), RenderMode::Offline, Some(&mut rt)));
        assert_eq!(fake.told_render_mode(), None);
        assert!(!cycled(&fake), "{:?}", fake.calls());
        assert_eq!(instance.render_mode(), RenderMode::Offline, "kept");
        assert!(instance.renders());
    }

    /// **A live plugin is told the mode where it runs**, and is neither
    /// deactivated nor activated again.
    #[test]
    fn a_live_plugin_is_told_and_not_cycled() {
        let (mut instance, mut rt, fake) = adopted(RenderModeSupport::Live);
        assert!(instance.set_render_mode(main_thread(), RenderMode::Offline, Some(&mut rt)));
        assert_eq!(fake.told_render_mode(), Some(RenderMode::Offline));
        assert!(!cycled(&fake), "{:?}", fake.calls());
    }

    /// **A panic out of a call into the plugin latches it lost, with the
    /// reason, and reports its death once**: the cause is a
    /// `PluginFailure::Panicked`, readable with no lock through the latch, and
    /// the next pump has nothing more to say.
    #[test]
    fn a_panicked_plugin_is_latched_lost_with_a_panicked_cause_reported_once() {
        use tutti_plugin_types::Request;
        let (mut instance, _rt, fake) = adopted(RenderModeSupport::Live);
        let lost = instance.lost_latch();
        assert!(!lost.is_lost(), "a live plugin");
        fake.panic_in_latency(true);
        assert!(Request::LatencyChanged.raise(fake.request_flags()));
        let changes = instance.pump(main_thread());
        assert!(lost.is_lost());
        assert!(
            matches!(lost.cause(), Some(PluginFailure::Panicked { .. })),
            "{:?}",
            lost.cause()
        );
        assert!(
            matches!(
                changes.reports.as_slice(),
                [PluginInvalidation::Crashed { .. }]
            ),
            "{:?}",
            changes.reports
        );
        assert!(instance.pump(main_thread()).reports.is_empty(), "told once");
    }

    /// **A restart the plugin would not stop for is reported as stuck, not as a
    /// refused rate**: the plugin runs as it was, so nothing was refused and
    /// nothing is to be restored.
    #[test]
    fn a_restart_that_would_not_stop_is_stuck_not_a_refused_rate() {
        let (mut instance, mut rt, fake) = adopted(RenderModeSupport::OnActivation);
        fake.stick_deactivations(1);
        let why = instance
            .set_sample_rate(main_thread(), SampleRate::SR_96K, Some(&mut rt))
            .expect_err("the fake will not stop");
        assert!(why.contains("would not stop"), "{why}");
        let reports = instance.take_reports();
        assert!(
            matches!(reports.as_slice(), [PluginInvalidation::Stuck { .. }]),
            "{reports:?}"
        );
        assert!(!instance.rate_refused);
        assert!(instance.renders(), "as it was");
    }

    /// **A refused rate is reported with the rate that was refused, and the
    /// recovery is reported once, when the plugin renders again** (here by its
    /// activation for a render mode, not by a rate change that would have
    /// reported it).
    #[test]
    fn a_refusal_names_the_rate_and_the_recovery_is_reported_when_it_renders_again() {
        let (mut instance, mut rt, fake) = adopted(RenderModeSupport::OnActivation);
        fake.refuse_rate(Some(SampleRate::SR_96K));
        instance
            .set_sample_rate(main_thread(), SampleRate::SR_96K, Some(&mut rt))
            .expect_err("refused");
        let reports = instance.take_reports();
        assert!(
            matches!(
                reports.as_slice(),
                [PluginInvalidation::RateRefused(refusal)] if refusal.rate == SampleRate::SR_96K
                    && matches!(refusal.cause, RateRefusalCause::Deactivated { .. })
            ),
            "{reports:?}"
        );
        assert!(!instance.renders());

        fake.refuse_rate(None);
        assert!(instance.set_render_mode(main_thread(), RenderMode::Offline, Some(&mut rt)));
        assert!(instance.renders(), "the activation for the mode ran");
        let reports = instance.take_reports();
        assert!(
            matches!(reports.as_slice(), [PluginInvalidation::RateRestored]),
            "{reports:?}"
        );
        assert!(instance.pump(main_thread()).reports.is_empty(), "told once");
    }

    /// **A plugin that panics in the middle of a restart is reported lost and
    /// nothing else**: the refusal its failed activation raises is not a
    /// refused rate.
    #[test]
    fn a_panic_during_a_restart_reports_the_death_and_no_refusal() {
        let (mut instance, mut rt, fake) = adopted(RenderModeSupport::OnActivation);
        fake.panic_in_activate(true);
        let why = instance
            .set_sample_rate(main_thread(), SampleRate::SR_96K, Some(&mut rt))
            .expect_err("the plugin is lost");
        assert!(
            why.starts_with("plugin was lost changing to sample rate 96000"),
            "{why}"
        );
        let reports = instance.take_reports();
        assert!(
            matches!(reports.as_slice(), [PluginInvalidation::Crashed { .. }]),
            "{reports:?}"
        );
        assert!(instance.lost_latch().is_lost());
    }
}
