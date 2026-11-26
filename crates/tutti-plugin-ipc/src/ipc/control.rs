//! [`Control`] over the bridge, for a plugin loaded ([`Ipc`]) or active
//! ([`IpcController`]): the same calls either way, answered by the bridge.
//!
//! None of them reads its [`MainThreadMarker`] token: the bridge answers from any
//! thread, and the token is the in-process trait's
//! proof, not a hop. A caller with no token uses the bridge directly
//! ([`Ipc::bridge`]).

use dpi::Size;
use tutti_plugin_types::{
    Answer, AnyThread, AutomationMode, AutomationModeError, BypassWrite, ContentScale, Control,
    Delivered, EditorCapabilities, EditorOpened, EditorPresence, EditorWindow, Features, Health,
    MainThreadMarker, MidiAssignment, MidiMapApplied, ModeTold, OwnBypass, ParamWriteError,
    ParameterInfo, PipelineCounts, PluginDescriptor, PluginError, PluginFailure, PluginLearnError,
    PluginPreset, PluginResult, PluginWiring, PresetId, ProcessWake, RateRefusal, RenderMode,
    RenderModeSupport, Request, StateError, StateUse, TerminationReason, Told, TrackInfo,
    WindowHandle, bypass_value,
};
use tutti_units::Normalized;
use tutti_units::ParamId;

use super::{Ipc, IpcController};

/// `$ty`'s [`AnyThread`], [`EditorWindow`] and [`Control`], all through its
/// `link`.
///
/// Each call both traits carry has **one** body, in `AnyThread`; `Control`'s
/// forwards to it. The two exist because `Control` takes the main-thread
/// token a served plugin does not need, and a handle's off-thread calls have
/// none to give.
macro_rules! served_control {
    ($ty:ty) => {
        impl AnyThread for $ty {
            fn parameter_descriptors(&self) -> Answer<Option<Vec<ParameterInfo>>> {
                self.link.bridge.audio().parameters_answer()
            }

            /// A served read states no pending write: the host's writes reach the
            /// server by a flush the read does not wait for or report.
            fn parameter_value(
                &self,
                id: ParamId,
            ) -> Answer<Option<tutti_plugin_types::PluginValue>> {
                self.link.bridge.audio().parameter_answer(id)
            }

            /// `None`: the session compares a served plugin's bypass through its
            /// mirrored parameter values.
            fn bypassed(&self, _site: OwnBypass) -> Option<bool> {
                None
            }

            fn parameter_normalized(&self, id: ParamId, plain: f64) -> Answer<Option<Normalized>> {
                self.link
                    .bridge
                    .audio()
                    .parameter_normalized_answer(id, plain)
            }

            /// Stored in the parameter's slot and sent with the next block as a
            /// change on its first frame ([`AudioBridge::write_parameter`]), so
            /// it is never refused for a full queue, and the latest write to a
            /// parameter wins. The server's own `Control::set_parameter` policy
            /// refuses a write to a read-only parameter, and writes nothing. The
            /// client-side check, which names the error, is
            /// [`Control::set_parameter`].
            ///
            /// A control-side call: it takes a lock, and is not for the audio
            /// thread.
            ///
            /// Stays `Normalized` to the wire, which carries its `f64`.
            /// The server's decode refuses a value outside 0..=1 (NaN
            /// included): the wire is a foreign boundary and the peer is
            /// another process.
            ///
            /// [`AudioBridge::write_parameter`]: crate::AudioBridge
            fn set_parameter_value(
                &self,
                id: ParamId,
                value: Normalized,
            ) -> Result<(), ParamWriteError> {
                self.link.bridge.set_parameter_value(id, value)
            }

            /// A parameter bypass is a parameter write: [`Lost`](BypassWrite::Lost)
            /// when the link is gone, so the session writes it again,
            /// [`Refused`](BypassWrite::Refused) for a parameter the plugin does
            /// not declare. A property bypass is not claimed by a served plugin.
            fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
                match site {
                    OwnBypass::Parameter(_) if self.link.bridge.audio().is_crashed() => {
                        BypassWrite::Lost
                    }
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

            fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64> {
                self.link.bridge.audio().parameter_plain(id, value)
            }

            fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
                self.link.bridge.audio().parameter_text(id, value)
            }

            /// Asked of the audio instance, like every other parameter read.
            /// On VST2 this also *writes* that instance (`effString2Parameter`
            /// parses by applying), the instance `set_parameter_value`
            /// targets; the editor's mirror follows through the existing
            /// `PluginParamValuesChanged` refresh.
            fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
                self.link.bridge.audio().parameter_value_from_text(id, text)
            }

            /// The bridge flushes the host's writes and saves after them, so the
            /// bytes hold them and none is stated pending.
            fn save_state(
                &self,
                state_use: StateUse,
            ) -> Result<tutti_plugin_types::PluginState, StateError> {
                self.link
                    .bridge
                    .audio()
                    .save_state(state_use)
                    .map(tutti_plugin_types::PluginState::new)
            }

            /// Queued on the command bus as the blocks around it are, so the
            /// change lands between two blocks, and never needs a restart.
            /// `true` when queued: the plugin's answer reaches the host as
            /// [`Told`]. A plugin without `Features::RENDER_MODE` is not asked,
            /// as one in process is not: it has no mode to take.
            fn set_render_mode(&self, mode: RenderMode) -> bool {
                self.link.loaded.features.contains(Features::RENDER_MODE)
                    && !self.link.bridge.audio().is_crashed()
                    && self.link.bridge.audio().set_render_mode(mode)
            }

            /// Delivered to both the audio subprocess and the in-process GUI
            /// (the knob-glow lives in the GUI). [`Delivered`] says whether
            /// the command was *queued*; no format confirms the plugin
            /// visibly reacted.
            fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), AutomationModeError> {
                if self.link.bridge.audio().is_crashed() {
                    return Err(AutomationModeError::PluginDead);
                }
                automation_push_outcome(self.link.bridge.set_automation_state_rt(mode))
            }
        }

        impl EditorWindow for $ty {
            fn has_editor(&self, _main: MainThreadMarker) -> bool {
                !matches!(self.link.descriptor.editor, EditorPresence::Absent)
            }

            fn open_editor(
                &mut self,
                main: MainThreadMarker,
                parent: WindowHandle,
                scale: ContentScale,
            ) -> PluginResult<EditorOpened<Size>> {
                Ok(self.link.bridge.editor().open_editor(main, parent, scale)?)
            }

            fn open_floating_editor(
                &mut self,
                main: MainThreadMarker,
            ) -> PluginResult<EditorOpened<()>> {
                Ok(self.link.bridge.editor().open_floating_editor(main)?)
            }

            fn set_editor_scale(
                &mut self,
                main: MainThreadMarker,
                scale: ContentScale,
            ) -> PluginResult<()> {
                Ok(self.link.bridge.editor().set_editor_scale(main, scale)?)
            }

            fn close_editor(&mut self, main: MainThreadMarker) {
                let _ = self.link.bridge.editor().close_editor(main);
            }

            fn editor_idle(&mut self, main: MainThreadMarker) {
                self.link.bridge.editor().editor_idle(main);
            }

            fn wakeups(&mut self, main: MainThreadMarker, out: &mut tutti_plugin_types::Wakeups) {
                self.link.bridge.editor().wakeups(main, out);
            }

            fn wake(
                &mut self,
                main: MainThreadMarker,
                ready: &[tutti_plugin_ipc_ffi::Ready],
                now: tutti_platform::Instant,
            ) {
                self.link.bridge.editor().wake(main, ready, now);
            }

            fn editor_capabilities(&mut self, main: MainThreadMarker) -> EditorCapabilities {
                self.link.bridge.editor().editor_capabilities(main)
            }

            fn set_editor_size(
                &mut self,
                main: MainThreadMarker,
                requested: Size,
            ) -> PluginResult<Size> {
                Ok(self.link.bridge.editor().set_editor_size(main, requested)?)
            }

            fn poll_editor_resize_request(&mut self, main: MainThreadMarker) -> Option<Size> {
                self.link.bridge.editor().poll_editor_resize_request(main)
            }
        }

        impl Control for $ty {
            fn info(&self) -> &PluginDescriptor {
                &self.link.descriptor
            }

            fn wiring(&self, _main: MainThreadMarker) -> PluginWiring {
                // The buses are the activation's; the declared controller table is
                // the server's, read now.
                let mut wiring = self.wiring_now();
                if let Some(live) = self.link.bridge.audio().wiring() {
                    wiring.assignments = live.assignments;
                    wiring.assignments_applied = live.assignments_applied;
                }
                wiring
            }

            fn parameters(&self, _main: MainThreadMarker) -> Vec<ParameterInfo> {
                self.link.bridge.audio().parameters().unwrap_or_default()
            }

            fn parameter(&self, _main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
                self.link.bridge.audio().parameter(id)
            }

            fn set_parameter(
                &mut self,
                main: MainThreadMarker,
                id: ParamId,
                value: Normalized,
            ) -> Result<(), tutti_plugin_types::ParamWriteError> {
                // The policy answers here, at the call, from the declared
                // parameters kept on the client; the write itself is
                // fire-and-forget across the wire.
                self.parameter_writable(main, id)?;
                AnyThread::set_parameter_value(self, id, value)
            }

            /// From the declared parameters the client keeps, not a round trip
            /// per write (the client's declared-parameter cache): a lost plugin is
            /// [`Lost`](tutti_plugin_types::ParamWriteError::Lost), one that does
            /// not answer in time [`Unanswered`](tutti_plugin_types::ParamWriteError::Unanswered).
            fn parameter_writable(
                &self,
                _main: MainThreadMarker,
                id: ParamId,
            ) -> Result<(), tutti_plugin_types::ParamWriteError> {
                let declared = self.link.declared(id)?;
                tutti_plugin_types::ParamWriteError::check_sorted(id, &declared)
            }

            fn parameter_plain(
                &self,
                _main: MainThreadMarker,
                id: ParamId,
                value: Normalized,
            ) -> Option<f64> {
                AnyThread::parameter_plain(self, id, value)
            }

            fn parameter_normalized(
                &self,
                _main: MainThreadMarker,
                id: ParamId,
                plain: f64,
            ) -> Option<Normalized> {
                self.link.bridge.audio().parameter_normalized(id, plain)
            }

            fn parameter_text(
                &self,
                _main: MainThreadMarker,
                id: ParamId,
                value: Normalized,
            ) -> Option<String> {
                AnyThread::parameter_text(self, id, value)
            }

            fn parameter_value_from_text(
                &self,
                _main: MainThreadMarker,
                id: ParamId,
                text: &str,
            ) -> Option<Normalized> {
                AnyThread::parameter_value_from_text(self, id, text)
            }

            fn save_state(
                &mut self,
                _main: MainThreadMarker,
                state_use: StateUse,
            ) -> PluginResult<Vec<u8>> {
                self.link
                    .bridge
                    .audio()
                    .save_state(state_use)
                    .map_err(PluginError::StateFailed)
            }

            fn load_state(
                &mut self,
                main: MainThreadMarker,
                bytes: &[u8],
                state_use: StateUse,
            ) -> PluginResult<()> {
                self.link
                    .bridge
                    .load_state(main, bytes, state_use)
                    .map_err(PluginError::StateFailed)
            }

            fn presets(&mut self, _main: MainThreadMarker) -> Vec<PluginPreset> {
                self.link.bridge.audio().presets().unwrap_or_default()
            }

            fn load_preset(&mut self, _main: MainThreadMarker, id: &PresetId) -> bool {
                self.link.bridge.audio().load_preset(id.clone())
            }

            fn current_preset(&mut self, _main: MainThreadMarker) -> Option<PresetId> {
                self.link.bridge.audio().current_preset()
            }

            /// Whether `AnyThread::set_automation_mode` delivered the mode.
            fn set_automation_state(
                &mut self,
                _main: MainThreadMarker,
                mode: AutomationMode,
            ) -> bool {
                AnyThread::set_automation_mode(self, mode).is_ok()
            }

            /// What the plugin re-reported since the last call (a latency or
            /// a tail), as the requests the lifecycle answers by re-reading.
            fn take_requests(&mut self, _main: MainThreadMarker, out: &mut Vec<Request>) {
                let mut asked = self.link.events.requests.lock();
                out.append(&mut asked);
            }

            /// What the server answered to the rate and render mode the host
            /// told it since the last call.
            fn take_answers(&mut self, _main: MainThreadMarker, out: &mut Vec<Told>) {
                out.append(&mut self.link.events.told.lock());
            }

            /// Sent to the server, whose format answers the plugin's reads
            /// from it: queued ([`Yes`](Delivered::Yes)), refused because the
            /// queue is full ([`Dropped`](Delivered::Dropped)) or because the
            /// plugin is gone ([`PluginDead`](Delivered::PluginDead)).
            fn set_track_info(&mut self, _main: MainThreadMarker, info: &TrackInfo) -> Delivered {
                self.link.bridge.audio().set_track_info(info)
            }

            /// Sent to the server's plugin, which answers who applies the
            /// entries.
            fn set_app_midi_map(
                &mut self,
                _main: MainThreadMarker,
                app: &[MidiAssignment],
            ) -> MidiMapApplied {
                self.link.bridge.audio().set_app_midi_map(app)
            }

            fn arm_plugin_midi_learn(
                &mut self,
                _main: MainThreadMarker,
                armed: bool,
            ) -> Result<(), PluginLearnError> {
                self.link.bridge.audio().arm_plugin_midi_learn(armed)
            }

            /// What the server's `ProcessRequested` raises, from the bridge's
            /// thread.
            fn bind_wake(&self, wake: ProcessWake) {
                let _ = self.link.events.wake.set(wake);
            }

            /// `Live` from here wherever the plugin has a mode: the server
            /// tells it or activates it again itself and answers, so the host
            /// never reactivates a served plugin for its mode.
            fn render_mode_support(&self, _main: MainThreadMarker) -> RenderModeSupport {
                if self.link.loaded.features.contains(Features::RENDER_MODE) {
                    RenderModeSupport::Live
                } else {
                    RenderModeSupport::Unsupported
                }
            }

            /// Queued for the server, which answers later.
            fn set_render_mode(&mut self, _main: MainThreadMarker, mode: RenderMode) -> ModeTold {
                if AnyThread::set_render_mode(self, mode) {
                    ModeTold::Asked
                } else {
                    ModeTold::Refused
                }
            }

            /// The edits the host lost: counted by the bridge, which forwards
            /// a served plugin's edits through its listener
            /// ([`Ipc::forward_events`]).
            fn dropped_reported_edits(&self) -> u64 {
                self.link.bridge.audio().dropped_parameter_events()
            }

            /// The off-thread callbacks the plugin server refused, as its
            /// latest `News::ServerLosses` told.
            fn refused_off_thread_calls(&self) -> tutti_units::Calls {
                self.link.counters.server_losses().refused_off_thread_calls
            }
        }
    };
}

served_control!(Ipc);
served_control!(IpcController);

/// A served plugin's liveness, from the bridge and the server's guard.
impl Health for IpcController {
    fn is_crashed(&self) -> bool {
        self.link.bridge.audio().is_crashed()
    }

    fn crash_cause(&self) -> Option<PluginFailure> {
        self.link.bridge.audio().crash_cause()
    }

    fn rate_refusal(&self) -> Option<RateRefusal> {
        self.link.bridge.audio().rate_refusal()
    }

    fn block_counts(&self) -> PipelineCounts {
        self.link.bridge.audio().block_counts()
    }

    fn refused_edits(&self) -> u64 {
        // Two counts of one kind, each in the place that sees it: the server
        // counts the edits its plugin refused when applied
        // (`ServerLosses::edits_refused`), the client counts the writes a flush
        // the server answered as not applied did not deliver
        // (`AudioBridge::unapplied_edits`).
        self.link.counters.server_losses().edits_refused
            + self.link.bridge.audio().unapplied_edits()
    }

    fn terminate(&self, reason: TerminationReason) {
        // The cause first: the kill makes the bridge thread read an EOF, and
        // the first cause latched wins.
        self.link
            .bridge
            .audio()
            .mark_crashed(PluginFailure::Terminated { reason });
        self.link.guard.terminate();
    }
}

/// Turn a queue outcome into the error a UI shows.
///
/// The two failures produce **different** errors because they call for
/// different responses: a dropped push is transient and worth retrying, a dead
/// plugin is permanent.
fn automation_push_outcome(delivered: Delivered) -> Result<(), AutomationModeError> {
    match delivered {
        Delivered::Yes => Ok(()),
        Delivered::Dropped => Err(AutomationModeError::Dropped),
        Delivered::PluginDead => Err(AutomationModeError::PluginDead),
    }
}

#[cfg(test)]
mod automation_outcome_tests {
    use super::*;

    /// A dropped push and a dead plugin must not produce the same error.
    ///
    /// This is the property the `bool` could not express, and the only reason
    /// [`Delivered`] exists rather than a two-state answer: one is transient and
    /// worth retrying, the other is permanent. Asserting they *differ* is what
    /// fails if someone later folds the two arms back together — an assertion
    /// on either arm alone would survive that.
    #[test]
    fn a_dropped_push_and_a_dead_plugin_report_differently() {
        let dropped = automation_push_outcome(Delivered::Dropped)
            .expect_err("a dropped push is not a success");
        let dead = automation_push_outcome(Delivered::PluginDead)
            .expect_err("a dead plugin is not a success");

        assert!(
            matches!(dropped, AutomationModeError::Dropped),
            "a full queue is the plugin declining to be reached, not a crash: {dropped:?}"
        );
        assert!(
            matches!(dead, AutomationModeError::PluginDead),
            "a dead plugin must report as crashed: {dead:?}"
        );
        assert_ne!(
            dropped.to_string(),
            dead.to_string(),
            "the two failures must be distinguishable by a user reading the message"
        );
    }

    /// The negative half: a delivered push is not reported as a failure. Without
    /// this, folding every arm to `Err` would still pass the test above.
    #[test]
    fn a_delivered_push_is_not_an_error() {
        assert!(automation_push_outcome(Delivered::Yes).is_ok());
    }
}
