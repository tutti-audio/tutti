//! The handlers of the control messages: one [`Handle`] per row of the table,
//! which `serve` picks by the request.

use super::{BLOCK_WAIT, Session, editor_wire_error, load_failure};
use crate::audio_pipeline::Clock;
use crate::render::ToRender;
use crate::served::AnyServer;
use tutti_plugin_ipc::{
    AppMidiMapApplied, ArmPluginMidiLearn, CloseEditor, CurrentPreset, EditorOpened, EditorSized,
    GetCurrentPreset, GetParameter, GetParameterList, GetParameterNormalized, GetParameterPlain,
    GetParameterText, GetParameterValueFromText, GetPresetList, GetWiring, Handle, Handling,
    LoadPlugin, LoadPreset, LoadState, ModeOutcome, OpenEditor, OpenLane, ParameterList,
    ParameterNormalized, ParameterPlain, ParameterText, ParameterValue, ParameterValueFromText,
    PluginLoaded, PluginMidiLearn, PresetList, PresetLoaded, RenderModeSet, SampleRateSet,
    SaveState, SavedState, SetAppMidiMap, SetAutomationState, SetEditorSize, SetRenderMode,
    SetSampleRate, SetTrackInfo, SetupSharedMemory, Shutdown, StageState, Stop, Wiring, Working,
};
use tutti_plugin_types::{
    MAX_STATE_BYTES, MidiMapApplied, ParameterQueue, PluginLearnError, PluginRef, ServerError,
    StateError,
};

impl Handle<LoadPlugin> for Session {
    /// Opens the plugin the message names, replacing the one held.
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        LoadPlugin {
            path,
            format,
            plugin,
            sample_rate,
            block_size,
            preferred_format,
            state: declared,
        }: LoadPlugin,
    ) -> Result<PluginLoaded, ServerError> {
        let main = cx.main;
        // What a host staged for this load, restored before the activation: only
        // when the load declares exactly it. Staged here or declared here and not
        // the other, or of another length, is a host that is out of step with
        // this server (or a stage left by another load), and restoring it would
        // hand a plugin a state nobody meant for it.
        let staged = self.staged.take();
        // A load replaces what the server held, whether or not it succeeds:
        // a refused one must not leave the old plugin answering a host that
        // was told the load failed (and holding its instance while the next
        // one loads).
        self.drop_plugin(main);
        self.rate_refused = None;
        match (&declared, &staged) {
            (None, None) => {}
            (Some(d), Some(bytes)) if d.len == bytes.len() as u64 => {}
            (declared, staged) => {
                return Err(ServerError::StateLengthMismatch {
                    declared: declared.map(|d| d.len),
                    staged: staged.as_ref().map(|s| s.len() as u64),
                });
            }
        }
        let mut restoring = || cx.interim(Working::Restoring);
        let (plugin, state_error, seat) = match AnyServer::load(
            main,
            &self.work_handle,
            &PluginRef::of(path, plugin),
            format,
            sample_rate,
            block_size,
            preferred_format,
            staged.as_deref().zip(declared.map(|d| d.state_use)),
            &mut restoring,
            self.render.ties().clone(),
        ) {
            Ok(loaded) => loaded,
            Err(e) => return Err(load_failure(&e)),
        };
        let Some(id) = plugin.plugin().origin().id().cloned() else {
            return Err(ServerError::NoPluginId);
        };
        let descriptor = plugin.plugin().descriptor().clone();
        let loaded = plugin.plugin().loaded().clone();
        let negotiated = plugin.plugin().negotiated(preferred_format);
        self.clock = Clock { format: negotiated };
        self.max_block = block_size;
        // The plugin's own parameter output is held in fixed storage
        // reserved here, before its first block: a queue per
        // parameter, each within a block's chunk room
        // (`ParameterChanges::reserved`).
        self.render.set_format(negotiated);
        self.render.reserve_params(
            plugin.plugin().parameter_count(main),
            ParameterQueue::chunk_room(block_size),
        );
        self.plugin = Some(plugin);
        self.rate_refused = None;
        self.publish();
        self.declare_informs(main);
        if let Err(e) = self.render.send(ToRender::Seat(seat)) {
            // The load is not answered as done: the render thread has no
            // plugin to run, and every block would be refused.
            return Err(ServerError::NotSeated {
                reason: e.to_string(),
            });
        }
        Ok(PluginLoaded {
            id,
            descriptor: Box::new(descriptor),
            loaded: Box::new(loaded),
            negotiated_format: negotiated,
            state_error: state_error.map(StateError::from),
        })
    }
}

impl Handle<SaveState> for Session {
    /// Answer `SaveState` with the state as an object.
    ///
    /// An object because a plugin's state is the one payload whose size the
    /// plugin picks, and a sample-embedding instrument's exceeds what a frame
    /// carries. The empty cases are an empty object: a host waits for the
    /// answer, so answering nothing would hang it rather than tell it there is
    /// no state.
    ///
    /// **A save never silences a block that is playing.** A parameter write
    /// waits in the Processor's ring until a block drains it, so a state saved
    /// right after one could miss it.
    /// - Nothing pending: the state is saved at once.
    /// - A write pending and blocks flowing: a barrier is queued to the render
    ///   thread and the save waits for the next block that reaches the plugin
    ///   to run after it ([`RenderLink::barrier`](crate::render::RenderLink::barrier)), then saves. The Processor
    ///   never leaves the render thread.
    /// - A write pending and no block within [`BLOCK_WAIT`]: the host is quiet,
    ///   so the Processor is brought home for the settle, which is silent
    ///   because nothing is playing.
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        SaveState { state_use }: SaveState,
    ) -> Result<SavedState, ServerError> {
        let settle = self.write_pending && {
            let id = self.render.barrier();
            !self.render.wait_applied(id, BLOCK_WAIT)
        };
        self.write_pending = false;
        let data = match self.plugin.as_mut() {
            Some(p) => match if settle {
                p.plugin_mut().save_state(cx.main, state_use)
            } else {
                p.plugin_mut().save_state_live(cx.main, state_use)
            } {
                Ok(data) => data,
                // A plugin that fails to serialise is not a transport problem:
                // one `Error` frame, and the host reports it as a rejection.
                Err(e) => {
                    return Err(ServerError::SaveFailed {
                        reason: e.to_string(),
                    });
                }
            },
            None => Vec::new(),
        };
        // Refuse an over-limit state here rather than sending a gigabyte the
        // host is only going to reject.
        if data.len() > MAX_STATE_BYTES {
            return Err(ServerError::StateTooLarge {
                bytes: data.len() as u64,
                limit: MAX_STATE_BYTES as u64,
            });
        }
        Ok(SavedState(data))
    }
}

impl Handle<SetAutomationState> for Session {
    fn handle(&mut self, cx: &mut Handling<'_>, SetAutomationState { mode }: SetAutomationState) {
        self.write_pending = true;
        if let Some(plugin) = self.plugin.as_mut() {
            plugin.plugin_mut().set_automation_state(cx.main, mode);
        }
    }
}

impl Handle<SetTrackInfo> for Session {
    fn handle(&mut self, cx: &mut Handling<'_>, SetTrackInfo { info }: SetTrackInfo) {
        if let Some(plugin) = self.plugin.as_mut() {
            plugin.plugin_mut().set_track_info(cx.main, &info);
        }
    }
}

impl Handle<SetAppMidiMap> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        SetAppMidiMap { app }: SetAppMidiMap,
    ) -> Result<AppMidiMapApplied, ServerError> {
        let applied = self.plugin.as_mut().map_or(MidiMapApplied::Host, |p| {
            p.plugin_mut().set_app_midi_map(cx.main, &app)
        });
        Ok(AppMidiMapApplied { applied })
    }
}

impl Handle<GetWiring> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetWiring: GetWiring,
    ) -> Result<Wiring, ServerError> {
        // With no plugin the answer is the refusal, which the host reads as
        // "no wiring" and keeps the one it has.
        match self.plugin.as_mut() {
            Some(p) => Ok(Wiring {
                wiring: p.plugin_mut().wiring(cx.main),
            }),
            None => Err(ServerError::NoPlugin),
        }
    }
}

impl Handle<ArmPluginMidiLearn> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        ArmPluginMidiLearn { armed }: ArmPluginMidiLearn,
    ) -> Result<PluginMidiLearn, ServerError> {
        let result = match self.plugin.as_mut() {
            Some(p) => p.plugin_mut().arm_plugin_midi_learn(cx.main, armed),
            None => Err(PluginLearnError::Lost),
        };
        Ok(PluginMidiLearn { result })
    }
}

impl Handle<GetParameter> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetParameter { param_id }: GetParameter,
    ) -> Result<ParameterValue, ServerError> {
        let value = self
            .plugin
            .as_ref()
            .and_then(|p| p.plugin().parameter(cx.main, param_id));
        Ok(ParameterValue { value })
    }
}

impl Handle<GetParameterList> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetParameterList: GetParameterList,
    ) -> Result<ParameterList, ServerError> {
        let parameters = self
            .plugin
            .as_ref()
            .map(|p| p.plugin().parameters(cx.main))
            .unwrap_or_default();
        Ok(ParameterList { parameters })
    }
}

impl Handle<GetPresetList> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetPresetList: GetPresetList,
    ) -> Result<PresetList, ServerError> {
        // `unwrap_or_default` — an empty list, matching
        // `GetParameterList` above. A request arriving before a plugin
        // is loaded is a host sequencing error, not something to report
        // as a preset-less plugin, and the same shape already covers it
        // for parameters.
        let presets = self
            .plugin
            .as_mut()
            .map(|p| p.plugin_mut().presets(cx.main))
            .unwrap_or_default();
        Ok(PresetList { presets })
    }
}

impl Handle<LoadPreset> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        LoadPreset { id }: LoadPreset,
    ) -> Result<PresetLoaded, ServerError> {
        self.write_pending = true;
        self.forget_informs();
        let ok = self
            .plugin
            .as_mut()
            .is_some_and(|p| p.plugin_mut().load_preset(cx.main, &id));
        self.forget_informs();
        Ok(PresetLoaded { ok })
    }
}

impl Handle<GetCurrentPreset> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetCurrentPreset: GetCurrentPreset,
    ) -> Result<CurrentPreset, ServerError> {
        let id = self
            .plugin
            .as_mut()
            .and_then(|p| p.plugin_mut().current_preset(cx.main));
        Ok(CurrentPreset { id })
    }
}

impl Handle<GetParameterPlain> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetParameterPlain { param_id, value }: GetParameterPlain,
    ) -> Result<ParameterPlain, ServerError> {
        let plain = self
            .plugin
            .as_ref()
            .and_then(|p| p.plugin().parameter_plain(cx.main, param_id, value));
        Ok(ParameterPlain { plain })
    }
}

impl Handle<GetParameterNormalized> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetParameterNormalized { param_id, plain }: GetParameterNormalized,
    ) -> Result<ParameterNormalized, ServerError> {
        let value = self
            .plugin
            .as_ref()
            .and_then(|p| p.plugin().parameter_normalized(cx.main, param_id, plain));
        Ok(ParameterNormalized { value })
    }
}

impl Handle<GetParameterText> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetParameterText { param_id, value }: GetParameterText,
    ) -> Result<ParameterText, ServerError> {
        let text = self
            .plugin
            .as_ref()
            .and_then(|p| p.plugin().parameter_text(cx.main, param_id, value));
        Ok(ParameterText { text })
    }
}

impl Handle<GetParameterValueFromText> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        GetParameterValueFromText { param_id, text }: GetParameterValueFromText,
    ) -> Result<ParameterValueFromText, ServerError> {
        // Reads as a query, but is not one on every format: VST2's
        // `effString2Parameter` writes the parsed value into the plugin
        // as it reads it. The trait takes `&self` because the plugin
        // handle is interior-mutable at the FFI edge, so this stays on
        // the `as_ref` path with the other queries.
        let value = self.plugin.as_ref().and_then(|p| {
            p.plugin()
                .parameter_value_from_text(cx.main, param_id, &text)
        });
        Ok(ParameterValueFromText { value })
    }
}

impl Handle<SetSampleRate> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        SetSampleRate { rate }: SetSampleRate,
    ) -> Result<SampleRateSet, ServerError> {
        // A rate no clock can run at is refused by the wire itself: a
        // `SampleRate` does not decode from one.
        // A refusal leaves it silent (`handle_process` publishes nothing) and
        // the host is told, because a rolled-back plugin would
        // otherwise render at its old rate against the host's blocks.
        let refusal = match self.plugin.as_mut() {
            Some(plugin) => match plugin.plugin_mut().set_sample_rate(cx.main, rate) {
                Ok(()) => None,
                Err(e) => {
                    tracing::warn!(requested = %rate, "sample rate not applied: {e}");
                    Some(e.cause)
                }
            },
            None => None,
        };
        self.rate_refused = refusal.clone();
        Ok(SampleRateSet { rate, refusal })
    }
}

impl Handle<SetRenderMode> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        SetRenderMode { mode }: SetRenderMode,
    ) -> Result<RenderModeSet, ServerError> {
        // A server with no plugin has nothing to tell the mode to, as a
        // plugin not yet prepared has not: it did not take it.
        let outcome = match self.plugin.as_mut() {
            Some(plugin) => plugin.plugin_mut().set_render_mode(cx.main, mode),
            None => ModeOutcome::Refused,
        };
        if outcome == ModeOutcome::Refused {
            tracing::debug!(?mode, "plugin did not accept the render mode");
        }
        Ok(RenderModeSet { mode, outcome })
    }
}

impl Handle<OpenEditor> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        OpenEditor { parent, scale }: OpenEditor,
    ) -> Result<EditorOpened, ServerError> {
        let result = self
            .open_editor(cx.main, parent, scale)
            .map_err(editor_wire_error);
        Ok(EditorOpened { result })
    }
}

impl Handle<CloseEditor> for Session {
    fn handle(&mut self, cx: &mut Handling<'_>, CloseEditor: CloseEditor) {
        self.close_editor(cx.main);
    }
}

impl Handle<SetEditorSize> for Session {
    fn handle(
        &mut self,
        cx: &mut Handling<'_>,
        SetEditorSize { size }: SetEditorSize,
    ) -> Result<EditorSized, ServerError> {
        let result = self
            .set_editor_size(cx.main, size)
            .map_err(editor_wire_error);
        Ok(EditorSized { result })
    }
}

// The transport delivers a state with its object as `Incoming::LoadState` and
// `Incoming::StageState`, a slab as `Incoming::SetupSlab` and a lane's end as
// `Incoming::OpenAudio`; a frame that reaches a handler has none.
impl Handle<LoadState> for Session {
    fn handle(
        &mut self,
        _: &mut Handling<'_>,
        _: LoadState,
    ) -> Result<tutti_plugin_ipc::StateLoaded, ServerError> {
        Err(ServerError::StateWithoutObject)
    }
}

impl Handle<StageState> for Session {
    fn handle(
        &mut self,
        _: &mut Handling<'_>,
        _: StageState,
    ) -> Result<tutti_plugin_ipc::StateStaged, ServerError> {
        Err(ServerError::StateWithoutObject)
    }
}

impl Handle<SetupSharedMemory> for Session {
    fn handle(
        &mut self,
        _: &mut Handling<'_>,
        _: SetupSharedMemory,
    ) -> Result<tutti_plugin_ipc::SharedMemoryReady, ServerError> {
        Err(ServerError::SlabSetupWithoutSlab)
    }
}

impl Handle<OpenLane> for Session {
    fn handle(
        &mut self,
        _: &mut Handling<'_>,
        _: OpenLane,
    ) -> Result<tutti_plugin_ipc::LaneOpened, ServerError> {
        Err(ServerError::LaneWithoutEnd)
    }
}

impl Handle<Shutdown> for Session {
    fn handle(&mut self, _: &mut Handling<'_>, Shutdown: Shutdown) -> Stop {
        Stop
    }
}
