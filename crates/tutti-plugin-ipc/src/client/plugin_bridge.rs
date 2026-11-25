//! Plugin bridge — a served plugin's audio and its editor.

use super::audio::{AudioBridge, IpcThread, Replies};
use super::editor_site::EditorSite;
use crate::ControlStream;
use crate::ServerWatch;
use std::sync::Arc;
use tutti_platform::Executor;
use tutti_plugin_types::{Delivered, Editor, IpcResult, PluginFormat, PluginRef, Timeouts};
use tutti_plugin_types::{MainThreadMarker, MainWorkHandle, StateError};
use tutti_units::{Normalized, ParamId, SampleRate};

/// A served plugin's audio bridge and its editor: the two parts a host drives,
/// each reached through its own accessor ([`audio`](Self::audio),
/// [`editor`](Self::editor)). The calls that touch both are the bridge's own:
/// a parameter or automation write goes to the audio instance and is shown on
/// the editor, and a state load reaches both.
///
/// Audio processing goes through the [`AudioBridge`] (IPC to the
/// plugin-server child process). The editor is in-process (loaded on the
/// first open) or, for a bridge made with no [`EditorLoader`], in the server.
pub struct PluginBridge {
    audio: AudioBridge,
    editor: EditorSite,
}

impl std::fmt::Debug for PluginBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBridge")
            .field("audio", &self.audio)
            .field("editor", &self.editor)
            .finish()
    }
}

/// What loads the editor-only instance of the plugin `load` chooses in the file
/// at a path, in this process (`OutOfProcess::load_editor` for the file's
/// format, which the load names), and the collector of the plugin's owner that instance hands its close
/// to. The host passes its own: which formats it was built with is the host's, not
/// the bridge's.
#[derive(Clone)]
pub struct EditorLoader {
    load: EditorLoad,
    work: MainWorkHandle,
}

/// How an [`EditorLoader`] makes an editor: the work collector, the plugin (its
/// file and the id chosen in it) and the format that opens it.
type EditorLoad =
    fn(MainThreadMarker, &MainWorkHandle, &PluginRef, PluginFormat) -> IpcResult<Box<dyn Editor>>;

impl EditorLoader {
    /// A loader that makes the editor instance with `load`, which hands its close
    /// to `work`.
    #[must_use]
    pub fn new(load: EditorLoad, work: MainWorkHandle) -> Self {
        Self { load, work }
    }

    pub(super) fn load(
        &self,
        main: MainThreadMarker,
        plugin: &PluginRef,
        format: PluginFormat,
    ) -> IpcResult<Box<dyn Editor>> {
        (self.load)(main, &self.work, plugin, format)
    }
}

impl std::fmt::Debug for EditorLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorLoader").finish_non_exhaustive()
    }
}

impl PluginBridge {
    /// Build the full bridge — spawns the audio bridge thread that speaks to the
    /// plugin-server on its `audio` lane (and starts the control lane's thread on
    /// `control`, so a block never waits behind a control request) and leaves the
    /// GUI instance to be lazily loaded on first `open_editor`.
    ///
    /// Returns an `Arc<Self>` (for cheap cloning into the audio graph), a
    /// `IpcThread` (whose `Drop` shuts down the bridge thread) and the
    /// [`Replies`] the pipeline reads the plugin's replies from, the ring's
    /// one consumer.
    ///
    /// `sample_rate` seeds the bridge thread's per-block reply timeout (see
    /// `dispatch::process_timeout`); `set_sample_rate` keeps it current after
    /// device rate changes. The audio thread itself sizes nothing from the rate
    /// any more — it does not wait.
    ///
    /// # Errors
    ///
    /// The bridge thread cannot be spawned, or `timeouts` has a zero deadline.
    pub fn new(
        executor: &Executor,
        audio: ControlStream,
        control: ControlStream,
        origin: PluginRef,
        format: PluginFormat,
        load_editor: Option<EditorLoader>,
        sample_rate: SampleRate,
        watch: Arc<ServerWatch>,
        timeouts: Timeouts,
    ) -> IpcResult<(Arc<Self>, IpcThread, Replies)> {
        timeouts.validate()?;
        let (audio, bridge_thread, replies) =
            AudioBridge::new(executor, audio, control, sample_rate, watch, timeouts)?;
        let editor = EditorSite::new(&audio, origin, format, load_editor);
        Ok((Arc::new(Self { audio, editor }), bridge_thread, replies))
    }

    /// The audio instance's bridge.
    pub fn audio(&self) -> &AudioBridge {
        &self.audio
    }

    /// Where the editor lives, and its calls.
    pub fn editor(&self) -> &EditorSite {
        &self.editor
    }

    /// Stores a parameter write for the plugin, sent with its next block, and
    /// shows it on an open editor. A control-side call: it takes a lock.
    ///
    /// # Errors
    ///
    /// [`ParamWriteError::Undeclared`](tutti_plugin_types::ParamWriteError::Undeclared)
    /// when the plugin does not declare `param_id`;
    /// [`Lost`](tutti_plugin_types::ParamWriteError::Lost) when it crashed and
    /// [`Unanswered`](tutti_plugin_types::ParamWriteError::Unanswered) when it
    /// gave no parameter list: nothing is stored.
    pub fn set_parameter_value(
        &self,
        param_id: ParamId,
        value: Normalized,
    ) -> Result<(), tutti_plugin_types::ParamWriteError> {
        // Cosmetic GUI mirror (keeps the display in sync): queued for the next
        // `editor_idle`, a bounded lock-free queue that never blocks. The
        // authoritative delivery is the slot below; a dropped mirror write
        // self-heals on the next edit.
        self.editor.show_parameter(param_id, value);
        self.audio.write_parameter(param_id, value)
    }

    /// Sends the host's automation mode to the plugin, mirrored into an open
    /// editor. Realtime-safe; says whether it was delivered.
    pub fn set_automation_state_rt(&self, mode: tutti_plugin_types::AutomationMode) -> Delivered {
        // Deliver to BOTH the audio subprocess AND the in-process GUI instance
        // (mirrors `set_parameter_value`). The automation-state advisory drives
        // editor UI feedback (a glowing knob ring), which lives in the GUI
        // instance — so a GUI-only delivery would leave it unlit. The audio
        // instance also receives it for formats that gate DSP on it. Both sides
        // take the format-neutral `AutomationMode` and encode it at their own
        // ABI edge (the GUI instance and the server loader) — no format bitmask
        // here.
        //
        // The GUI mirror is cosmetic and a main-thread call, so the latest mode
        // waits for the next `editor_idle` (one store, never block); a dropped
        // glow self-heals on the next mode change.
        self.editor.show_automation(mode);
        self.audio.set_automation_state_rt(mode)
    }

    /// Loads `data` into the audio instance and the editor instance.
    ///
    /// The **audio** instance's answer is the one returned.
    ///
    /// The GUI instance is a second dlopen of the same plugin kept only so its
    /// editor displays the right values; a failure to mirror there leaves the
    /// editor stale but the audio correct, which is not what a caller asking
    /// "did my preset load" is asking about. Mirroring stays best-effort, and
    /// the audio result is the return value.
    ///
    /// # Errors
    ///
    /// As `AudioBridge::load_state`.
    pub fn load_state(
        &self,
        main: MainThreadMarker,
        data: &[u8],
        state_use: tutti_plugin_types::StateUse,
    ) -> std::result::Result<(), StateError> {
        let audio = self.audio.load_state(data, state_use);
        // Also load into GUI instance so its display stays in sync.
        self.editor.mirror_state(main, data);
        audio
    }

    /// Loads `data` into the in-process editor instance alone, best effort, so
    /// its display follows a state the audio instance took
    /// ([`AudioBridge::load_state_answer`]).
    pub fn mirror_state(&self, main: MainThreadMarker, data: &[u8]) {
        self.editor.mirror_state(main, data);
    }
}
