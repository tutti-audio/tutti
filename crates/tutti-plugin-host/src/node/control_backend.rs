//! An in-process plugin of any [`InProcess`] format, behind a handle: its
//! [`AnyThread`] (the hop to the main thread), its [`Health`] and the
//! [`MainThreadControl`] a handle's token-taking calls reach it by.
//!
//! All three are over the plugin's [`Control`]
//! surface ([`Lifecycle::control`](tutti_plugin_types::Lifecycle::control),
//! which reaches it whether the plugin is loaded or active). Holds the plugin's
//! control side, the `Arc<Mutex<Instance<L>>>` the node's `shape` and
//! `prepare` also take. Control-thread calls take the lock for the duration of
//! one plugin operation — short for parameter / state methods, potentially long
//! for editor ones. The audio thread never takes it: it renders through the
//! Processor lent to the node, so a slow `editor_idle` cannot underrun audio.
//!
//! # Threads
//!
//! Every [`Control`] entry is a main-thread call
//! that takes a [`MainThreadMarker`]. [`MainThreadControl`] is lent to a caller that
//! holds one; [`AnyThread`] takes none, because an out-of-process plugin answers those
//! calls from any thread and the session calls them from its executor tasks. Here they
//! work from any thread too, through `on_main`: on the main thread the call runs
//! at once; off it the
//! work is posted to the owner's [`MainWork`](tutti_plugin_types::MainWork) and the
//! caller waits for the answer, which costs one main-thread tick (`Session::update`
//! runs the session's `MainWork` every tick). The wait has
//! a deadline ([`OFF_MAIN_DEADLINE`]): the host's main thread must keep
//! pumping, and a host that blocks its main thread on such a call (joining the
//! task from the main thread) gets the trait's "unavailable" answer (`None`,
//! `Err`, `false`) when it expires. A timed-out call's work still runs at the
//! next tick. Fire-and-forget writes post and do not wait.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_types::{MainThreadMarker, MainWorkHandle, StateUse};
use tutti_rt::PendingSampleRate;

use parking_lot::Mutex;

use super::instance::{Changes, Instance, Lost};
use super::off_main_wait::{OffMainWait, await_answer};
use crate::MainThreadControl;
use crate::ParameterChangeSink;
use tutti_plugin_types::{
    AnyThread, AutomationMode, AutomationModeError, BypassWrite, Control, Health, InProcess,
    OwnBypass, StateError, UnknownParam, bypass_value, is_bypassed,
};
use tutti_plugin_types::{ParameterInfo, PluginFailure, RenderMode, TerminationReason};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// Bundles the shared Mutex with the parameter-change sink so editor
/// idle calls can drain plugin-internal automation events and notify
/// the user-installed callback.
pub(crate) struct ControlBackend<L: InProcess> {
    pub(super) inner: Arc<Mutex<Instance<L>>>,
    pub(crate) param_sink: ParameterChangeSink,
    /// The cell the audio unit parks a rate change in, shared with the
    /// node. Drained here because telling a plugin its rate deactivates and
    /// activates it, which allocates.
    pub(crate) pending_sample_rate: Arc<PendingSampleRate>,
    /// The host's parameter writes, which the node drains at the head of its
    /// next block (`host_edits`).
    pub(super) edits: super::host_edits::EditPost,
    /// Where what a pump finds is delivered: a re-read that found the
    /// plugin's latency, tail or pins moved, what it asked of the transport
    /// (`PluginHandle::subscribe`).
    pub(crate) reporting: Reporting,
    /// Whether the plugin is lost, and why: what the handle's health reads.
    pub(crate) lost: Arc<Lost>,
    /// The plugin's format name, for the refusal of an advisory the plugin
    /// does not take.
    pub(crate) format: &'static str,
    /// The collector of the plugin's owner: where an off-main call posts its
    /// work for the main thread's next tick.
    pub(crate) work: MainWorkHandle,
}

/// The listeners what a pump finds is delivered to: the invalidations
/// ([`InvalidateSink`](crate::InvalidateSink)) and the plugin's requests of the
/// transport ([`TransportSink`](crate::TransportSink)).
#[derive(Clone, Debug)]
pub(crate) struct Reporting {
    pub(crate) invalidate: crate::InvalidateSink,
    pub(crate) transport: crate::TransportSink,
}

impl Reporting {
    /// Delivers what a pump found, in the order the plugin did it: what the
    /// instance reported of itself (a refused rate, a death), the figures an
    /// activation re-read, the pins, then the transport requests. Never under
    /// the instance's lock: a callback may call back.
    pub(super) fn deliver(&self, changes: Changes) {
        use tutti_plugin_types::PluginInvalidation;
        let Changes {
            latency,
            tail,
            io,
            transport,
            reports,
        } = changes;
        for report in reports {
            self.invalidate.fire(report);
        }
        if let Some(samples) = latency {
            self.invalidate
                .fire(PluginInvalidation::Latency { samples });
        }
        if let Some(tail) = tail {
            self.invalidate.fire(PluginInvalidation::Tail { tail });
        }
        if io {
            self.invalidate.fire(PluginInvalidation::Io);
        }
        for request in transport {
            self.transport.fire(request);
        }
    }
}

/// How long an off-main-thread call waits for the main thread to run its work
/// (see the module docs, "Threads"). A collector that has never run
/// ([`MainWorkHandle::is_pumped`]) is not waited for at all: the call answers
/// "unavailable" at once, as a call with no main thread does, and a post is
/// refused rather than queued without bound. The decision is [`OffMainWait`]'s.
const OFF_MAIN_DEADLINE: Duration = Duration::from_secs(2);

impl<L> ControlBackend<L>
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    /// Runs `work` on the plugin on the main thread, under the instance lock,
    /// from whichever thread asks. `None` when it cannot be answered: the
    /// collector has never run, or the main thread did not run the work within
    /// [`OFF_MAIN_DEADLINE`].
    fn on_main<R: Send + 'static>(
        &self,
        work: impl FnOnce(MainThreadMarker, &mut Instance<L>) -> R + Send + 'static,
    ) -> Option<R> {
        if let Some(main) = MainThreadMarker::new() {
            return Some(self.flushed(main, work));
        }
        if !self.work.is_pumped() {
            return None;
        }
        let (answer, wait) = sync_channel(1);
        let inner = Arc::clone(&self.inner);
        let post = self.edits.clone();
        self.work.defer(move |main| {
            let _ = answer.send(flushed(&post, main, &mut inner.lock(), work));
        });
        await_answer(
            &wait,
            OffMainWait::new(OFF_MAIN_DEADLINE, Instant::now()),
            Instant::now,
        )
    }

    /// Queues the main-thread job that tells the plugin's edit controller of
    /// the host's writes, unless one is queued: the controller hears each
    /// parameter's newest value when the main thread runs, the way the
    /// Processor heard it through a block. A collector that has never run has no
    /// main thread to tell it: the writes wait for a read, a save or a pump.
    fn inform_controller_later(&self) {
        if !self.edits.claim_job() {
            return;
        }
        if !self.work.is_pumped() {
            self.edits.release_job();
            return;
        }
        let inner = Arc::clone(&self.inner);
        queue_inform(
            self.work.clone(),
            self.edits.clone(),
            Arc::new(move |main, told| {
                let Some(mut instance) = inner.try_lock() else {
                    return false;
                };
                if let Some(control) = instance.control() {
                    for &(id, value) in told {
                        control.inform_controller(main, id, value);
                    }
                }
                true
            }),
        );
    }

    /// `work` on the plugin under the instance's lock, after the host's
    /// pending parameter writes are applied.
    fn flushed<R>(
        &self,
        main: MainThreadMarker,
        work: impl FnOnce(MainThreadMarker, &mut Instance<L>) -> R,
    ) -> R {
        flushed(&self.edits, main, &mut self.inner.lock(), work)
    }

    /// `work` on the plugin now, when this is the main thread and the instance is
    /// not locked; `None` otherwise. Never waits.
    fn try_main<R>(&self, work: impl FnOnce(MainThreadMarker, &mut Instance<L>) -> R) -> Option<R> {
        let main = MainThreadMarker::new()?;
        let mut instance = self.inner.try_lock()?;
        Some(flushed(&self.edits, main, &mut instance, work))
    }
}

/// Tells the plugin's edit controller of the host's writes on the main thread's
/// next tick, through `tell`, which answers whether it could (`false`: the
/// instance is locked, as it is while the main thread is inside a call of its
/// own). A job whose `tell` could not puts what it took back and queues another,
/// so no write waits on a lock for good.
pub(super) fn queue_inform(
    work: MainWorkHandle,
    post: super::host_edits::EditPost,
    tell: Arc<dyn Fn(MainThreadMarker, &[(ParamId, Normalized)]) -> bool + Send + Sync>,
) {
    let again = work.clone();
    work.defer(move |main| {
        let told = post.take_inform();
        if told.is_empty() {
            return;
        }
        if !tell(main, &told) {
            post.give_back(told);
        }
        // Written since the take, or not told: another job is owed.
        if post.claim_job() {
            queue_inform(again, post, tell);
        }
    });
}

/// `work` on `instance`, after the writes no block has taken are applied to the
/// plugin: a read or a save made through the backend sees every write made
/// before it.
fn flushed<L: InProcess, R>(
    post: &super::host_edits::EditPost,
    main: MainThreadMarker,
    instance: &mut Instance<L>,
    work: impl FnOnce(MainThreadMarker, &mut Instance<L>) -> R,
) -> R {
    if let Some(control) = instance.control() {
        post.flush(main, control);
    }
    work(main, instance)
}

impl<L> AnyThread for ControlBackend<L>
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    /// `None` for a lost plugin, and when the main thread does not answer.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        self.on_main(|main, instance| Some(instance.control()?.parameters(main)))?
    }

    /// `None` for a lost plugin, for a plugin that exposes no value to read,
    /// for an id it does not have, and when the main thread does not answer.
    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        self.on_main(move |main, instance| instance.control()?.parameter(main, id))?
    }

    /// Stored in the parameter's slot, from any thread, with no instance lock
    /// and no hop to the main thread: the node applies it at the head of its
    /// next block (`host_edits`). A write is never skipped for a locked
    /// instance, and a newer one replaces one no block has taken.
    ///
    /// The slot holds a `Normalized`, so no out-of-range or NaN value reaches
    /// the plugin. A read-only parameter is written like any other: the
    /// session checks that policy before it hands a write over.
    fn set_parameter_value(&self, id: ParamId, value: Normalized) -> Result<(), UnknownParam> {
        self.edits.write(id, value)?;
        self.inform_controller_later();
        Ok(())
    }

    /// The plugin's own bypass at `site`. A parameter bypass is a parameter
    /// write ([`set_parameter_value`](Self::set_parameter_value)): stored, from
    /// any thread, and applied at the next block. A property bypass is written
    /// on the main thread and only there: the write runs at once and is
    /// [`Lost`](BypassWrite::Lost) when the instance is locked (a long
    /// `editor_idle`), and off the main thread without waiting for it: an
    /// in-process plugin's own property bypass is kept only by a session ticked
    /// on the main thread, where the session makes it.
    fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
        match site {
            OwnBypass::Parameter(id) => {
                match self.set_parameter_value(id, bypass_value(bypassed)) {
                    Ok(()) => BypassWrite::Sent,
                    Err(UnknownParam(_)) => BypassWrite::Refused,
                }
            }
            OwnBypass::Property => self
                .try_main(|main, instance| Some(instance.control()?.set_bypassed(main, bypassed)))
                .flatten()
                .unwrap_or(BypassWrite::Lost),
        }
    }

    /// The bypass the plugin holds now, read without waiting for the instance:
    /// `None` when it is locked or this is not the main thread, so a control
    /// tick skips a busy plugin instead of waiting on it.
    fn bypassed(&self, site: OwnBypass) -> Option<bool> {
        self.try_main(|main, instance| {
            let control = instance.control()?;
            match site {
                OwnBypass::Parameter(id) => control.parameter(main, id).map(is_bypassed),
                OwnBypass::Property => control.bypassed(main),
            }
        })
        .flatten()
    }

    /// The format's own conversion ([`Control::parameter_plain`]).
    fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64> {
        self.on_main(move |main, instance| instance.control()?.parameter_plain(main, id, value))?
    }

    /// The format's own conversion ([`Control::parameter_normalized`]).
    fn parameter_normalized(&self, id: ParamId, plain: f64) -> Option<Normalized> {
        self.on_main(move |main, instance| {
            instance.control()?.parameter_normalized(main, id, plain)
        })?
    }

    /// The plugin's display string for `value`, as
    /// [`Control::parameter_text`]
    /// answers it (VST2's only for the value it currently holds).
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        self.on_main(move |main, instance| instance.control()?.parameter_text(main, id, value))?
    }

    /// Parse through the plugin's own parser
    /// ([`Control::parameter_value_from_text`]).
    /// **On VST2 this writes**, since its opcode parses by applying.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let text = text.to_owned();
        self.on_main(move |main, instance| {
            instance
                .control()?
                .parameter_value_from_text(main, id, &text)
        })?
    }

    /// The plugin's own failure, forwarded rather than dropped, so the caller
    /// can tell "the plugin has no state" from "asking it failed".
    ///
    /// `effGetChunk` and its kin are main-thread calls, and the health snapshot
    /// runs this on the session's executor: off the main thread it
    /// is run by the main thread's next tick (`on_main`), and
    /// [`StateError::Unavailable`] if that does not come within the deadline.
    fn save_state(&self, state_use: StateUse) -> Result<Vec<u8>, StateError> {
        match self.on_main(move |main, instance| {
            instance.control().map(|control| {
                control
                    .save_state(main, state_use)
                    .map_err(StateError::from)
            })
        }) {
            Some(Some(state)) => state,
            Some(None) => Err(StateError::PluginCrashed),
            None => Err(StateError::Unavailable),
        }
    }

    /// Whether the plugin runs in `mode` now: told live where its format takes
    /// it (VST2 always does: the host answers
    /// `audioMasterGetCurrentProcessLevel` whenever the plugin asks), else
    /// reconfigured through the lifecycle so the next activation carries it
    /// (`Instance::set_render_mode`).
    ///
    /// Off the main thread it runs on the main thread's next tick, and is
    /// `false` if that does not come within the deadline.
    /// `InProcessClient::set_render_mode` is the same call on the node's side.
    fn set_render_mode(&self, mode: RenderMode) -> bool {
        self.on_main(move |main, instance| instance.set_render_mode(main, mode, None))
            .unwrap_or(false)
    }

    /// The format's automation-state call, on the main thread
    /// ([`Control::set_automation_state`]): [`Unsupported`](AutomationModeError::Unsupported)
    /// for a plugin without the channel, [`PluginDead`](AutomationModeError::PluginDead)
    /// once it is lost, and [`Dropped`](AutomationModeError::Dropped) when the
    /// main thread does not run the call (nothing pumps it, or it did not
    /// within the deadline); a later call may land.
    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), AutomationModeError> {
        match self.on_main(move |main, instance| {
            instance
                .control()
                .map(|control| control.set_automation_state(main, mode))
        }) {
            Some(Some(true)) => Ok(()),
            Some(Some(false)) => Err(AutomationModeError::Unsupported {
                format: self.format.to_string(),
            }),
            Some(None) => Err(AutomationModeError::PluginDead),
            None => Err(AutomationModeError::Dropped),
        }
    }
}

/// An in-process plugin is dead once its lifecycle poisons: a panic out of a
/// call into it latches the instance's [`Lost`], which is what this reads, with
/// no lock. A plugin that takes the host down with it is not observed; the rest
/// is the trait's defaults.
impl<L: InProcess> Health for ControlBackend<L> {
    fn is_crashed(&self) -> bool {
        self.lost.is_lost()
    }

    fn crash_cause(&self) -> Option<PluginFailure> {
        self.lost.cause()
    }

    fn rate_refusal(&self) -> Option<tutti_plugin_types::RateRefusal> {
        None
    }

    fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
        tutti_plugin_types::BlockCounts::default()
    }

    fn terminate(&self, _reason: TerminationReason) {}
}

impl<L> MainThreadControl for ControlBackend<L>
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    /// Under the instance's lock; not called for a lost plugin.
    fn lend(&self, f: &mut dyn FnMut(&mut dyn Control)) {
        if let Some(control) = self.inner.lock().control() {
            f(control);
        }
    }

    fn before_restore(&self) {
        self.edits.reset();
    }

    fn editor_idle(&self, main: MainThreadMarker) {
        // Before the lock below, not inside it: the drain takes the same lock
        // and would deadlock under that guard. This runs on the main thread,
        // every editor frame, which is what makes it the delivery point for the
        // rate the audio thread had to park.
        super::client::drain_sample_rate(main, &self.pending_sample_rate, &self.inner);

        let (changes, param_changes) = {
            let mut instance = self.inner.lock();
            // What the plugin changed about itself since the last pump: the
            // figures an activation read (a rate change that finished here, once
            // the audio thread handed its Processor back), and what the plugin
            // asked for. Re-read here, on the main thread, where a latency or
            // tail query belongs.
            let changes = instance.pump(main);
            let param_changes = match instance.control() {
                Some(control) => {
                    control.editor_idle(main);
                    instance.drain_param_changes(main)
                }
                // A lost plugin has nothing to idle; what it reported is told.
                None => Vec::new(),
            };
            (changes, param_changes)
        };
        // Drain any plugin-internal parameter changes (knob movement on
        // the editor surface) and forward them to the user sink. Fires
        // on the GUI thread — same threading contract as the parameter
        // sink callback for the subprocess backend (which fires on the
        // bridge thread). After the lock is released: a callback may call back.
        self.param_sink.fire_all(param_changes);
        self.reporting.deliver(changes);
    }
}

/// The main-thread job that tells a plugin's edit controller of the host's
/// writes, over the fake controller that records what it is told.
#[cfg(test)]
mod inform_tests {
    use super::*;
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::fake::{Call, FakePlugin};
    use tutti_plugin_types::{Loaded, MainWork, MaxBlock, Prepare};

    /// A fake whose controller is told through `queue_inform`, and the post
    /// that feeds it.
    fn rig() -> (
        FakePlugin,
        super::super::host_edits::EditPost,
        Arc<Mutex<tutti_plugin_types::fake::FakeController>>,
    ) {
        let (fake, loaded) = FakePlugin::new();
        fake.declare(&[(ParamId::new(1), false), (ParamId::new(2), false)]);
        let prepare = Prepare::new(
            tutti_units::SampleRate::SR_48K,
            MaxBlock::new(tutti_plugin_types::Samples::new(64)).expect("a block"),
        );
        let Ok((_processor, controller)) = loaded.activate(main_thread(), prepare) else {
            panic!("the fake activates");
        };
        let (post, _port) =
            super::super::host_edits::host_edits([ParamId::new(1), ParamId::new(2)]);
        (fake, post, Arc::new(Mutex::new(controller)))
    }

    fn informs(fake: &FakePlugin) -> Vec<(ParamId, f64)> {
        fake.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::InformController(id, value) => Some((id, value.get())),
                _ => None,
            })
            .collect()
    }

    /// The `tell` of a controller that is always free.
    fn tell_of(
        controller: &Arc<Mutex<tutti_plugin_types::fake::FakeController>>,
    ) -> Arc<dyn Fn(MainThreadMarker, &[(ParamId, Normalized)]) -> bool + Send + Sync> {
        let controller = Arc::clone(controller);
        Arc::new(move |main, told| {
            let Some(mut controller) = controller.try_lock() else {
                return false;
            };
            for &(id, value) in told {
                tutti_plugin_types::Control::inform_controller(&mut *controller, main, id, value);
            }
            true
        })
    }

    /// **A host write reaches the edit controller on the main thread, one value
    /// per parameter, the newest.** Three writes to two parameters before the
    /// main thread runs tell the controller of two values; a later write is told
    /// by the next job.
    #[test]
    fn the_controller_is_told_each_parameters_newest_write_on_the_main_thread() {
        let (collector, work) = MainWork::new();
        let (fake, post, controller) = rig();
        let (one, two) = (ParamId::new(1), ParamId::new(2));
        post.write(one, Normalized::of(0.25)).unwrap();
        post.write(two, Normalized::of(0.5)).unwrap();
        post.write(one, Normalized::of(0.75)).unwrap();
        assert!(post.claim_job());
        queue_inform(work.clone(), post.clone(), tell_of(&controller));
        assert!(
            informs(&fake).is_empty(),
            "nothing before the main thread runs"
        );
        collector.run(main_thread());
        assert_eq!(informs(&fake), [(one, 0.75), (two, 0.5)]);

        post.write(two, Normalized::of(0.125)).unwrap();
        assert!(post.claim_job());
        queue_inform(work, post, tell_of(&controller));
        collector.run(main_thread());
        assert_eq!(informs(&fake).last(), Some(&(two, 0.125)));
    }

    /// **A job that finds the instance busy waits for the next tick and loses
    /// nothing.**
    #[test]
    fn a_busy_instance_is_told_on_the_next_tick() {
        let (collector, work) = MainWork::new();
        let (fake, post, controller) = rig();
        let one = ParamId::new(1);
        post.write(one, Normalized::of(0.25)).unwrap();
        assert!(post.claim_job());
        queue_inform(work, post, tell_of(&controller));
        let held = controller.lock();
        collector.run(main_thread());
        assert!(informs(&fake).is_empty(), "busy: nothing told yet");
        drop(held);
        collector.run(main_thread());
        assert_eq!(informs(&fake), [(one, 0.25)]);
    }
}

/// The off-main-thread routing of the handle's methods (`on_main`), against the
/// reference VST2 plugin. The expiry rule is `off_main_wait`'s, tested there
/// over constructed instants.
#[cfg(test)]
mod off_main_tests {
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::{
        AutomationMode, AutomationModeError, MainWork, MainWorkHandle, RenderMode, StateError,
        StateUse,
    };
    use tutti_units::Normalized;
    use tutti_units::ParamId;
    use tutti_units::SampleRate;
    use tutti_vst2_host::Vst2Loaded;

    use crate::PluginHandle;

    /// The node of the probe, prepared and driven by hand.
    type Unit = tutti_graph::contract::Direct<crate::PluginNode<Vst2Loaded>>;

    /// The probe, loaded in process on this thread, which is not the main thread
    /// (the harness runs tests on threads it spawns), with a thread standing in for
    /// the main thread's tick: every call through the handle hops to it.
    fn loaded() -> (Unit, PluginHandle, tutti_fixtures::Pump) {
        let (collector, work) = MainWork::new();
        let pump = tutti_fixtures::pump(collector);
        let (unit, handle) = load(&work);
        (unit, handle, pump)
    }

    /// The probe, loaded in process, with parameter 1 written to 0.75 and one
    /// block run, which applies it; its main-thread work goes to `work`.
    fn load(work: &MainWorkHandle) -> (Unit, PluginHandle) {
        let (client, handle) = crate::load_client::<Vst2Loaded, f32>(
            main_thread(),
            work,
            &tutti_plugin_types::PluginRef::file(tutti_fixtures::vst2_probe()),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        let mut unit = Unit::new(client.bind(tutti_fixtures::tempo()), SampleRate::SR_48K, 64);
        handle
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.75))
            .expect("a declared parameter");
        unit.block();
        (unit, handle)
    }

    /// **A pool thread's call on the handle is answered by the main thread's
    /// tick**, with the real values: the state equals one saved on the main
    /// thread, and the parameter reads what was written. This is what the
    /// session's snapshot and parameter-mirror tasks do.
    #[test]
    fn a_pool_threads_calls_are_answered_by_the_main_threads_tick() {
        let (_unit, handle, _pump) = loaded();
        let expected = handle
            .any_thread()
            .save_state(StateUse::Project)
            .expect("saved on the main thread");
        let id = ParamId::new(1);
        let job = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                (
                    handle.any_thread().save_state(StateUse::Project),
                    handle.any_thread().parameter_value(id),
                )
            })
        };
        let (state, value) = job.join().expect("the job returns");
        assert_eq!(state.expect("saved off the main thread"), expected);
        assert_eq!(value.map(Normalized::get), Some(0.75));
    }

    /// **With a collector that has never run, an off-main call answers
    /// "unavailable" and queues nothing**: it waits for no main thread.
    #[test]
    fn a_never_run_collector_gets_the_unavailable_answer_and_queues_nothing() {
        let (collector, work) = MainWork::new();
        let (_unit, handle) = load(&work);
        let id = ParamId::new(1);
        let (state, value) = std::thread::spawn(move || {
            (
                handle.any_thread().save_state(StateUse::Project),
                handle.any_thread().parameter_value(id),
            )
        })
        .join()
        .expect("the job returns");
        assert!(matches!(state, Err(StateError::Unavailable)), "{state:?}");
        assert_eq!(value, None);
        assert_eq!(collector.run(main_thread()), 0, "nothing was queued");
    }

    /// **The parameter reads answer on the main thread**: the catalog lists
    /// the probe's parameters, the value is the one written, and the text
    /// and parse are the plugin's own (a text it cannot parse is `None`, never a
    /// default that looks like an answer).
    #[test]
    fn the_parameter_reads_answer_on_the_main_thread() {
        let (_unit, handle, _pump) = loaded();
        let any = handle.any_thread();
        let catalog = any.parameter_descriptors().expect("the probe lists");
        assert!(catalog.iter().any(|p| p.id() == ParamId::new(1)));
        assert_eq!(
            any.parameter_value(ParamId::new(1)).map(Normalized::get),
            Some(0.75)
        );
        let text = any.parameter_text(ParamId::new(1), Normalized::of(0.75));
        assert!(
            text.as_deref().is_some_and(|t| t.contains("0.75")),
            "the plugin's own display string: {text:?}"
        );
        assert_eq!(
            any.parameter_value_from_text(ParamId::new(1), "no such value"),
            None,
            "a text the plugin cannot parse leaves the field where it was"
        );
    }

    /// **A parameter write lands on the plugin** at the head of the next block.
    #[test]
    fn a_parameter_write_lands_on_the_plugin() {
        let (mut unit, handle, _pump) = loaded();
        handle
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.25))
            .expect("a declared parameter");
        unit.block();
        assert_eq!(
            handle
                .any_thread()
                .parameter_value(ParamId::new(1))
                .map(Normalized::get),
            Some(0.25)
        );
    }

    /// **A write made while the instance is locked is not lost**: the instance
    /// is held (as a long `editor_idle` holds it), a write is made and the main
    /// thread ticks, and once the lock is released the next block applies the
    /// write. A write that was posted to the main thread and skipped on a busy
    /// instance would leave the plugin at the value before it.
    #[test]
    fn a_write_made_while_the_instance_is_locked_lands_on_the_next_block() {
        let (collector, work) = MainWork::new();
        // Run once, so a post is queued rather than refused as unpumped.
        collector.run(main_thread());
        let (mut unit, handle) = load(&work);
        let instance = std::sync::Arc::clone(&unit.node.client.inner);
        let held = instance.lock();
        handle
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.25))
            .expect("a declared parameter");
        collector.run(main_thread());
        drop(held);
        unit.block();
        let _pump = tutti_fixtures::pump(collector);
        assert_eq!(
            handle
                .any_thread()
                .parameter_value(ParamId::new(1))
                .map(Normalized::get),
            Some(0.25)
        );
    }

    /// **A write made after a flush wins over it**, through the whole path: a
    /// write, a read (which flushes it into the plugin), a later write, a block.
    /// The plugin ends on the later write; a flush that landed after the block
    /// would leave it on the first.
    #[test]
    fn a_write_after_a_flush_is_the_plugins_last_word() {
        let (mut unit, handle, _pump) = loaded();
        let id = ParamId::new(1);
        handle
            .any_thread()
            .set_parameter_value(id, Normalized::of(0.25))
            .expect("a declared parameter");
        assert_eq!(
            handle.any_thread().parameter_value(id).map(Normalized::get),
            Some(0.25),
            "the read flushes the first write into the plugin"
        );
        handle
            .any_thread()
            .set_parameter_value(id, Normalized::of(0.75))
            .expect("a declared parameter");
        unit.block();
        assert_eq!(
            handle.any_thread().parameter_value(id).map(Normalized::get),
            Some(0.75)
        );
    }

    /// **A write to a parameter the plugin does not declare fails at the call**,
    /// naming it, from any thread.
    #[test]
    fn a_write_to_an_unknown_parameter_fails_at_the_call() {
        let (_unit, handle, _pump) = loaded();
        assert_eq!(
            handle
                .any_thread()
                .set_parameter_value(ParamId::new(9_999), Normalized::of(0.5)),
            Err(tutti_plugin_types::UnknownParam(ParamId::new(9_999)))
        );
    }

    /// **The state saved on the main thread is the plugin's own**: not
    /// `Unavailable` (nothing waited) and not empty.
    #[test]
    fn the_state_saves_on_the_main_thread() {
        let (_unit, handle, _pump) = loaded();
        let state = handle
            .any_thread()
            .save_state(StateUse::Project)
            .expect("the probe saves");
        assert!(!state.is_empty());
    }

    /// **The render mode is accepted** (VST2 answers the process level from a
    /// host callback, so there is nothing to decline).
    #[test]
    fn the_render_mode_is_accepted_on_the_main_thread() {
        let (_unit, handle, _pump) = loaded();
        assert!(handle.any_thread().set_render_mode(RenderMode::Offline));
        assert!(handle.any_thread().set_render_mode(RenderMode::Realtime));
    }

    /// **Off the main thread an in-process property bypass is lost and unread,
    /// and a parameter bypass is stored but unread**: a tick on a pool thread
    /// neither writes nor reads the plugin's property (the main thread's pump is
    /// not asked), while a parameter bypass is a parameter write, which needs no
    /// main thread. A write that took the main thread's hop would be answered by
    /// the pump, not lost.
    #[test]
    fn off_the_main_thread_an_in_process_bypass_is_stored_or_lost_and_unread() {
        let (_unit, handle, _pump) = loaded();
        for (site, written) in [
            (
                tutti_plugin_types::OwnBypass::Parameter(ParamId::new(1)),
                tutti_plugin_types::BypassWrite::Sent,
            ),
            (
                tutti_plugin_types::OwnBypass::Property,
                tutti_plugin_types::BypassWrite::Lost,
            ),
        ] {
            assert_eq!(
                handle.any_thread().set_bypassed(site, true),
                written,
                "{site:?}"
            );
            assert_eq!(handle.any_thread().bypassed(site), None, "{site:?}");
        }
    }

    /// **A VST2 plugin that advertises the soft bypass and refuses it claims no
    /// bypass of its own**: the reference plugin answers `canDo(bypass)` and
    /// declines `effSetBypass`, so its node keeps the executor's bypass and the
    /// handle writes nothing.
    #[test]
    fn a_plugin_that_refuses_its_soft_bypass_claims_no_bypass_of_its_own() {
        let (_unit, handle, _pump) = loaded();
        assert_eq!(handle.loaded().own_bypass, None);
        assert_eq!(
            handle.set_bypassed(true),
            tutti_plugin_types::BypassWrite::NoOwnBypass
        );
    }

    /// **The automation advisory reaches an in-process plugin that takes it**:
    /// the reference VST2 plugin carries the channel, so the call on the handle
    /// answers `Ok` from a thread that is not the main one (the pump runs it).
    #[test]
    fn the_automation_advisory_is_delivered_in_process() {
        let (_unit, handle, _pump) = loaded();
        assert_eq!(handle.set_automation_mode(AutomationMode::Writing), Ok(()));
    }

    /// **A healthy in-process plugin reads alive, with no refusal and zero
    /// block counts, and a `terminate` ends nothing**: there is no process to
    /// end.
    #[test]
    fn a_healthy_in_process_plugin_reads_alive() {
        let (_unit, handle, _pump) = loaded();
        assert!(!handle.is_crashed());
        assert!(matches!(handle.status(), crate::PluginStatus::Alive));
        assert_eq!(
            handle.block_counts(),
            tutti_plugin_types::BlockCounts::default()
        );
        handle.terminate(tutti_plugin_types::TerminationReason::Hung {
            blocks: 16,
            held: std::time::Duration::from_secs(5),
        });
        assert!(
            matches!(handle.status(), crate::PluginStatus::Alive),
            "there is no process to end"
        );
    }
    /// **The rest of the calls have the same unavailable answer when nothing
    /// pumps the main thread**: the catalog and the text are `None`, a render
    /// mode is refused (`false`), never a default that looks like an answer.
    /// (The advisory runs on the main thread too: it answers `Dropped`.)
    #[test]
    fn an_unpumped_main_thread_refuses_the_other_calls_too() {
        let (_collector, work) = MainWork::new();
        let (_unit, handle) = load(&work);
        let (catalog, text, from_text, mode, automation) = std::thread::spawn(move || {
            (
                handle.any_thread().parameter_descriptors(),
                handle
                    .any_thread()
                    .parameter_text(ParamId::new(1), Normalized::of(0.5)),
                handle
                    .any_thread()
                    .parameter_value_from_text(ParamId::new(1), "0.5"),
                handle.any_thread().set_render_mode(RenderMode::Offline),
                handle.set_automation_mode(AutomationMode::Off),
            )
        })
        .join()
        .expect("the job returns");
        assert!(catalog.is_none());
        assert!(text.is_none());
        assert!(from_text.is_none());
        assert!(!mode);
        assert_eq!(automation, Err(AutomationModeError::Dropped));
    }
}
