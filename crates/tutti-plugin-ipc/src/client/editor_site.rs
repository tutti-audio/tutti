//! Where a served plugin's editor lives.
//!
//! An editor is in one of two places, and [`EditorSite`] is the one place that
//! knows which. CLAP, VST3 and AU keep an editor-only second instance in the
//! host, which the host's [`EditorLoader`] loads on the first open
//! (`InProcessEditor`); a bridge made with no loader opens the editor on the
//! served instance in the server (`ServerEditor`). Every editor call is one
//! `match` on the site.

use super::audio::AudioBridge;
use super::ipc_editor::IpcEditor;
use super::plugin_bridge::EditorLoader;
use crossbeam_queue::ArrayQueue;
use dpi::Size;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_plugin_types::AutomationMode;
use tutti_plugin_types::{
    ContentScale, Editor, EditorCapabilities, EditorError, EditorMirrorError, EditorOpened,
    EditorWindow, MainThreadMarker, ParamEdit, PluginError, PluginFormat, PluginRef, Request,
    StateError, WindowHandle,
};
use tutti_rt::{LockExt, PackedAtomic, Tally, TallyWriter, tally};
use tutti_units::{Normalized, ParamId};

/// Where a served plugin's editor lives: an editor-only instance in this
/// process, or the served instance's own editor in the server.
#[derive(Debug)]
pub struct EditorSite {
    site: Site,
}

/// The two places an editor lives. The variants are boxed because the
/// audio-bridge handles they hold differ in size by more than clippy's
/// `large_enum_variant` allows on Windows.
#[derive(Debug)]
enum Site {
    InProcess(Box<InProcessEditor>),
    Server(Box<ServerEditor>),
}

/// The editor-only instance in this process: loaded with the [`EditorLoader`]
/// on the first open, kept in step with the audio instance's state, and shown
/// what the realtime setters wrote.
pub(crate) struct InProcessEditor {
    audio: AudioBridge,
    /// The file and the plugin in it, which the instance loads (the one the
    /// audio instance is).
    origin: PluginRef,
    /// The format that opens `origin`'s file.
    format: PluginFormat,
    /// Loads the instance on the first open: the host's table of formats,
    /// which this crate does not depend on.
    loader: EditorLoader,
    gui: Mutex<Option<Box<dyn Editor>>>,
    /// The editor's state when it was last in step with the audio instance:
    /// what [`pull_editor_state`] compares against. Read and written with the
    /// `gui` lock held.
    baseline: Mutex<Option<Vec<u8>>>,
    /// What the realtime setters want the editor to show, applied by the next
    /// [`editor_idle`](EditorSite::editor_idle).
    mirror: GuiMirror,
    /// Requests the editor instance made that were dropped, ever.
    requests_dropped: TallyWriter<u64>,
}

/// The editor in the server: every window call is a message to it.
pub(crate) struct ServerEditor {
    audio: AudioBridge,
    remote: Mutex<IpcEditor>,
}

impl std::fmt::Debug for InProcessEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessEditor")
            .field("plugin_path", &self.origin.path())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ServerEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerEditor").finish_non_exhaustive()
    }
}

impl EditorSite {
    /// The site for `audio`'s plugin: the in-process editor `loader` loads for
    /// `origin` as `format`, or with no loader the editor in the server.
    pub(crate) fn new(
        audio: &AudioBridge,
        origin: PluginRef,
        format: PluginFormat,
        loader: Option<EditorLoader>,
    ) -> Self {
        let site = match loader {
            Some(loader) => Site::InProcess(Box::new(InProcessEditor {
                audio: audio.clone(),
                origin,
                format,
                loader,
                gui: Mutex::new(None),
                baseline: Mutex::new(None),
                mirror: GuiMirror::new(),
                requests_dropped: tally().writer(),
            })),
            None => Site::Server(Box::new(ServerEditor {
                audio: audio.clone(),
                remote: Mutex::new(IpcEditor::new(audio.clone())),
            })),
        };
        Self { site }
    }

    /// Opens the editor in `parent`, loading the editor-only instance with
    /// the [`EditorLoader`] on the first open and giving it the audio instance's
    /// state.
    ///
    /// The result says whether the editor instance was given the audio
    /// instance's state ([`EditorOpened::mirrored`]); an editor whose mirror
    /// failed still opens, at the plugin's defaults, and the failure is
    /// counted in [`mirror_transfers_refused_cell`](Self::mirror_transfers_refused_cell). A served
    /// editor is the audio instance's own: nothing to mirror.
    ///
    /// # Errors
    ///
    /// The plugin is dead, the editor instance cannot be loaded, or the plugin
    /// refuses to open its editor.
    pub fn open_editor(
        &self,
        main: MainThreadMarker,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> std::result::Result<EditorOpened<Size>, EditorError> {
        match &self.site {
            Site::InProcess(site) => site.open_editor(main, parent, scale),
            Site::Server(site) => site.open_editor(main, parent, scale),
        }
    }

    /// [`open_editor`](Self::open_editor) for a plugin-owned floating window.
    ///
    /// Same lazy load and the same post-open state sync: the editor instance
    /// and its state are the plugin's either way. What differs is only that no
    /// parent goes in and no size comes out.
    ///
    /// # Errors
    ///
    /// As [`open_editor`](Self::open_editor).
    pub fn open_floating_editor(
        &self,
        main: MainThreadMarker,
    ) -> std::result::Result<EditorOpened<()>, EditorError> {
        match &self.site {
            Site::InProcess(site) => site.open_floating_editor(main),
            Site::Server(site) => site.open_floating_editor(main),
        }
    }

    /// Closes the editor, after sending the audio instance what the user changed
    /// in it. `false` when no editor was open.
    pub fn close_editor(&self, main: MainThreadMarker) -> bool {
        match &self.site {
            Site::InProcess(site) => site.close_editor(main),
            Site::Server(site) => site.close_editor(main),
        }
    }

    /// What the editor instance in this process wants the host's main loop to
    /// wait on (its descriptors and timers, added to `out`). A served editor
    /// lives in the server, whose own loop waits on them: nothing here.
    pub fn wakeups(&self, main: MainThreadMarker, out: &mut tutti_plugin_types::Wakeups) {
        if let Site::InProcess(site) = &self.site {
            site.wakeups(main, out);
        }
    }

    /// Hands the editor instance in this process what a wait on
    /// [`wakeups`](Self::wakeups) found.
    pub fn wake(
        &self,
        main: MainThreadMarker,
        ready: &[tutti_plugin_ipc_ffi::Ready],
        now: tutti_platform::Instant,
    ) {
        if let Site::InProcess(site) = &self.site {
            site.wake(main, ready, now);
        }
    }

    /// The editor's main-thread tick: applies what the realtime setters queued
    /// for it and sends the audio instance the user's edits.
    pub fn editor_idle(&self, main: MainThreadMarker) {
        match &self.site {
            Site::InProcess(site) => site.editor_idle(main),
            Site::Server(site) => site.editor_idle(main),
        }
    }

    /// The open editor's resize capabilities (the default when none is open).
    pub fn editor_capabilities(&self, main: MainThreadMarker) -> EditorCapabilities {
        match &self.site {
            Site::InProcess(site) => site.editor_capabilities(main),
            Site::Server(site) => site.editor_capabilities(main),
        }
    }

    /// Asks the open editor to take `requested`; answers with the size it took.
    ///
    /// # Errors
    ///
    /// No editor is open, or it cannot resize.
    pub fn set_editor_size(
        &self,
        main: MainThreadMarker,
        requested: Size,
    ) -> std::result::Result<Size, EditorError> {
        match &self.site {
            Site::InProcess(site) => site.set_editor_size(main, requested),
            Site::Server(site) => site.set_editor_size(main, requested),
        }
    }

    /// Tells the open editor its window's scale changed. The in-process editor
    /// is a call on the instance and no wire message.
    ///
    /// # Errors
    ///
    /// No editor is open, or the editor's own refusal (`set_editor_scale`).
    pub fn set_editor_scale(
        &self,
        main: MainThreadMarker,
        scale: ContentScale,
    ) -> std::result::Result<(), EditorError> {
        match &self.site {
            Site::InProcess(site) => site.set_editor_scale(main, scale),
            Site::Server(site) => site.set_editor_scale(main, scale),
        }
    }

    /// A size the open editor asked its window to take since the last poll.
    pub fn poll_editor_resize_request(&self, main: MainThreadMarker) -> Option<Size> {
        match &self.site {
            Site::InProcess(site) => site.poll_editor_resize_request(main),
            Site::Server(site) => site.poll_editor_resize_request(main),
        }
    }

    /// The count of transfers between the audio instance and the editor
    /// instance that were refused: parameter writes a full queue refused (the
    /// audio command is unaffected: the mirror only keeps the display
    /// current), user edits the audio instance did not take, and states that
    /// could not be read or loaded either way, each counted once. `None` for a
    /// served editor, which has no mirror.
    pub fn mirror_transfers_refused_cell(&self) -> Option<Tally<u64>> {
        match &self.site {
            Site::InProcess(site) => Some(site.mirror.refused.clone()),
            Site::Server(_) => None,
        }
    }

    /// The count of requests the editor instance made that the host cannot
    /// answer, dropped: every [`Request`] but a transport request (a restart,
    /// an I/O change, a parameter rescan, a latency or tail change, a reload,
    /// a callback, a MIDI-CC or note-expression change, a timer or a file
    /// descriptor). The audio instance cannot be told of them. `None` for a
    /// served editor, whose requests the server raises itself.
    pub fn editor_requests_dropped_cell(&self) -> Option<Tally<u64>> {
        match &self.site {
            Site::InProcess(site) => Some(site.requests_dropped.tally()),
            Site::Server(_) => None,
        }
    }

    /// Queues a parameter write for the in-process editor to show.
    pub(crate) fn show_parameter(&self, id: ParamId, value: Normalized) {
        if let Site::InProcess(site) = &self.site {
            site.mirror.push_param(id, value);
        }
    }

    /// Queues the host's automation mode for the in-process editor to show.
    pub(crate) fn show_automation(&self, mode: AutomationMode) {
        if let Site::InProcess(site) = &self.site {
            site.mirror.push_automation(mode);
        }
    }

    /// Loads `data` into the in-process editor so its display stays in step
    /// with the audio instance. A refusal is counted in
    /// [`mirror_transfers_refused_cell`](Self::mirror_transfers_refused_cell): the audio instance
    /// has the state either way.
    pub(crate) fn mirror_state(&self, main: MainThreadMarker, data: &[u8]) {
        if let Site::InProcess(site) = &self.site
            && let Ok(mut guard) = site.gui.lock()
            && let Some(gui) = guard.as_mut()
            && gui.set_state(main, data).is_err()
        {
            site.mirror.refuse();
        }
    }
}

impl Drop for EditorSite {
    fn drop(&mut self) {
        // Close the editor, then intentionally leak the GUI instance.
        //
        // JUCE plugins crash during their ScopedJuceInitialiser_GUI destructor
        // because Desktop::~Desktop() tries to unregister notification observers
        // that reference already-freed CFString objects. This is a known issue
        // with JUCE-based AU/CLAP plugins: the JUCE static cleanup order is
        // incompatible with being hosted as a dynamic library.
        //
        // Leaking the GUI instance avoids the crash. The OS reclaims all memory
        // on process exit anyway.
        //
        // A drop can run on a worker thread (the graph's editor drops a retired
        // node on the thread that collects the commit, which can be off-main),
        // where there is no token. The editor is closed when the drop happens to
        // be on the main thread; off it, the instance is leaked below with its
        // editor still open, which is what an app that closes the editor first
        // (a main-thread editor system) never meets.
        if let Some(main) = MainThreadMarker::new() {
            self.close_editor(main);
        }
        if let Site::InProcess(site) = &self.site
            && let Ok(mut guard) = site.gui.lock()
            && let Some(gui) = guard.take()
        {
            std::mem::forget(gui);
        }
    }
}

impl ServerEditor {
    /// Runs a call of the editor in the server, its refusal as the
    /// [`EditorError`] the bridge's editor calls answer with.
    fn call<T>(
        &self,
        call: impl FnOnce(&mut IpcEditor) -> tutti_plugin_types::PluginResult<T>,
    ) -> std::result::Result<T, EditorError> {
        let mut remote = self.remote.lock().map_err(|_| EditorError::Busy)?;
        call(&mut remote).map_err(|e| match e {
            PluginError::Editor(typed) => typed,
            other => EditorError::PluginError(other.to_string()),
        })
    }

    fn open_editor(
        &self,
        main: MainThreadMarker,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> std::result::Result<EditorOpened<Size>, EditorError> {
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }
        self.call(|remote| remote.open_editor(main, parent, scale))
    }

    fn open_floating_editor(
        &self,
        main: MainThreadMarker,
    ) -> std::result::Result<EditorOpened<()>, EditorError> {
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }
        self.call(|remote| remote.open_floating_editor(main))
    }

    fn close_editor(&self, main: MainThreadMarker) -> bool {
        let Ok(mut remote) = self.remote.lock() else {
            return false;
        };
        let was_open = remote.is_open();
        remote.close_editor(main);
        was_open
    }

    fn editor_idle(&self, main: MainThreadMarker) {
        if let Ok(mut remote) = self.remote.lock() {
            remote.editor_idle(main);
        }
    }

    fn editor_capabilities(&self, main: MainThreadMarker) -> EditorCapabilities {
        self.remote
            .lock()
            .map(|mut remote| remote.editor_capabilities(main))
            .unwrap_or_default()
    }

    fn set_editor_size(
        &self,
        main: MainThreadMarker,
        requested: Size,
    ) -> std::result::Result<Size, EditorError> {
        self.call(|remote| remote.set_editor_size(main, requested))
    }

    fn set_editor_scale(
        &self,
        main: MainThreadMarker,
        scale: ContentScale,
    ) -> std::result::Result<(), EditorError> {
        self.call(|remote| remote.set_editor_scale(main, scale))
    }

    fn poll_editor_resize_request(&self, main: MainThreadMarker) -> Option<Size> {
        self.remote.lock().ok()?.poll_editor_resize_request(main)
    }
}

impl InProcessEditor {
    /// The editor-only instance in `slot`, loaded with the [`EditorLoader`] if
    /// this is the first open.
    fn instance<'a>(
        &self,
        main: MainThreadMarker,
        slot: &'a mut Option<Box<dyn Editor>>,
    ) -> std::result::Result<&'a mut Box<dyn Editor>, EditorError> {
        match slot {
            Some(gui) => Ok(gui),
            empty => {
                tracing::info!("[bridge] lazy-loading the in-process GUI");
                let instance = self
                    .loader
                    .load(main, &self.origin, self.format)
                    .map_err(|e| {
                        EditorError::PluginError(format!("failed to load in-process GUI: {e}"))
                    })?;
                Ok(empty.insert(instance))
            }
        }
    }

    fn open_editor(
        &self,
        main: MainThreadMarker,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> std::result::Result<EditorOpened<Size>, EditorError> {
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }

        tracing::info!(
            "[bridge] open_editor: acquiring GUI lock for {}",
            self.origin.path().display()
        );
        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;

        // Lazy-load the in-process GUI instance on first open.
        let gui = self.instance(main, &mut guard)?;

        // Sync state from audio (source of truth) to GUI after opening.
        // Some plugins crash if set_state is called before the editor is attached.
        // A state we cannot read means the GUI instance opens at its defaults,
        // which is visibly wrong but not destructive — the audio instance
        // remains the source of truth and nothing is written back. So the
        // open still succeeds (failing it would leave the user with no window
        // at all) and its result carries the failure.
        let state_to_sync = self.audio.save_state(tutti_plugin_types::StateUse::Project);

        tracing::info!("[bridge] open_editor: calling gui.open_editor(parent={parent:?})");
        let size = gui
            .open_editor(main, parent, scale)
            .map_err(|e| EditorError::PluginError(e.to_string()))?
            .window;
        tracing::info!("[bridge] open_editor: editor opened {size:?}");

        // Sync state after editor is open (some plugins need the view attached first).
        let mirrored = self.sync_state(main, gui.as_mut(), state_to_sync);
        self.take_baseline(main, gui.as_mut());
        self.mirror.opened();

        Ok(EditorOpened {
            window: size,
            mirrored,
        })
    }

    /// Gives the editor instance the audio instance's `state` after an open,
    /// counting a failure of either half.
    fn sync_state(
        &self,
        main: MainThreadMarker,
        gui: &mut dyn Editor,
        state: std::result::Result<Vec<u8>, StateError>,
    ) -> std::result::Result<(), EditorMirrorError> {
        let result = match state {
            Ok(state) => {
                tracing::info!(
                    "[bridge] open_editor: syncing {} bytes of state post-open",
                    state.len()
                );
                gui.set_state(main, &state)
                    .map_err(|e| EditorMirrorError::Refused(std::sync::Arc::new(e)))
            }
            Err(e) => Err(EditorMirrorError::Unread(e)),
        };
        if let Err(e) = &result {
            tracing::warn!("[bridge] could not mirror plugin state into the editor: {e}");
            self.mirror.refuse();
        }
        result
    }

    fn open_floating_editor(
        &self,
        main: MainThreadMarker,
    ) -> std::result::Result<EditorOpened<()>, EditorError> {
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }

        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        let gui = self.instance(main, &mut guard)?;

        // Read before the open, as the embedded path does: audio is the source
        // of truth for state, and some plugins refuse `set_state` until their
        // view exists. A failure is in the result, as there.
        let state_to_sync = self.audio.save_state(tutti_plugin_types::StateUse::Project);

        // The editor instance's own open mirrors nothing: its result is the
        // window alone.
        let () = gui
            .open_floating_editor(main)
            .map_err(|e| EditorError::PluginError(e.to_string()))?
            .window;

        let mirrored = self.sync_state(main, gui.as_mut(), state_to_sync);
        self.take_baseline(main, gui.as_mut());
        self.mirror.opened();

        Ok(EditorOpened {
            window: (),
            mirrored,
        })
    }

    /// Sends the audio instance the changes the user made in the editor, and
    /// tells the listener of every edit, a gesture's ends among them.
    fn forward_edits(&self, edits: Vec<ParamEdit>) {
        for edit in edits {
            if let ParamEdit::Change { id, value, .. } = edit {
                // An id the editor reports is the plugin's own; the table
                // follows its list, so a refusal is counted.
                if self.audio.write_parameter(id, value).is_err() {
                    self.mirror.refuse();
                }
            }
            self.audio.report_edit(edit);
        }
    }

    /// Where a request the editor instance made goes. The editor instance is
    /// never active and its audio is the server's, so the requests an `Editor`
    /// yields (`Editor::take_requests`) are the ones the application answers:
    /// a transport request reaches the listener as the server's own do. The
    /// others are the audio instance's to raise, and the server raises them
    /// there; an editor that yielded one would be asking for a restart, a
    /// re-read or a timer of an instance that has none.
    fn route_editor_request(&self, request: Request) {
        match request {
            Request::Transport(asked) => self.audio.report_transport(asked),
            Request::Restart
            | Request::IoChanged
            | Request::RescanParams(_)
            | Request::LatencyChanged
            | Request::TailChanged
            | Request::Reload
            | Request::Callback
            | Request::MidiCcAssignmentChanged
            | Request::NoteExpressionChanged
            | Request::Timer(_)
            | Request::Fd(_) => {
                tracing::warn!(
                    "[bridge] the editor instance asked for {request:?}, which is not its to ask"
                );
                self.requests_dropped.add(1);
            }
        }
    }

    fn close_editor(&self, main: MainThreadMarker) -> bool {
        self.mirror.closed();
        let Ok(mut guard) = self.gui.lock() else {
            return false;
        };
        if let Some(gui) = guard.as_mut() {
            // Flush pending GUI param changes before closing.
            self.forward_edits(gui.poll_edits(main));
            // What the user changed in the editor that is not a parameter
            // reaches the audio instance. Read while the view
            // is still attached.
            self.pull_editor_state(main, gui.as_mut());
            gui.close_editor(main);
            *self.baseline.lock_recover() = None;
            true
        } else {
            false
        }
    }

    fn wakeups(&self, main: MainThreadMarker, out: &mut tutti_plugin_types::Wakeups) {
        if let Ok(mut guard) = self.gui.lock()
            && let Some(gui) = guard.as_mut()
        {
            gui.wakeups(main, out);
        }
    }

    fn wake(
        &self,
        main: MainThreadMarker,
        ready: &[tutti_plugin_ipc_ffi::Ready],
        now: tutti_platform::Instant,
    ) {
        if let Ok(mut guard) = self.gui.lock()
            && let Some(gui) = guard.as_mut()
        {
            gui.wake(main, ready, now);
        }
    }

    fn editor_idle(&self, main: MainThreadMarker) {
        if let Ok(mut guard) = self.gui.lock()
            && let Some(gui) = guard.as_mut()
        {
            gui.editor_idle(main);
            // Apply what the realtime setters queued for the editor's display.
            self.mirror
                .drain(|id, value| gui.set_parameter(main, id, value));
            if let Some(mode) = self.mirror.take_automation() {
                gui.set_automation_state(main, mode);
            }
            // Forward GUI-originated parameter changes to the audio bridge.
            self.forward_edits(gui.poll_edits(main));
            // What the editor instance asked of the host. A transport request
            // goes where the server's own go.
            let mut asked = Vec::new();
            gui.take_requests(main, &mut asked);
            for request in asked {
                self.route_editor_request(request);
            }
            // The plugin said its state changed: give the audio instance the
            // editor's state now rather than at close.
            if gui.take_dirty(main) {
                self.pull_editor_state(main, gui.as_mut());
            }
        }
    }

    fn editor_capabilities(&self, main: MainThreadMarker) -> EditorCapabilities {
        let Ok(mut guard) = self.gui.lock() else {
            return EditorCapabilities::default();
        };
        match guard.as_mut() {
            Some(gui) => gui.editor_capabilities(main),
            None => EditorCapabilities::default(),
        }
    }

    fn set_editor_size(
        &self,
        main: MainThreadMarker,
        requested: Size,
    ) -> std::result::Result<Size, EditorError> {
        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        let gui = guard.as_mut().ok_or(EditorError::NotOpen)?;
        gui.set_editor_size(main, requested)
            .map_err(|e| EditorError::PluginError(e.to_string()))
    }

    fn set_editor_scale(
        &self,
        main: MainThreadMarker,
        scale: ContentScale,
    ) -> std::result::Result<(), EditorError> {
        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        let gui = guard.as_mut().ok_or(EditorError::NotOpen)?;
        gui.set_editor_scale(main, scale).map_err(|e| match e {
            PluginError::Editor(typed) => typed,
            other => EditorError::PluginError(other.to_string()),
        })
    }

    fn poll_editor_resize_request(&self, main: MainThreadMarker) -> Option<Size> {
        let mut guard = self.gui.lock().ok()?;
        guard.as_mut()?.poll_editor_resize_request(main)
    }

    /// The editor's state as it stands now, as the baseline for the next
    /// [`pull_editor_state`]: called once the open has synced it from the
    /// audio instance. Read back rather than taken from the sync's bytes,
    /// because a plugin may normalise a state it is given. A state the editor
    /// instance cannot read leaves no baseline, so the editor's own changes are
    /// not carried to the audio instance: counted as a refused transfer.
    fn take_baseline(&self, main: MainThreadMarker, gui: &mut dyn Editor) {
        let baseline = match gui.save_state(main) {
            Ok(state) => Some(state),
            Err(e) => {
                tracing::warn!("[bridge] the editor's baseline state could not be read: {e}");
                self.mirror.refuse();
                None
            }
        };
        *self.baseline.lock_recover() = baseline;
    }

    /// [`pull_editor_state`] against this bridge's audio instance.
    fn pull_editor_state(&self, main: MainThreadMarker, gui: &mut dyn Editor) {
        let mut baseline = self.baseline.lock_recover();
        let pulled = pull_editor_state(main, gui, &mut baseline, |state| {
            self.audio
                .load_state(state, tutti_plugin_types::StateUse::Project)
        });
        if let Err(e) = pulled {
            tracing::warn!("[bridge] the editor's state did not reach the audio instance: {e}");
            self.mirror.refuse();
        }
    }
}

/// Gives the audio instance the editor instance's state when the user changed
/// it: the half of the mirror that carries what the user changed in the editor
/// and is not a parameter (a sample loaded in the plugin's UI, a mode switch). Parameters
/// already travel as edits.
///
/// **"Changed" is against `baseline`**, the editor's own state when it was
/// last in step with the audio instance (read right after the open's sync,
/// and moved by each pull). Comparing with the audio instance's state instead
/// took any difference for a user edit: automation or a plugin's volatile
/// state moved the audio side on its own, and the editor's stale state then
/// overwrote it. With no baseline (the editor's state could not be
/// read at open) the editor's state is loaded. Nothing is loaded when the
/// editor is at its baseline, so a closed editor that changed nothing does not
/// reload a playing plugin.
///
/// # Errors
///
/// The editor's state cannot be read (nothing is loaded), or the audio
/// instance refuses it (the baseline stays, so the next pull tries again).
fn pull_editor_state(
    main: MainThreadMarker,
    gui: &mut dyn Editor,
    baseline: &mut Option<Vec<u8>>,
    load: impl FnOnce(&[u8]) -> std::result::Result<(), StateError>,
) -> std::result::Result<bool, String> {
    let editor = gui
        .save_state(main)
        .map_err(|e| format!("reading the editor's state: {e}"))?;
    if baseline.as_deref() == Some(editor.as_slice()) {
        return Ok(false);
    }
    load(&editor).map_err(|e| format!("loading it into the audio instance: {e}"))?;
    *baseline = Some(editor);
    Ok(true)
}

/// How many parameter writes wait for the next editor idle. A full ring drops
/// the newest, counted: the mirror is cosmetic, and the audio command is authoritative.
const MIRROR_CAPACITY: usize = 256;

/// An automation mode as a nonzero code, 0 being "none".
fn mode_code(mode: AutomationMode) -> u8 {
    match mode {
        AutomationMode::Off => 1,
        AutomationMode::Reading => 2,
        AutomationMode::Writing => 3,
        AutomationMode::ReadWriting => 4,
    }
}

/// The mode a [`mode_code`] stands for; `None` for 0.
fn mode_of_code(code: u8) -> Option<AutomationMode> {
    match code {
        1 => Some(AutomationMode::Off),
        2 => Some(AutomationMode::Reading),
        3 => Some(AutomationMode::Writing),
        4 => Some(AutomationMode::ReadWriting),
        _ => None,
    }
}

/// The cosmetic mirror of the realtime setters onto the in-process editor.
///
/// The in-process editor is a second instance of the plugin whose parameter and
/// automation calls are main-thread calls, and `set_parameter_value` and
/// `set_automation_state_rt` run wherever the node's controls are written. So
/// they queue here (never blocking, never allocating) and the main-thread
/// [`editor_idle`](EditorSite::editor_idle) applies what was queued.
struct GuiMirror {
    /// The writes waiting for the next editor idle: a bounded lock-free
    /// queue, pushed from any thread and popped on the main thread.
    params: ArrayQueue<(ParamId, Normalized)>,
    /// Transfers between the audio and editor instances that were refused,
    /// ever: writes the full queue refused, and the failed ones of the editor's
    /// edits and state (see [`EditorSite::mirror_transfers_refused_cell`]).
    refused: Tally<u64>,
    /// The writer of `refused`.
    refused_writer: TallyWriter<u64>,
    /// The latest automation mode written, as [`mode_code`]; 0 for none.
    automation: PackedAtomic<u8>,
    /// The code of the mode the editor was last shown, 0 for none, so an idle
    /// applies a mode once. Main thread only.
    shown: PackedAtomic<u8>,
    /// Whether an editor is open. Nothing is queued while it is not, and what
    /// was queued is discarded when it opens or closes: an edit made with no
    /// editor to show it is not one to replay on the first idle.
    open: AtomicBool,
}

impl GuiMirror {
    fn new() -> Self {
        let refused = tally();
        Self {
            params: ArrayQueue::new(MIRROR_CAPACITY),
            refused_writer: refused.writer(),
            refused,
            automation: PackedAtomic::new(0),
            shown: PackedAtomic::new(0),
            open: AtomicBool::new(false),
        }
    }

    /// Queues a parameter write for the editor, if one is open. Wait-free,
    /// any thread; a full queue counts the write refused.
    fn push_param(&self, id: ParamId, value: Normalized) {
        if self.open.load(Ordering::Acquire) && self.params.push((id, value)).is_err() {
            self.refused_writer.add(1);
        }
    }

    /// Hands each queued write, oldest first, to `apply`. Main thread.
    fn drain(&self, mut apply: impl FnMut(ParamId, Normalized)) {
        for _ in 0..self.params.len() {
            let Some((id, value)) = self.params.pop() else {
                break;
            };
            apply(id, value);
        }
    }

    /// Records the latest automation mode for the editor, if one is open.
    /// Wait-free, any thread: one store.
    fn push_automation(&self, mode: AutomationMode) {
        if self.open.load(Ordering::Acquire) {
            self.automation.store(mode_code(mode));
        }
    }

    /// The automation mode written since the editor was last shown one, if
    /// any. Main thread.
    fn take_automation(&self) -> Option<AutomationMode> {
        let latest = self.automation.load();
        if latest == self.shown.load() {
            return None;
        }
        self.shown.store(latest);
        mode_of_code(latest)
    }

    /// Counts a transfer between the audio and editor instances that failed.
    fn refuse(&self) {
        self.refused_writer.add(1);
    }

    /// Drops everything queued.
    fn discard(&self) {
        self.drain(|_, _| {});
        self.automation.store(0);
        self.shown.store(0);
    }

    /// An editor opened (after its state sync): what was queued before it is
    /// stale, and writes are queued from here on.
    fn opened(&self) {
        self.open.store(false, Ordering::Release);
        self.discard();
        self.open.store(true, Ordering::Release);
    }

    /// The editor closed: stop queueing and drop what waits.
    fn closed(&self) {
        self.open.store(false, Ordering::Release);
        self.discard();
    }
}

#[cfg(test)]
mod gui_mirror_tests {
    use super::*;

    fn applied(mirror: &GuiMirror) -> Vec<ParamId> {
        let mut out = Vec::new();
        mirror.drain(|id, _| out.push(id));
        out
    }

    /// **An edit made with no editor open is not replayed by the first idle.**
    /// Automation runs with the editor closed; the editor opens; the first idle
    /// applies nothing stale. A write made while it is open is applied; what is
    /// still queued when it closes, or when it opens again, is dropped.
    #[test]
    fn automation_with_the_editor_closed_is_not_replayed_when_it_opens() {
        let mirror = GuiMirror::new();
        let id = ParamId::new(3);
        mirror.push_param(id, Normalized::of(0.9));
        mirror.push_automation(tutti_plugin_types::AutomationMode::Writing);
        assert!(
            applied(&mirror).is_empty(),
            "nothing is queued with no editor"
        );
        mirror.opened();
        assert!(
            applied(&mirror).is_empty(),
            "the first idle applies nothing stale"
        );

        mirror.push_param(id, Normalized::of(0.2));
        assert_eq!(
            applied(&mirror),
            vec![id],
            "an open editor is shown the write"
        );

        mirror.push_param(id, Normalized::of(0.4));
        mirror.closed();
        assert!(applied(&mirror).is_empty(), "closing drops what waited");

        mirror.opened();
        mirror.push_param(id, Normalized::of(0.6));
        mirror.opened();
        assert!(
            applied(&mirror).is_empty(),
            "opening again drops what waited"
        );
    }

    /// **`take_automation` applies a mode once**: the mode written is
    /// returned, then nothing until another is written; closing the editor
    /// drops one not yet applied.
    #[test]
    fn an_automation_mode_is_applied_once() {
        use tutti_plugin_types::AutomationMode;
        let mirror = GuiMirror::new();
        mirror.opened();
        assert_eq!(mirror.take_automation(), None, "nothing written yet");
        mirror.push_automation(AutomationMode::Reading);
        assert_eq!(mirror.take_automation(), Some(AutomationMode::Reading));
        assert_eq!(mirror.take_automation(), None, "applied once");
        mirror.push_automation(AutomationMode::Off);
        assert_eq!(mirror.take_automation(), Some(AutomationMode::Off));
        mirror.push_automation(AutomationMode::Writing);
        mirror.closed();
        assert_eq!(mirror.take_automation(), None, "closing drops it");
    }

    /// **A full mirror queue counts each refused write and keeps what it
    /// holds**: the first `MIRROR_CAPACITY` writes are applied in order.
    #[test]
    fn a_full_mirror_queue_counts_its_refusals() {
        let mirror = GuiMirror::new();
        mirror.opened();
        for i in 0..MIRROR_CAPACITY + 3 {
            mirror.push_param(ParamId::new(i as u32), Normalized::of(0.5));
        }
        assert_eq!(mirror.refused.get(), 3);
        let kept = applied(&mirror);
        assert_eq!(kept.len(), MIRROR_CAPACITY);
        assert_eq!(kept[0], ParamId::new(0));
    }

    /// **Pushing a write or a mode allocates nothing**, with the editor open
    /// and with the queue full.
    #[test]
    fn pushing_to_the_mirror_allocates_nothing() {
        let mirror = GuiMirror::new();
        mirror.opened();
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..MIRROR_CAPACITY + 2 {
                mirror.push_param(ParamId::new(i as u32), Normalized::of(0.25));
            }
            mirror.push_automation(tutti_plugin_types::AutomationMode::ReadWriting);
        });
        assert_eq!(mirror.refused.get(), 2);
    }
}

#[cfg(test)]
mod state_back_tests {
    //! The editor's state reaching the audio instance, on a
    //! stand-in editor: the decision is the bridge's, and each format's
    //! `save_state` is its host crate's.
    use super::*;
    use std::cell::RefCell;
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::{EditorWindow, PluginError, PluginResult};

    struct FakeEditor {
        /// The editor's state, or `None` when it cannot be read.
        state: Option<Vec<u8>>,
    }

    impl Editor for FakeEditor {
        fn set_parameter(&mut self, _: MainThreadMarker, _: ParamId, _: Normalized) {}
        fn set_state(&mut self, _: MainThreadMarker, _: &[u8]) -> PluginResult<()> {
            Ok(())
        }
        fn poll_edits(&mut self, _: MainThreadMarker) -> Vec<ParamEdit> {
            Vec::new()
        }
        fn save_state(&mut self, _: MainThreadMarker) -> PluginResult<Vec<u8>> {
            self.state
                .clone()
                .ok_or(PluginError::StateFailed(StateError::Unavailable))
        }
    }

    impl EditorWindow for FakeEditor {
        fn has_editor(&self, _: MainThreadMarker) -> bool {
            false
        }

        fn open_editor(
            &mut self,
            _: MainThreadMarker,
            _: WindowHandle,
            _: ContentScale,
        ) -> PluginResult<EditorOpened<Size>> {
            unreachable!("the state-back never opens")
        }

        fn close_editor(&mut self, _: MainThreadMarker) {}

        fn editor_idle(&mut self, _: MainThreadMarker) {}

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
        ) -> tutti_plugin_types::PluginResult<dpi::Size> {
            Err(tutti_plugin_types::PluginError::Editor(
                tutti_plugin_types::EditorError::PluginError(
                    "set_editor_size not supported".into(),
                ),
            ))
        }

        /// Nothing asked: this surface never queues a resize request.
        fn poll_editor_resize_request(
            &mut self,
            _main: tutti_plugin_types::MainThreadMarker,
        ) -> Option<dpi::Size> {
            None
        }
    }

    /// What the audio instance was given, if anything, and the baseline after.
    fn pull(
        editor: Option<Vec<u8>>,
        baseline: Option<Vec<u8>>,
    ) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let loaded = RefCell::new(None);
        let mut gui = FakeEditor { state: editor };
        let mut baseline = baseline;
        let _ = pull_editor_state(main_thread(), &mut gui, &mut baseline, |state| {
            *loaded.borrow_mut() = Some(state.to_vec());
            Ok(())
        });
        (loaded.into_inner(), baseline)
    }

    /// **A state the user changed in the editor reaches the audio instance.**
    /// (Was "a state only the editor holds", against the audio instance's
    /// state; the comparison is against the baseline now.)
    #[test]
    fn a_state_only_the_editor_holds_reaches_the_audio_instance() {
        let (got, _) = pull(Some(b"mode=delay".to_vec()), Some(b"mode=gain".to_vec()));
        assert_eq!(got.as_deref(), Some(&b"mode=delay"[..]));
    }

    /// **An editor that changed nothing does not reload the audio instance**,
    /// whatever the audio instance did meanwhile: its state is not consulted,
    /// so automation that moved it is not overwritten by the editor's stale
    /// one.
    #[test]
    fn an_unchanged_editor_reloads_nothing() {
        assert_eq!(pull(Some(b"same".to_vec()), Some(b"same".to_vec())).0, None);
    }

    /// **With no baseline the editor's state is taken.** No baseline is not
    /// evidence the editor changed nothing. (Was "an unreadable audio state".)
    #[test]
    fn an_unreadable_audio_state_still_takes_the_editors() {
        let (got, _) = pull(Some(b"editor".to_vec()), None);
        assert_eq!(got.as_deref(), Some(&b"editor"[..]));
    }

    /// **An editor whose state cannot be read gives the audio instance
    /// nothing**, not an empty state.
    #[test]
    fn an_unreadable_editor_state_loads_nothing() {
        let (got, _) = pull(None, Some(b"audio".to_vec()));
        assert_eq!(got, None);
    }

    /// **A pull moves the baseline**, so the same edit is not loaded again at
    /// the next dirty tick or at close.
    #[test]
    fn a_pulled_state_is_the_new_baseline() {
        let (got, baseline) = pull(Some(b"edited".to_vec()), Some(b"opened".to_vec()));
        assert!(got.is_some());
        assert_eq!(pull(Some(b"edited".to_vec()), baseline).0, None);
    }
}
