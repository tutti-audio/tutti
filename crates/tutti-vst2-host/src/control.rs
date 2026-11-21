//! The control surface of a VST2 plugin in the shared vocabulary, implemented by
//! [`Vst2Loaded`] and, through it, by [`Vst2Controller`](crate::Vst2Controller).

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_plugin_types::MainThreadMarker;
use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, EditorPresence, EditorWindow, Features, KeyEvent,
    KeyUse, LoadedPlugin, ModifierConvention, ParamAccess, ParamEdit, ParamWriteError,
    ParameterInfo, PluginBus, PluginBuses, PluginClass, PluginDescriptor, PluginError,
    PluginResult, Preset, PresetId, RenderMode, RenderModeSupport, Request, StateUse, Widths,
    WindowHandle,
};
use tutti_units::Normalized;
use tutti_units::ParamId;

use crate::error::Vst2Error;
use crate::host::ParameterChange;
use crate::keys::key_code_of;
use crate::loaded::Vst2Loaded;
use crate::types::PluginInfo;
use keyboard_types::KeyState;

/// An error and its sources, as one line: the shared vocabulary carries text.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A `Vst2Error` as the shared vocabulary's error.
pub(crate) fn plugin_error(error: Vst2Error) -> PluginError {
    match error {
        Vst2Error::LoadFailed { stage, reason, .. } => PluginError::Load { stage, reason },
        Vst2Error::Loader { stage, source, .. } => PluginError::Load {
            stage,
            reason: error_chain(&source),
        },
        Vst2Error::StateFrame(frame) => PluginError::State(frame.to_string()),
        Vst2Error::NoEditor | Vst2Error::EditorOpenFailed | Vst2Error::NoEditorSize { .. } => {
            PluginError::Editor(tutti_plugin_types::EditorError::PluginError(
                error.to_string(),
            ))
        }
        state @ (Vst2Error::ChunkSave(_)
        | Vst2Error::NoGetParameter { .. }
        | Vst2Error::TooManyParameters { .. }
        | Vst2Error::ParameterOutOfRange { .. }
        | Vst2Error::NoSetParameter { .. }) => PluginError::State(error_chain(&state)),
    }
}

/// The catalog descriptor of a plugin as loaded.
#[must_use]
pub(crate) fn descriptor_of(info: &PluginInfo) -> PluginDescriptor {
    PluginDescriptor {
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        class: PluginClass::Vst2 {
            category: info.category,
        },
        editor: EditorPresence::measured(info.has_editor),
        measured: None,
        widths: Some(Widths::new(info.supports_f64)),
    }
}

/// The main bus of `channels` placed at `kept`, or no bus for no channels.
fn placed(channels: ChannelLayout, kept: Option<&ChannelTopology>) -> PluginBuses {
    PluginBus::single(channels)
        .into_iter()
        .map(|bus| bus.placed(kept.cloned()))
        .collect()
}

impl Vst2Loaded {
    /// The capabilities this plugin has, as the shared vocabulary names them.
    fn features(&self) -> Features {
        let info = self.metadata();
        let mut features = Features::empty();
        // Backed by the render path: with this bit set, an `F64` buffer reaches
        // `processReplacingF64` (an activation at `F64` tells the plugin
        // `effSetProcessPrecision(64)`) rather than being narrowed.
        features.set(Features::F64_AUDIO, info.supports_f64);
        features.set(Features::MIDI_IN, info.receives_midi);
        // MIDI-out is the plugin's declared output-bus count, not the combined
        // input flag — the host drains only what the plugin emits.
        features.set(Features::MIDI_OUT, info.emits_midi);
        features.set(Features::EDITOR, info.has_editor);
        // VST2 always gets a transport snapshot (`audioMasterGetTime`). No
        // sample-accurate automation, note-expression, sequencer context, or
        // host-driven editor resize (fused AEffect editor, no sizeWindow).
        features.insert(Features::TRANSPORT);
        // The host answers the process level, so every plugin takes the mode.
        features.insert(Features::RENDER_MODE);
        // The host answers `audioMasterGetAutomationState` for every plugin.
        features.insert(Features::AUTOMATION_STATE);
        features
    }
}

impl Control for Vst2Loaded {
    fn info(&self) -> &PluginDescriptor {
        self.descriptor()
    }

    /// The wiring read from the plugin now: its pin counts, latency and tail are
    /// asked of the `AEffect` and the dispatcher, not remembered from load.
    fn wiring(&self, main: MainThreadMarker) -> LoadedPlugin {
        let (inputs, outputs) = self.pins();
        // The arrangements the plugin kept after `negotiate_arrangement`; a bus reads one
        // through `placement`, which is `None` where its width is not the pin count.
        let kept = self.kept.as_ref();
        LoadedPlugin {
            // VST2 is single-bus: one main input bus, one main output bus. Before a
            // negotiation a bus has no placement: `effGetSpeakerArrangement` is asked only
            // of a plugin the host has proposed an arrangement to.
            inputs: placed(inputs, kept.and_then(|k| k.input.as_ref())),
            outputs: placed(outputs, kept.and_then(|k| k.output.as_ref())),
            latency_samples: self.latency_samples(),
            // Asked and decoded: VST2 inverts the tail convention, so `0` is "no
            // information" where every other format means "no tail" (see
            // `PluginInfo`). `Unknown` remains the answer for a plugin that
            // declines the opcode.
            tail: self.tail(main),
            features: self.features(),
            probed: probed::VST2,
            // VST2 has no opcode that reads the bypass back, so the host cannot
            // confirm a write of `effSetBypass`: the executor's bypass serves it.
            own_bypass: None,
            assignments: Vec::new(),
        }
    }

    fn parameters(&self, _main: MainThreadMarker) -> Vec<ParameterInfo> {
        self.parameter_list()
    }

    /// Normalized `0..=1`, VST2's own domain. `None` when `id` addresses no
    /// declared parameter, or the plugin exposes no `getParameter`, which VST 2.4
    /// permits for a plugin declaring no parameters: that is not a parameter at
    /// zero.
    fn parameter(&self, _main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
        self.parameter_value(id)
    }

    /// Applied now, with `setParameter`, which the SDK allows from either thread.
    /// A write to a plugin that exposes no `setParameter` is discarded.
    fn set_parameter(
        &mut self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Result<(), ParamWriteError> {
        self.parameter_writable(main, id)?;
        let _ = self.write_parameter(id, value);
        Ok(())
    }

    /// Declared when `id` addresses a parameter. VST2 has no opcode for a
    /// read-only one, so a declared parameter is writable.
    fn parameter_writable(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
    ) -> Result<(), ParamWriteError> {
        ParamWriteError::check(id, self.param_index(id).map(|_| ParamAccess::Unreported))
    }

    fn parameter_plain(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64> {
        Vst2Loaded::parameter_plain(self, id, value)
    }

    fn parameter_normalized(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        Vst2Loaded::parameter_normalized(self, id, plain)
    }

    /// The plugin's display string, but **only for the value it currently
    /// holds**.
    ///
    /// `effGetParamDisplay` passes the plugin an index and nothing else, so it
    /// formats its own current value; VST 2.4 has no call that formats an
    /// arbitrary one. So this answers when `value` is the value the plugin is
    /// already at, and `None` otherwise. Setting the parameter in order to read
    /// its label would make a display query *audible* and would race whatever
    /// else is writing that parameter — a text lookup must not move a knob.
    /// Returning the current value's string regardless would be worse still: it
    /// is the wrong answer, presented as the right one, and a caller comparing
    /// two values would see the same label for both.
    ///
    /// The comparison is on the `f32` word: the plugin holds the narrowing of the
    /// `f64` written, so `value` is current when it narrows to the word that
    /// `parameter` reads back, and a value a whole `f32` step away is not one the
    /// plugin can describe.
    fn parameter_text(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String> {
        self.display_if_current(id, value)
    }

    /// Parse `text` through the plugin's own `effString2Parameter`.
    ///
    /// **This writes.** The VST2 opcode is a setter with no parse-only
    /// counterpart, so the parameter lands on the parsed value as a side effect
    /// of asking. That is a real divergence from the other three formats, where
    /// this method is a pure query; it is surfaced here rather than hidden
    /// because the alternative — refusing to implement it — would leave a user
    /// unable to type a value into a VST2 field at all. The value is read back
    /// after the write, since the opcode reports only acceptance.
    fn parameter_value_from_text(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.set_parameter_from_string(id, text)
    }

    fn save_state(&mut self, main: MainThreadMarker, _: StateUse) -> PluginResult<Vec<u8>> {
        self.read_state(main).map_err(plugin_error)
    }

    fn load_state(
        &mut self,
        main: MainThreadMarker,
        bytes: &[u8],
        _: StateUse,
    ) -> PluginResult<()> {
        self.write_state(main, bytes).map_err(plugin_error)
    }

    /// VST2 programs. The index **is** the identifier — `effProgramChange` takes
    /// a position in `[0, numPrograms)` — so unlike AU's sparse selectors these
    /// are genuinely dense. Unnamed slots are kept: dropping one would renumber
    /// every program after it.
    fn presets(&mut self, main: MainThreadMarker) -> Vec<Preset> {
        self.programs(main)
            .into_iter()
            .map(|(index, name)| Preset::new(PresetId::Number(index), name))
            .collect()
    }

    /// Switch program, bracketed by `effBeginSetProgram`/`effEndSetProgram`.
    /// `false` for an id this format cannot address.
    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        match id.number() {
            Some(index) => self.set_program(main, index),
            None => false,
        }
    }

    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId> {
        Some(PresetId::Number(self.current_program(main)))
    }

    /// Takes the requests the plugin raised since the last call, in the order
    /// [`taken`](tutti_plugin_types::taken) yields them, into `out`.
    ///
    /// VST 2.4 has one notice, `audioMasterIOChanged`, for the pins, the latency
    /// and the tail alike, so it raises [`Request::IoChanged`],
    /// [`Request::LatencyChanged`] and [`Request::TailChanged`] together;
    /// `audioMasterUpdateDisplay` raises a rescan of the parameters' values and
    /// of their names. Feed them to [`Lifecycle::drive`](tutti_plugin_types::Lifecycle::drive);
    /// the core restarts the plugin for the I/O change
    /// ([`RestartRules::VST2`](tutti_plugin_types::RestartRules::VST2)) and reads the latency
    /// and the tail again.
    fn take_requests(&mut self, _main: MainThreadMarker, out: &mut Vec<Request>) {
        // Control side, so the one warning about a plugin that grew its pins past the
        // activation (it renders silence until the restart its `audioMasterIOChanged` asks for)
        // is logged here.
        self.instance.report_pin_refusals();
        out.extend(tutti_plugin_types::taken(self.host.requests()));
    }

    /// `Live`: the plugin reads the mode by asking the host
    /// (`audioMasterGetCurrentProcessLevel`), whose answer changes at once.
    fn render_mode_support(&self, _main: MainThreadMarker) -> RenderModeSupport {
        RenderModeSupport::Live
    }

    /// Sets whether the host reports itself as rendering offline.
    ///
    /// Unlike the other three formats there is nothing to push: VST2 carries
    /// this through `audioMasterGetCurrentProcessLevel`, a callback the plugin
    /// makes whenever it likes. So this stores the answer the host will give,
    /// and no plugin can decline it: there is no query to refuse. That is also
    /// why a render-mode change needs no restart here, and the answer is always
    /// `true`. The store is atomic: the plugin may ask from its audio thread
    /// while the control thread sets it.
    fn set_render_mode(&mut self, _main: MainThreadMarker, mode: RenderMode) -> bool {
        self.set_offline_render(mode.is_offline());
        true
    }

    /// Stores the mode the plugin is told: `audioMasterGetAutomationState` answers it from
    /// now on, and the time info of each block carries it in its automation flags. VST2
    /// has no push to the plugin, so there is nothing to refuse. The store is atomic: the
    /// plugin may ask from its audio thread while the control thread sets it.
    fn set_automation_state(&mut self, _main: MainThreadMarker, mode: AutomationMode) -> bool {
        self.host.set_automation(mode);
        true
    }

    /// The reports of `audioMasterBeginEdit`, `audioMasterAutomate` and
    /// `audioMasterEndEdit` (the plugin's own editor moving a knob), oldest
    /// first, as [`ParamEdit`]s. A report naming a negative index names no
    /// parameter and is dropped. The queue the reports arrive in drops rather
    /// than growing when nobody drains it:
    /// [`dropped_param_changes`](Vst2Loaded::dropped_param_changes) counts them.
    fn take_reported_edits(&mut self, _main: MainThreadMarker, out: &mut Vec<ParamEdit>) {
        while let Some(report) = self.param_rx.pop() {
            let id = |index: i32| u32::try_from(index).ok().map(ParamId::new);
            let edit = match report {
                ParameterChange::Begin(i) => id(i).map(ParamEdit::Begin),
                ParameterChange::End(i) => id(i).map(ParamEdit::End),
                ParameterChange::Automate(i, value) => id(i).map(|id| ParamEdit::change(id, value)),
            };
            out.extend(edit);
        }
    }

    /// The reports the bounded queue refused ([`take_reported_edits`](Control::take_reported_edits)).
    fn dropped_reported_edits(&self) -> u64 {
        self.dropped_param_changes().get()
    }
}

impl EditorWindow for Vst2Loaded {
    fn has_editor(&self, _main: MainThreadMarker) -> bool {
        self.instance.has_editor()
    }

    /// `scale` is ignored: VST2 has no content-scale call, the fused `AEffect`
    /// editor sizes itself in the parent's pixels. The size it opens at is in the
    /// unit of an `ERect` ([`EditorUnit::WINDOWING`](tutti_plugin_types::EditorUnit::WINDOWING): pixels, or points on macOS).
    fn open_editor(
        &mut self,
        main: MainThreadMarker,
        parent: WindowHandle,
        _scale: ContentScale,
    ) -> PluginResult<tutti_plugin_types::EditorOpened<dpi::Size>> {
        self.open_editor_in(main, parent)
            .map(tutti_plugin_types::EditorOpened::new)
            .map_err(plugin_error)
    }

    /// Forwards the key as `effEditKeyDown` or `effEditKeyUp` and reports the plugin's 1 as
    /// [`KeyUse::Used`]. A key VST2 cannot write (a character outside ASCII, a modifier with
    /// no bit) is not sent and is [`KeyUse::Ignored`], as is any key while no editor is open.
    fn editor_key(&mut self, main: MainThreadMarker, event: KeyEvent) -> PluginResult<KeyUse> {
        let Some(code) = key_code_of(&event, ModifierConvention::HOST) else {
            return Ok(KeyUse::Ignored);
        };
        let used = match event.state {
            KeyState::Down => self.instance.editor_key_down(main, code),
            KeyState::Up => self.instance.editor_key_up(main, code),
        };
        Ok(if used { KeyUse::Used } else { KeyUse::Ignored })
    }

    fn close_editor(&mut self, main: MainThreadMarker) {
        self.instance.close_editor(main);
    }

    fn editor_idle(&mut self, main: MainThreadMarker) {
        self.instance.idle_editor(main);
    }

    /// The default capabilities: this surface reports no resize support.
    fn editor_capabilities(
        &mut self,
        _main: tutti_plugin_types::MainThreadMarker,
    ) -> tutti_plugin_types::EditorCapabilities {
        tutti_plugin_types::EditorCapabilities::default()
    }

    /// Not supported here.
    fn set_editor_size(
        &mut self,
        _main: tutti_plugin_types::MainThreadMarker,
        _requested: dpi::Size,
    ) -> PluginResult<dpi::Size> {
        Err(tutti_plugin_types::PluginError::Editor(
            tutti_plugin_types::EditorError::PluginError("set_editor_size not supported".into()),
        ))
    }

    /// The size the plugin asked the window to take with `audioMasterSizeWindow` since the
    /// last poll, if any (`aeffectx.h`: `[index]` width, `[value]` height).
    fn poll_editor_resize_request(
        &mut self,
        _main: tutti_plugin_types::MainThreadMarker,
    ) -> Option<dpi::Size> {
        self.host.take_size_request()
    }
}
