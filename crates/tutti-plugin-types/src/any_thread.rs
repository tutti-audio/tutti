//! What a loaded plugin answers from any thread ([`AnyThread`]), and its
//! liveness as its host observes it ([`Health`]).
//!
//! Both are what a `PluginHandle` holds beside the main-thread
//! [`Control`](crate::Control): a handle is cloned into pool jobs and
//! polled from any thread, so these calls take no
//! [`MainThreadMarker`](crate::MainThreadMarker).

use crate::{
    AutomationMode, AutomationModeError, BlockCounts, BypassWrite, OwnBypass, ParameterInfo,
    PluginFailure, RenderMode, SampleRate, StateError, StateUse, TerminationReason, UnknownParam,
    bypass_value, is_bypassed,
};
use tutti_units::Normalized;
use tutti_units::ParamId;

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
///   [`StateError::Unavailable`], or the write is dropped). An owner that has
///   never run its collector has no main thread to hop to: the call answers
///   "unavailable" at once, and a write is dropped rather than queued.
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
    /// The static parameter catalog (id, name, range, flags), or `None` when
    /// it is unavailable: the plugin is lost, or (in-process, off the main
    /// thread) the main thread did not answer in time.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>>;

    /// The plugin's current value for one parameter.
    ///
    /// `None` if unavailable, including when `id` uses the other addressing
    /// model, which addresses no parameter of this plugin. See [`ParamId`].
    fn parameter_value(&self, id: ParamId) -> Option<Normalized>;

    /// Writes one parameter value, for a UI knob or an initial value.
    /// Fire-and-forget, and never blocks the audio thread.
    ///
    /// [`Normalized`], not a bare float: an in-process plugin is written
    /// straight through, so the type is what stops an out-of-range or NaN
    /// value there (a served one is clamped again on receipt).
    ///
    /// # Errors
    ///
    /// [`UnknownParam`] when the plugin did not declare `id`, as far as the
    /// hosting knows without asking the plugin.
    fn set_parameter_value(&self, id: ParamId, value: Normalized) -> Result<(), UnknownParam>;

    /// The plain value `value` stands for, converted by the plugin's format
    /// ([`Control::parameter_plain`](crate::Control::parameter_plain)), so a
    /// consumer never computes it from a range. `None` when unavailable, or
    /// when the format has no plain value for the parameter.
    fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64>;

    /// The inverse of [`parameter_plain`](Self::parameter_plain)
    /// ([`Control::parameter_normalized`](crate::Control::parameter_normalized)).
    fn parameter_normalized(&self, id: ParamId, plain: f64) -> Option<Normalized>;

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
    /// project save stores for [`StateUse::Project`]. A plugin with nothing to
    /// save answers `Ok(vec![])`.
    ///
    /// Blocks on a round trip (served) or a main-thread tick (in-process).
    ///
    /// # Errors
    ///
    /// A [`StateError`] saying why no state was produced: the plugin crashed,
    /// refused, the state exceeds the transport's limit, the transfer stalled,
    /// or ([`StateError::Unavailable`]) the plugin was not asked because the
    /// main thread did not answer in time.
    fn save_state(&self, state_use: StateUse) -> Result<Vec<u8>, StateError>;

    /// Tells the plugin whether it is rendering offline, and returns whether it
    /// *accepted* the mode (`false` is a refusal or an unavailable plugin, not
    /// an error: the render is unchanged). See
    /// [`Features::RENDER_MODE`](crate::Features).
    fn set_render_mode(&self, mode: RenderMode) -> bool;

    /// Writes the plugin's own bypass, `true` bypassed and `false` processing,
    /// where `site` says the plugin keeps it
    /// ([`LoadedPlugin::own_bypass`](crate::LoadedPlugin::own_bypass)).
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
                    Err(UnknownParam(_)) => BypassWrite::Refused,
                }
            }
            OwnBypass::Property => BypassWrite::Lost,
        }
    }

    /// Whether the plugin's own bypass is on now, read back from the plugin:
    /// `None` when the plugin did not answer, or (a
    /// [`Property`](OwnBypass::Property)) the backend cannot reach it. A served
    /// plugin's read is a round trip, so the session compares such a plugin
    /// with its mirrored parameter values instead.
    fn bypassed(&self, site: OwnBypass) -> Option<bool> {
        match site {
            OwnBypass::Parameter(id) => self.parameter_value(id).map(is_bypassed),
            OwnBypass::Property => None,
        }
    }

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

    /// The sample rate the plugin refused and its reason, while it is refused.
    /// An in-process plugin cannot refuse.
    fn rate_refusal(&self) -> Option<(SampleRate, String)>;

    /// Blocks the audio path gave up waiting for, and replies it took (on time
    /// or late), over the plugin's life: a plugin that stays alive but never
    /// returns from `process()` shows as the first growing while the second
    /// does not. An in-process plugin has no budget to miss.
    fn block_counts(&self) -> BlockCounts;

    /// Ends the plugin's server process, latching
    /// [`PluginFailure::Terminated`] with `reason` as why it is dead
    /// (unless it already died of something else). An in-process
    /// plugin has no process to end.
    fn terminate(&self, reason: TerminationReason);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{AutomationMode, ParamFlags, StateError, bypass_parameter};

    /// A plugin that lists `params` and records every parameter write.
    struct Listed {
        params: Vec<ParameterInfo>,
        written: Mutex<Vec<(ParamId, f64)>>,
    }

    impl AnyThread for Listed {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            Some(self.params.clone())
        }
        fn parameter_value(&self, _: ParamId) -> Option<Normalized> {
            None
        }
        fn set_parameter_value(&self, id: ParamId, value: Normalized) -> Result<(), UnknownParam> {
            self.written.lock().unwrap().push((id, value.get()));
            Ok(())
        }
        fn parameter_plain(&self, _: ParamId, _: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(&self, _: ParamId, _: f64) -> Option<Normalized> {
            None
        }
        fn parameter_text(&self, _: ParamId, _: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _: ParamId, _: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self, _: StateUse) -> Result<Vec<u8>, StateError> {
            Ok(Vec::new())
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
            *plugin.written.lock().unwrap(),
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
        assert!(plugin.written.lock().unwrap().is_empty());
        assert_eq!(plugin.bypassed(OwnBypass::Property), None);
    }
}
