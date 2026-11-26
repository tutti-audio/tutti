//! What a loaded plugin answers from any thread ([`AnyThread`]), and its
//! liveness as its host observes it ([`Health`]).
//!
//! Both are what a `PluginHandle` holds beside the main-thread
//! [`Control`](crate::Control): a handle is cloned into pool jobs and
//! polled from any thread, so these calls take no
//! [`MainThreadMarker`](crate::MainThreadMarker).

use crate::{
    Answer, AutomationMode, AutomationModeError, BypassWrite, OwnBypass, ParamWriteError,
    ParameterInfo, PipelineCounts, PluginFailure, RenderMode, StateError, StateUse,
    TerminationReason, bypass_value,
};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// A plugin's saved state with the writes the host made that the bytes do not
/// hold yet ([`AnyThread::save_state`]): a save never waits for a block, so it
/// states what a block has not applied, and a load replays it
/// ([`pending`](Self::pending), after the bytes).
///
/// **The one gap.** A change the plugin made itself, whose report has not yet
/// reached the host, is not in the host's record: replaying a pending write over
/// it can overwrite it, by one report's latency.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginState {
    /// The plugin's own bytes.
    pub bytes: Vec<u8>,
    /// The newest unapplied host write of each parameter.
    pub pending: Vec<(ParamId, Normalized)>,
}

impl PluginState {
    /// The bytes with nothing pending.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            pending: Vec::new(),
        }
    }
}

/// One parameter as a read answers it ([`AnyThread::parameter_value`]): the
/// value the plugin holds beside the host's newest write it has not applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PluginValue {
    /// The plugin's own value.
    pub plugin: Normalized,
    /// The host's newest write no block has applied, if there is one.
    pub pending: Option<Normalized>,
}

impl PluginValue {
    /// A plugin's value with no write pending.
    #[must_use]
    pub const fn new(plugin: Normalized) -> Self {
        Self {
            plugin,
            pending: None,
        }
    }

    /// What the host wants the parameter to be: the pending write, else the
    /// plugin's value.
    #[must_use]
    pub fn wanted(self) -> Normalized {
        self.pending.unwrap_or(self.plugin)
    }
}

/// What a loaded plugin answers from any thread, without a main-thread token.
///
/// [`Control`](crate::Control) is a format's main-thread surface, and every
/// call of it takes the token that proves it. This is a *hosting's* surface,
/// and the two overlap on purpose, because the two hostings differ in how they
/// reach the plugin from a thread that is not the main one:
///
/// - **In-process**, the plugin may only be touched on the main thread, so the
///   hosting hops there (through its owner's `MainWork`) and calls `Control`; off the main
///   thread the call waits for the next main-thread tick up to a deadline, and
///   answers "unavailable" when it passes (`None`, `false`,
///   [`StateError::Unavailable`]). An owner that has never run its collector
///   has no main thread to hop to: the call answers "unavailable" at once.
///   A parameter write needs no hop: it goes to a slot the node's next block
///   drains.
/// - **Served**, a bridge answers from any thread, concurrently with the main
///   thread's own calls, so no hop is added. A host
///   that blocks its main thread on a snapshot or a parameter read would be
///   told "unavailable" if it hopped, for a call the bridge can answer.
///
/// A token-free `Control` would erase that difference by pretending the main
/// thread is not needed, which is false for every in-process format; so the
/// token stays on `Control` and the calls a handle makes without one live here.
///
/// Object-safe, `&self`, `Send + Sync`: a handle stores it as `Arc<dyn
/// AnyThread>`.
pub trait AnyThread: Send + Sync {
    /// The static parameter catalog (id, name, range, flags) as an [`Answer`]:
    /// queued, and the call returns without waiting. **In-process** the main
    /// thread fills it when it runs the queued work (at once when this is the
    /// main thread); **served** the bridge's control lane fills it with the
    /// server's reply, or with `None` at its query deadline. `None` is
    /// unavailable: the plugin is lost, the backend cannot list, or the deadline
    /// passed; a dropped sender cancels the answer, which a caller reads as
    /// unavailable too.
    fn parameter_descriptors(&self) -> Answer<Option<Vec<ParameterInfo>>>;

    /// The plugin's current value for one parameter as an [`Answer`], filled as
    /// [`parameter_descriptors`](Self::parameter_descriptors) is, with the
    /// host's newest write to it that no block has applied beside it
    /// ([`PluginValue`]): a read never waits for a block.
    ///
    /// `None` if unavailable, including when `id` uses the other addressing
    /// model, which addresses no parameter of this plugin. See [`ParamId`].
    fn parameter_value(&self, id: ParamId) -> Answer<Option<PluginValue>>;

    /// Writes one parameter value, for a UI knob or an initial value.
    ///
    /// **In process** the write is stored in the parameter's slot
    /// ([`edit_slots`](crate::edit_slots())) and applied at the head of the
    /// plugin's next block. A read, a save or a fork made before that block
    /// states the write as pending ([`PluginValue`], [`PluginState`]) and does
    /// not wait for it. It never waits for the plugin, is never dropped for a
    /// locked instance or for lack of room, and a newer write to the same
    /// parameter replaces one no block has taken. **A served** plugin's is
    /// stored in its slot on the client and sent with the next block.
    ///
    /// A write to a plugin that is lost is refused with
    /// [`ParamWriteError::Lost`], in both hostings, and is not counted in
    /// [`Health::refused_edits`], which counts only the writes a plugin
    /// refused when they were applied.
    ///
    /// **A control-side call**: it takes a lock (in process the slots' writer,
    /// served the declared list), so it belongs to the session's control
    /// threads and pool jobs, not to the audio thread.
    ///
    /// [`Normalized`], not a bare float: the type is what stops an
    /// out-of-range or NaN value (a served one is clamped again on receipt).
    ///
    /// # Errors
    ///
    /// [`ParamWriteError::Undeclared`] when the plugin did not declare `id`;
    /// [`ParamWriteError::Lost`] when it is lost;
    /// [`ParamWriteError::Unanswered`] when a served plugin gave no parameter
    /// list to check `id` against. Nothing is stored in any of the three.
    fn set_parameter_value(&self, id: ParamId, value: Normalized) -> Result<(), ParamWriteError>;

    /// The plain value `value` stands for, converted by the plugin's format
    /// ([`Control::parameter_plain`](crate::Control::parameter_plain)), so a
    /// consumer never computes it from a range. `None` when unavailable, or
    /// when the format has no plain value for the parameter.
    fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64>;

    /// The inverse of [`parameter_plain`](Self::parameter_plain)
    /// ([`Control::parameter_normalized`](crate::Control::parameter_normalized)),
    /// as an [`Answer`] filled as [`parameter_descriptors`](Self::parameter_descriptors)
    /// is.
    fn parameter_normalized(&self, id: ParamId, plain: f64) -> Answer<Option<Normalized>>;

    /// The plugin's own display string for `value`, such as `"800 Hz"`.
    ///
    /// `None` means the plugin did not answer and the caller renders the number
    /// itself; that is not the same as an empty label.
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String>;

    /// Parses `text` with the plugin's own interpretation, for a user typing
    /// into a parameter field. `None` when it cannot, and the caller then
    /// leaves the field where it was.
    ///
    /// **Not a pure query on every format.** VST2's `effString2Parameter` is
    /// a setter with no parse-only counterpart, so asking applies the value.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized>;

    /// Serializes the plugin's state for `state_use`: its own bytes, the same a
    /// project save stores for [`StateUse::Project`], with the host writes no
    /// block has applied beside them ([`PluginState`]). A plugin with nothing to
    /// save answers empty bytes.
    ///
    /// Never waits for a block. Blocks on a round trip (served) or a main-thread
    /// tick (in-process).
    ///
    /// # Errors
    ///
    /// A [`StateError`] saying why no state was produced: the plugin crashed,
    /// refused, the state exceeds the transport's limit, the transfer stalled,
    /// or ([`StateError::Unavailable`]) the plugin was not asked because the
    /// main thread did not answer in time.
    fn save_state(&self, state_use: StateUse) -> Result<PluginState, StateError>;

    /// Tells the plugin whether it is rendering offline, and returns whether it
    /// *accepted* the mode (`false` is a refusal or an unavailable plugin, not
    /// an error: the render is unchanged). See
    /// [`Features::RENDER_MODE`](crate::Features).
    fn set_render_mode(&self, mode: RenderMode) -> bool;

    /// Writes the plugin's own bypass, `true` bypassed and `false` processing,
    /// where `site` says the plugin keeps it
    /// ([`PluginWiring::own_bypass`](crate::PluginWiring::own_bypass)).
    /// Fire-and-forget like [`set_parameter_value`](Self::set_parameter_value):
    /// [`Sent`](BypassWrite::Sent) is not applied, and
    /// [`bypassed`](Self::bypassed) reads the result back.
    ///
    /// The default writes a [`Parameter`](OwnBypass::Parameter) through
    /// [`set_parameter_value`](Self::set_parameter_value) and loses a
    /// [`Property`](OwnBypass::Property) one, which only a backend that reaches
    /// the plugin on the main thread (in-process) overrides.
    fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
        match site {
            OwnBypass::Parameter(id) => {
                match self.set_parameter_value(id, bypass_value(bypassed)) {
                    Ok(()) => BypassWrite::Sent,
                    Err(ParamWriteError::Lost(_)) => BypassWrite::Lost,
                    Err(_) => BypassWrite::Refused,
                }
            }
            OwnBypass::Property => BypassWrite::Lost,
        }
    }

    /// Whether the plugin's own bypass is on now, read back from the plugin
    /// without waiting: `None` when the plugin did not answer or the backend
    /// does not read it. An in-process backend reads it live; a served
    /// plugin's bypass is compared through its mirrored parameter values
    /// instead, so its backend answers `None`.
    fn bypassed(&self, site: OwnBypass) -> Option<bool>;

    /// Announces the host's automation mode, for the editor's UI feedback
    /// (VST3 `IAutomationState`). Applies to the whole plugin. `Ok(())` means
    /// delivered, not that the plugin visibly reacted.
    ///
    /// # Errors
    ///
    /// [`AutomationModeError`]: the plugin is gone, the push was dropped (the
    /// command queue was full), or this hosting carries no such advisory.
    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), AutomationModeError>;
}

/// The plugin's liveness as its host observes it.
///
/// What a `PluginHandle` reports as its `status`
/// and acts on: a served plugin can die, refuse a rate, or stop answering, and
/// an in-process one cannot be observed to (a crash takes the host with it), so
/// its answers say so (never crashed, no refusal, zero counts, nothing to end). Atomic reads and one kill, callable from any
/// thread.
pub trait Health: Send + Sync {
    /// `true` if the plugin is gone (the subprocess crashed). An in-process
    /// plugin is never observed `true`.
    fn is_crashed(&self) -> bool;

    /// Why the plugin died, when it can say. `Some` only when
    /// [`is_crashed`](Self::is_crashed) is `true`. Latched at the detection
    /// site, so it answers even for a crash before the host listened.
    fn crash_cause(&self) -> Option<PluginFailure>;

    /// The refusal of the sample rate, while the plugin refuses it.
    /// An in-process plugin cannot refuse.
    fn rate_refusal(&self) -> Option<crate::RateRefusal>;

    /// Blocks the audio path gave up waiting for, and replies it took (on time
    /// or late), over the plugin's life: a plugin that stays alive but never
    /// returns from `process()` shows as the first growing while the second
    /// does not. An in-process plugin has no budget to miss.
    fn block_counts(&self) -> PipelineCounts;

    /// Host parameter writes the plugin did not take, over its life: a write
    /// refused when it was applied (a read-only or withdrawn parameter). A
    /// write to a plugin that was already lost is refused at the call
    /// ([`AnyThread::set_parameter_value`]) and not counted. A
    /// served plugin's is the server's count, as it last told.
    fn refused_edits(&self) -> u64;

    /// Ends the plugin's server process, latching
    /// [`PluginFailure::Terminated`] with `reason` as why it is dead
    /// (unless it already died of something else). An in-process
    /// plugin has no process to end.
    fn terminate(&self, reason: TerminationReason);
}

#[cfg(test)]
mod tests {
    use tutti_rt::Mutex;

    use super::*;
    use crate::{Answer, AutomationMode, ParamFlags, StateError, bypass_parameter};

    /// A plugin that lists `params` and records every parameter write.
    struct Listed {
        params: Vec<ParameterInfo>,
        written: Mutex<Vec<(ParamId, f64)>>,
    }

    impl AnyThread for Listed {
        fn parameter_descriptors(&self) -> Answer<Option<Vec<ParameterInfo>>> {
            Answer::ready(Some(self.params.clone()))
        }
        fn parameter_value(&self, _: ParamId) -> Answer<Option<PluginValue>> {
            Answer::ready(None)
        }
        fn set_parameter_value(
            &self,
            id: ParamId,
            value: Normalized,
        ) -> Result<(), ParamWriteError> {
            self.written.lock().push((id, value.get()));
            Ok(())
        }
        fn bypassed(&self, _: OwnBypass) -> Option<bool> {
            None
        }
        fn parameter_plain(&self, _: ParamId, _: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(&self, _: ParamId, _: f64) -> Answer<Option<Normalized>> {
            Answer::ready(None)
        }
        fn parameter_text(&self, _: ParamId, _: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _: ParamId, _: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self, _: StateUse) -> Result<PluginState, StateError> {
            Ok(PluginState::new(Vec::new()))
        }
        fn set_render_mode(&self, _: RenderMode) -> bool {
            false
        }
        fn set_automation_mode(&self, _: AutomationMode) -> Result<(), AutomationModeError> {
            Ok(())
        }
    }

    fn param(id: u32, bypass: bool) -> ParameterInfo {
        let reported = if bypass {
            ParamFlags::BYPASS
        } else {
            ParamFlags::empty()
        };
        ParameterInfo::new(ParamId::new(id), "p").with_flags(ParamFlags::BYPASS, reported)
    }

    /// A write of the bypass reaches the parameter the plugin lists as its
    /// bypass, `1` for bypassed and `0` for processing, and no other.
    #[test]
    fn a_bypass_write_reaches_the_listed_parameter_only() {
        let plugin = Listed {
            params: vec![param(1, false), param(9, true), param(2, false)],
            written: Mutex::new(Vec::new()),
        };
        let site = OwnBypass::Parameter(bypass_parameter(&plugin.params).unwrap());
        assert_eq!(plugin.set_bypassed(site, true), BypassWrite::Sent);
        assert_eq!(plugin.set_bypassed(site, false), BypassWrite::Sent);
        assert_eq!(
            *plugin.written.lock(),
            [(ParamId::new(9), 1.0), (ParamId::new(9), 0.0)]
        );
    }

    /// A bypass the backend cannot reach is a lost write, and it writes
    /// nothing.
    #[test]
    fn an_unlisted_bypass_is_lost_by_a_backend_that_cannot_reach_it() {
        let plugin = Listed {
            params: vec![param(1, false)],
            written: Mutex::new(Vec::new()),
        };
        assert_eq!(
            plugin.set_bypassed(OwnBypass::Property, true),
            BypassWrite::Lost
        );
        assert!(plugin.written.lock().is_empty());
        assert_eq!(plugin.bypassed(OwnBypass::Property), None);
    }
}
