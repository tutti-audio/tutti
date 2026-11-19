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
//! holds one; [`AnyThread`] takes none, because a served plugin answers those
//! calls from any thread and the session calls them from pool jobs. Here they
//! work from any thread too, through `on_main`: on the main thread the call runs
//! at once; off it the
//! work is posted to the owner's [`MainWork`](tutti_plugin_types::MainWork) and the
//! caller waits for the answer, which costs one main-thread tick (`Session::update`
//! runs the session's `MainWork` every tick). The wait has
//! a deadline ([`OFF_MAIN_DEADLINE`]): the host's main thread must keep
//! pumping, and a host that blocks its main thread on such a call (joining the
//! job from the main thread) gets the trait's "unavailable" answer (`None`,
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
    /// Where what a pump finds is delivered: a re-read that found the
    /// plugin's latency, tail or pins moved, what it asked of the transport
    /// (`PluginHandle::subscribe`).
    pub(crate) reporting: Reporting,
    /// Whether the plugin is lost, and why: what the handle's health reads.
    pub(crate) lost: Arc<Lost>,
    /// The plugin's format name, for the refusal of an advisory no in-process
    /// format carries.
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
            return Some(work(main, &mut self.inner.lock()));
        }
        if !self.work.is_pumped() {
            return None;
        }
        let (answer, wait) = sync_channel(1);
        let inner = Arc::clone(&self.inner);
        self.work.defer(move |main| {
            let _ = answer.send(work(main, &mut inner.lock()));
        });
        await_answer(
            &wait,
            OffMainWait::new(OFF_MAIN_DEADLINE, Instant::now()),
            Instant::now,
        )
    }

    /// `work` on the plugin now, when this is the main thread and the instance is
    /// not locked; `None` otherwise. Never waits.
    fn try_main<R>(&self, work: impl FnOnce(MainThreadMarker, &mut Instance<L>) -> R) -> Option<R> {
        let main = MainThreadMarker::new()?;
        let mut instance = self.inner.try_lock()?;
        Some(work(main, &mut instance))
    }

    /// `work` on the main thread without waiting: at once on it, posted to the
    /// owner's collector off it (refused while that collector has never run).
    /// Skipped when the instance is locked.
    fn post(&self, work: impl FnOnce(MainThreadMarker, &mut Instance<L>) + Send + 'static) {
        if let Some(main) = MainThreadMarker::new() {
            if let Some(mut instance) = self.inner.try_lock() {
                work(main, &mut instance);
            }
        } else if self.work.is_pumped() {
            let inner = Arc::clone(&self.inner);
            self.work.defer(move |main| {
                if let Some(mut instance) = inner.try_lock() {
                    work(main, &mut instance);
                }
            });
        }
    }
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

    /// Fire-and-forget, and skipped while the instance is locked (a long
    /// `editor_idle`): lost writes are recoverable, the GUI thread will retry on
    /// the next idle. Off the main thread it is posted to it.
    ///
    /// In-process, so this write reaches the plugin directly with no
    /// subprocess boundary to re-clamp at: it is a `Normalized` rather than a
    /// bare float precisely because nothing downstream would catch one that
    /// was not.
    fn set_parameter_value(&self, id: ParamId, value: Normalized) -> Result<(), UnknownParam> {
        self.post(move |main, instance| {
            if let Some(control) = instance.control() {
                // Fire-and-forget: a write the plugin refuses (undeclared,
                // read-only) is refused by `Control::set_parameter` here and
                // not written; this route has no reply to carry the error. The
                // session checks the same policy before it hands a write over.
                let _ = control.set_parameter(main, id, value);
            }
        });
        Ok(())
    }

    /// The plugin's own bypass at `site`, written on the main thread and only
    /// there: the write runs at once and is [`Lost`](BypassWrite::Lost) when the
    /// instance is locked (a long `editor_idle`). Off the main thread it is
    /// `Lost` without waiting for it: an in-process plugin's own bypass is kept
    /// only by a session ticked on the main thread, where the session makes it.
    fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
        self.try_main(|main, instance| {
            let control = instance.control()?;
            match site {
                OwnBypass::Parameter(id) => Some(BypassWrite::of_write(control.set_parameter(
                    main,
                    id,
                    bypass_value(bypassed),
                ))),
                OwnBypass::Property => Some(control.set_bypassed(main, bypassed)),
            }
        })
        .flatten()
        .unwrap_or(BypassWrite::Lost)
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
    /// runs this on the session's spawner: off the main thread it
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

    /// No in-process format carries the VST3-style advisory.
    fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), AutomationModeError> {
        Err(AutomationModeError::Unsupported {
            format: self.format.to_string(),
        })
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

    fn rate_refusal(&self) -> Option<(tutti_units::SampleRate, String)> {
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

    /// The probe, loaded in process on this thread, which is not the main thread
    /// (the harness runs tests on threads it spawns), with a thread standing in for
    /// the main thread's tick: every call through the handle hops to it.
    fn loaded() -> (
        crate::InProcessClient<Vst2Loaded>,
        PluginHandle,
        tutti_fixtures::Pump,
    ) {
        let (collector, work) = MainWork::new();
        let pump = tutti_fixtures::pump(collector);
        let (client, handle) = load(&work);
        (client, handle, pump)
    }

    /// The probe, loaded in process, with parameter 1 written to 0.75; its main-thread
    /// work goes to `work`.
    fn load(work: &MainWorkHandle) -> (crate::InProcessClient<Vst2Loaded>, PluginHandle) {
        let (client, handle) = crate::load_client::<Vst2Loaded, f32>(
            main_thread(),
            work,
            &tutti_plugin_types::PluginRef::file(tutti_fixtures::vst2_probe()),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        handle
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.75))
            .expect("a declared parameter");
        (client, handle)
    }

    /// **A pool thread's call on the handle is answered by the main thread's
    /// tick**, with the real values: the state equals one saved on the main
    /// thread, and the parameter reads what was written. This is what the
    /// session's snapshot and parameter-mirror jobs do.
    #[test]
    fn a_pool_threads_calls_are_answered_by_the_main_threads_tick() {
        let (_client, handle, _pump) = loaded();
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
        let (_client, handle) = load(&work);
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
        let (_client, handle, _pump) = loaded();
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

    /// **A parameter write lands on the plugin** (on the main thread it runs at
    /// once, under the instance's lock).
    #[test]
    fn a_parameter_write_lands_on_the_plugin() {
        let (_client, handle, _pump) = loaded();
        handle
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.25))
            .expect("a declared parameter");
        assert_eq!(
            handle
                .any_thread()
                .parameter_value(ParamId::new(1))
                .map(Normalized::get),
            Some(0.25)
        );
    }

    /// **The state saved on the main thread is the plugin's own**: not
    /// `Unavailable` (nothing waited) and not empty.
    #[test]
    fn the_state_saves_on_the_main_thread() {
        let (_client, handle, _pump) = loaded();
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
        let (_client, handle, _pump) = loaded();
        assert!(handle.any_thread().set_render_mode(RenderMode::Offline));
        assert!(handle.any_thread().set_render_mode(RenderMode::Realtime));
    }

    /// **Off the main thread an in-process bypass is lost and unread**, whether
    /// it is a parameter or a property: a tick on a pool thread neither writes
    /// nor reads the plugin (the main thread's pump is not asked). A write that
    /// took the main thread's hop would be answered by the pump, not lost.
    #[test]
    fn off_the_main_thread_an_in_process_bypass_is_lost_and_unread() {
        let (_client, handle, _pump) = loaded();
        for site in [
            tutti_plugin_types::OwnBypass::Parameter(ParamId::new(1)),
            tutti_plugin_types::OwnBypass::Property,
        ] {
            assert_eq!(
                handle.any_thread().set_bypassed(site, true),
                tutti_plugin_types::BypassWrite::Lost,
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
        let (_client, handle, _pump) = loaded();
        assert_eq!(handle.loaded().own_bypass, None);
        assert_eq!(
            handle.set_bypassed(true),
            tutti_plugin_types::BypassWrite::NoOwnBypass
        );
    }

    /// **No in-process format carries the automation advisory**, and says so
    /// with `AutomationModeError::Unsupported` for its format, as the handle with
    /// no such route always did.
    #[test]
    fn the_automation_advisory_is_not_supported_in_process() {
        let (_client, handle, _pump) = loaded();
        let refused = handle
            .set_automation_mode(AutomationMode::Writing)
            .expect_err("no in-process format carries it");
        assert!(
            matches!(refused, AutomationModeError::Unsupported { ref format } if format == "vst2"),
            "{refused:?}"
        );
    }

    /// **A healthy in-process plugin reads alive, with no refusal and zero
    /// block counts, and a `terminate` ends nothing**: there is no process to
    /// end.
    #[test]
    fn a_healthy_in_process_plugin_reads_alive() {
        let (_client, handle, _pump) = loaded();
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
    /// (The advisory needs no main thread: it refuses at once.)
    #[test]
    fn an_unpumped_main_thread_refuses_the_other_calls_too() {
        let (_collector, work) = MainWork::new();
        let (_client, handle) = load(&work);
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
        assert!(matches!(
            automation,
            Err(AutomationModeError::Unsupported { .. })
        ));
    }
}
