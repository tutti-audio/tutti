//! Pure message dispatch — no I/O, no async, no sockets.
//!
//! [`Session`] owns the plugin's control half and tells the render thread what its blocks read.
//! [`Session::handle`] translates one [`ControlRequest`] into state changes and returns a [`Reaction`]
//! describing what the transport loop should do next. Because it has no `async` and no `Transport`
//! dependency, it's unit-testable without tokio or sockets.

use tutti_platform::Instant;

use crate::audio_pipeline::Clock;
use crate::render::{RenderConfig, RenderLink, RenderSide, ToRender, link};
use crate::served::AnyServer;
use crate::ties::Ties;
use dpi::Size;
use tutti_buffers::FloatWidth;
use tutti_plugin_ipc::{
    AudioSlab, ControlReply, ControlRequest, News, SlabHandle, SlabLayout, StagedState, Working,
};
use tutti_plugin_ipc_ffi::Ready;
use tutti_plugin_types::{
    EditorError, IpcResult, MAX_STATE_BYTES, MainThreadMarker, MainWork, MainWorkHandle,
    ParameterQueue, PluginError, PluginFormat, PluginId, PluginRef, PluginResult, SampleRate,
    StateUse, Wakeups, WindowHandle,
};

/// What [`Session::handle`] asks the transport loop to do.
// One value per message, moved once to the transport loop; a `ControlReply`
// carries a block's inline MIDI list.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum Reaction {
    /// Send this reply back to the host.
    Reply(ControlReply),
    /// Send the plugin's saved state back: a `StateReady` frame and the object
    /// holding these bytes.
    ///
    /// Exists for plugin state, which does not fit a frame and so is answered as
    /// an object. Modelled as one `Reaction` rather than letting the handler
    /// write to the transport itself, so dispatch stays a pure function of the
    /// request: the property the whole `Reaction` enum exists to preserve, and
    /// what makes `handle` testable without a socket.
    State(Vec<u8>),
    /// No reply; keep looping.
    None,
    /// The host asked for shutdown; leave the loop.
    Shutdown,
}

impl From<ControlReply> for Reaction {
    fn from(m: ControlReply) -> Self {
        Self::Reply(m)
    }
}

#[cfg(test)]
impl Reaction {
    fn into_reply(self) -> ControlReply {
        match self {
            Reaction::Reply(m) => m,
            other => panic!("expected one reply, got {other:?}"),
        }
    }

    /// The bytes of a saved state.
    fn into_state(self) -> Vec<u8> {
        match self {
            Reaction::State(data) => data,
            other => panic!("expected a state, got {other:?}"),
        }
    }
}

/// What the main thread holds of one hosted plugin, and the dispatch over the
/// control connection's messages.
///
/// The blocks are the render thread's ([`RenderSide`]): this holds the
/// plugin's `Controller` half and tells the render thread what its blocks read
/// (`RenderLink`). It holds no transport, so a test drives it by calling
/// [`Session::handle`] directly with a [`ControlRequest`].
pub(crate) struct Session {
    /// The hosted plugin, or `None` before a load and after an unload. Every
    /// handler that needs one refuses explicitly rather than going silent.
    plugin: Option<AnyServer>,
    /// What the plugin's drops off the main thread hand over: run on every tick and
    /// after an unload.
    work: MainWork,
    /// The plugin's way to that collector.
    work_handle: MainWorkHandle,
    /// The render thread's queues: what the plugin's blocks are rendered with
    /// travels to it here.
    pub(crate) render: RenderLink,
    /// Sample rate + negotiated format. Written on load and on an explicit
    /// `SetSampleRate`, and published to the render thread with the rest of
    /// what a block reads ([`RenderConfig`]).
    pub(crate) clock: Clock,
    /// Why the plugin is not running at the rate the host asked for, while it
    /// is not. A refused plugin renders silence (no block reaches it) and the
    /// next `SetSampleRate` retries it.
    pub(crate) rate_refused: Option<PluginError>,
    /// The longest block the loaded plugin was activated for (`LoadPlugin`'s
    /// `block_size`), 0 with none loaded. A block past it, or past the slab's
    /// per-channel room, is refused before anything is sized for it.
    pub(crate) max_block: usize,
    /// What the render thread was last told its blocks read: the figures are
    /// sent again only when they change.
    published: RenderConfig,
    /// A state received and waiting for the next load, which restores it into
    /// the plugin before activating it.
    pub(crate) staged: Option<Vec<u8>>,
    /// Whether the plugin's editor is open in this process: what the idle tick serves.
    pub(crate) editor_open: bool,
    /// The editor was closed here on the server's own account (its plugin was
    /// unloaded or replaced): the host is told at the next tick.
    pub(crate) editor_closed_unasked: bool,
    /// A parameter write was queued to the Processor and no block that reached
    /// the plugin has been waited for since: a state saved now could miss it
    /// ([`Session::handle_save_state`]).
    write_pending: bool,
    /// The reader of the slots the render thread leaves each block's parameter
    /// values in: the edit controller is told of them on each tick.
    informs: Option<tutti_plugin_types::EditReader>,
}

/// How long a save waits for a block to apply a pending write before it
/// takes the host to be quiet: a few block periods at the longest block the
/// slab takes, and a little more.
///
/// Who pays it: only a save that follows a parameter write no block has
/// applied yet, while the host sends no blocks (stopped). Such a save takes up
/// to this much longer; a save with nothing pending, or during playback, does
/// not wait on it. Shorter risks taking a playing host with long blocks for a
/// quiet one, whose recall would silence a block.
const BLOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(20);

impl Session {
    /// An empty session: no plugin, no slab, and a default
    /// [`Clock`] that the first load overwrites; and the render side that
    /// serves its blocks, which a thread runs once the host's audio connection
    /// is there ([`RenderThread::spawn`](crate::render::RenderThread::spawn)).
    ///
    /// # Errors
    ///
    /// The OS refused a bell.
    pub(crate) fn new() -> std::io::Result<(Self, RenderSide)> {
        let (render, side) = link()?;
        let published = RenderConfig::new();
        let (work, work_handle) = MainWork::new();
        let session = Self {
            plugin: None,
            work,
            work_handle,
            render,
            clock: published.clock,
            rate_refused: None,
            max_block: 0,
            published,
            staged: None,
            editor_open: false,
            editor_closed_unasked: false,
            write_pending: false,
            informs: None,
        };
        Ok((session, side))
    }

    /// Tells the render thread what its blocks read, if that changed since it
    /// was last told: the rate and format, the length the plugin was activated
    /// for, whether it refused the rate, what it consumes and its channel counts.
    fn publish(&mut self) {
        let loaded = self.plugin.as_ref().map(|p| p.plugin().loaded());
        let config = RenderConfig {
            clock: self.clock,
            max_block: self.max_block,
            rate_refused: self.rate_refused.is_some(),
            features: loaded.map_or_else(tutti_plugin_types::Features::empty, |l| l.features),
            inputs: loaded.map_or(0, |l| l.total_inputs()),
            outputs: loaded.map_or(0, |l| l.total_outputs()),
        };
        if config != self.published {
            self.published = config;
            self.render.tell(ToRender::Config(config));
        }
    }

    /// Translate `msg` into state changes + a [`Reaction`].
    ///
    /// **A request that expects a frame always gets one**, including on the
    /// refusal paths: the host blocks waiting for the reply, so answering
    /// [`Reaction::None`] where it expects one hangs it until its timeout.
    ///
    /// # Errors
    ///
    /// Returns an error only when a failure cannot be expressed as a reply the
    /// host can act on. Plugin-level failures — a load that fails, a request
    /// with no plugin loaded — come back as an error-carrying `Reply` instead.
    pub(crate) fn handle(
        &mut self,
        main: MainThreadMarker,
        msg: ControlRequest,
    ) -> IpcResult<Reaction> {
        self.handle_with(main, msg, &mut |_| {})
    }

    /// [`handle`](Self::handle), with `interim` for a message that cannot wait
    /// for the reply: the host is told a load has begun restoring its state
    /// ([`Working::Restoring`]) before the restore, so what it waits
    /// for changes with it. Dispatch stays a function of the request; `interim`
    /// is a sink, not a source.
    pub(crate) fn handle_with(
        &mut self,
        main: MainThreadMarker,
        msg: ControlRequest,
        interim: &mut dyn FnMut(Working),
    ) -> IpcResult<Reaction> {
        let reaction = self.dispatch(main, msg, interim);
        // What the message changed that a block reads reaches the render thread
        // before the host is answered, so every block it sends after the answer
        // runs with it.
        self.publish();
        reaction
    }

    /// Maps the slab the host sent, and hands it to the render thread.
    ///
    /// **Always answered**: a slab this side cannot open, or a render thread that
    /// does not take it, is the host's to hear of as a reply, not the end of the
    /// session, and the answer is `SharedMemoryReady` only once the render thread
    /// holds the slab.
    pub(crate) fn setup_slab(&mut self, slab: &SlabHandle, layout: SlabLayout) -> Reaction {
        let reply = match AudioSlab::open(slab, layout) {
            // The scratch is sized by the attach, to the most a block may carry;
            // the render thread refuses any block longer.
            Ok(opened) => match self.render.attach_slab(opened) {
                Ok(()) => ControlReply::SharedMemoryReady,
                Err(e) => ControlReply::Error {
                    message: format!("the render thread did not take the slab: {e}"),
                },
            },
            Err(e) => ControlReply::Error {
                message: e.to_string(),
            },
        };
        reply.into()
    }

    fn dispatch(
        &mut self,
        main: MainThreadMarker,
        msg: ControlRequest,
        interim: &mut dyn FnMut(Working),
    ) -> IpcResult<Reaction> {
        use ControlRequest as M;
        match msg {
            M::LoadPlugin {
                path,
                format,
                plugin,
                sample_rate,
                block_size,
                preferred_format,
                state,
            } => self.handle_load(
                main,
                path,
                format,
                plugin,
                sample_rate,
                block_size,
                preferred_format,
                state,
                interim,
            ),
            M::UnloadPlugin => {
                self.discard_staged();
                self.drop_plugin(main);
                self.rate_refused = None;
                self.render.detach_slab();
                Ok(Reaction::None)
            }

            M::SetAutomationState { mode } => {
                self.write_pending = true;
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.plugin_mut().set_automation_state(main, mode);
                }
                Ok(Reaction::None)
            }
            M::SetTrackInfo { info } => {
                if let Some(plugin) = self.plugin.as_mut() {
                    plugin.plugin_mut().set_track_info(main, &info);
                }
                Ok(Reaction::None)
            }
            M::GetParameter { param_id } => {
                let value = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.plugin().parameter(main, param_id));
                Ok(ControlReply::ParameterValue { value }.into())
            }
            M::GetParameterList => {
                let parameters = self
                    .plugin
                    .as_ref()
                    .map(|p| p.plugin().parameters(main))
                    .unwrap_or_default();
                Ok(ControlReply::ParameterList { parameters }.into())
            }
            M::GetPresetList => {
                // `unwrap_or_default` — an empty list, matching
                // `GetParameterList` above. A request arriving before a plugin
                // is loaded is a host sequencing error, not something to report
                // as a preset-less plugin, and the same shape already covers it
                // for parameters.
                let presets = self
                    .plugin
                    .as_mut()
                    .map(|p| p.plugin_mut().presets(main))
                    .unwrap_or_default();
                Ok(ControlReply::PresetList { presets }.into())
            }
            M::LoadPreset { id } => {
                self.write_pending = true;
                self.forget_informs();
                let ok = self
                    .plugin
                    .as_mut()
                    .is_some_and(|p| p.plugin_mut().load_preset(main, &id));
                self.forget_informs();
                Ok(ControlReply::PresetLoaded { ok }.into())
            }
            M::GetCurrentPreset => {
                let id = self
                    .plugin
                    .as_mut()
                    .and_then(|p| p.plugin_mut().current_preset(main));
                Ok(ControlReply::CurrentPreset { id }.into())
            }
            M::GetParameterPlain { param_id, value } => {
                let plain = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.plugin().parameter_plain(main, param_id, value));
                Ok(ControlReply::ParameterPlain { plain }.into())
            }
            M::GetParameterNormalized { param_id, plain } => {
                let value = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.plugin().parameter_normalized(main, param_id, plain));
                Ok(ControlReply::ParameterNormalized { value }.into())
            }
            M::GetParameterText { param_id, value } => {
                let text = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.plugin().parameter_text(main, param_id, value));
                Ok(ControlReply::ParameterText { text }.into())
            }
            M::GetParameterValueFromText { param_id, text } => {
                // Reads as a query, but is not one on every format: VST2's
                // `effString2Parameter` writes the parsed value into the plugin
                // as it reads it. The trait takes `&self` because the plugin
                // handle is interior-mutable at the FFI edge, so this stays on
                // the `as_ref` path with the other queries.
                let value = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.plugin().parameter_value_from_text(main, param_id, &text));
                Ok(ControlReply::ParameterValueFromText { value }.into())
            }

            M::SetSampleRate { rate } => {
                // A rate no clock can run at is refused by the wire itself: a
                // `SampleRate` does not decode from one.
                // A refusal leaves it silent (`handle_process` publishes nothing) and
                // the host is told, because a rolled-back plugin would
                // otherwise render at its old rate against the host's blocks.
                let refusal = match self.plugin.as_mut() {
                    Some(plugin) => match plugin.plugin_mut().set_sample_rate(main, rate) {
                        Ok(()) => None,
                        Err(e) => {
                            tracing::warn!(requested = %rate, "sample rate not applied: {e}");
                            Some(e)
                        }
                    },
                    None => None,
                };
                // The wire carries the reason as text; the session keeps it typed.
                let error = refusal.as_ref().map(ToString::to_string);
                self.rate_refused = refusal;
                Ok(ControlReply::SampleRateSet { rate, error }.into())
            }
            M::SetRenderMode { mode } => {
                // The plugin's answer is deliberately not returned to the host:
                // whether a plugin honours the mode is a load-time capability
                // (`Features::RENDER_MODE`), not a per-call outcome, and the
                // host already has it. Logged rather than dropped so a refusal
                // is greppable when a bounce sounds like the live path.
                if let Some(plugin) = self.plugin.as_mut()
                    && !plugin.plugin_mut().set_render_mode(main, mode)
                {
                    tracing::debug!(?mode, "plugin did not accept the render mode");
                }
                Ok(Reaction::None)
            }
            M::OpenEditor { parent, scale } => {
                // The wire's result carries the reason as text.
                let result = self
                    .open_editor(main, parent, scale)
                    .map_err(|e| editor_wire_text(&e));
                Ok(ControlReply::EditorOpened { result }.into())
            }
            M::CloseEditor => {
                self.close_editor(main);
                Ok(Reaction::None)
            }
            M::SetEditorSize { size } => {
                let result = self
                    .set_editor_size(main, size)
                    .map_err(|e| editor_wire_text(&e));
                Ok(ControlReply::EditorSized { result }.into())
            }

            M::SaveState { state_use } => Ok(self.handle_save_state(main, state_use)),
            // The transport delivers a state with its object as
            // `Incoming::LoadState` and `Incoming::StageState`; a frame that
            // reaches here has none.
            M::LoadState { .. } | M::StageState { .. } => Ok(ControlReply::Error {
                message: "a state arrived without its object".into(),
            }
            .into()),

            // The transport delivers a setup with its slab as `Incoming::SetupSlab`
            // (`setup_slab`); a frame that reaches here has no slab.
            M::SetupSharedMemory { .. } => Ok(ControlReply::Error {
                message: "a slab setup arrived without its slab".into(),
            }
            .into()),
            M::Shutdown => Ok(Reaction::Shutdown),
        }
    }

    /// Leaves the loaded plugin as a panic in a call into it does.
    #[cfg(test)]
    pub(crate) fn lose_plugin(&mut self) {
        self.plugin.as_mut().expect("a plugin is loaded").poison();
    }

    /// Whether the loaded plugin is lost (a call into it panicked).
    pub(crate) fn plugin_lost(&self) -> bool {
        self.plugin.as_ref().is_some_and(|p| p.plugin().is_lost())
    }

    /// Whether a plugin is loaded.
    pub(crate) fn has_plugin(&self) -> bool {
        self.plugin.is_some()
    }

    /// What wakes the render thread and what it wakes, and the bell a plugin
    /// rings to make the main loop run.
    pub(crate) fn ties(&self) -> &Ties {
        self.render.ties()
    }

    /// Drain async events from the loaded plugin, mapping each into a
    /// `ControlReply` the transport can push back. Called from the
    /// main thread's loop after every message dispatch and on every tick.
    pub(crate) fn drain_async_events(&mut self, main: MainThreadMarker) -> Vec<News> {
        let Some(plugin) = self.plugin.as_mut() else {
            // A plugin rang as it was unloaded: nothing is left to tell.
            self.render.ties().bell.take();
            return Vec::new();
        };
        let events = plugin.plugin_mut().poll(main);
        // A parameter list that changed may have grown past the output reserved at load. This runs
        // on the main thread between messages, so only a pass that told of a changed list pays for
        // the count. What it helps: a VST3 `Reloaded` (the reload builds a new instance, whose
        // output lists are sized at its activation) and an AU list change. A VST3
        // `kParamTitlesChanged` without a reload keeps the instance's activation-time output and
        // emitted queue counts, so the instance drops (and counts) points past them before they
        // reach this output. A CLAP plugin's `params.rescan` that changes the list reaches this as
        // `PluginParamTitlesChanged` (the instance re-read it).
        if events
            .iter()
            .any(|e| matches!(e, News::PluginParamTitlesChanged | News::PluginReloaded))
        {
            let params = plugin.plugin().parameter_count(main);
            self.render.fit_params(params);
        }
        self.publish();
        events
    }

    /// What the loaded plugin wants the main loop to wait on: its descriptors
    /// and the earliest of its timers, added to `out`. Read before every wait,
    /// since registrations change as the plugin runs.
    pub(crate) fn wakeups(&mut self, main: MainThreadMarker, out: &mut Wakeups) {
        if let Some(control) = self
            .plugin
            .as_mut()
            .and_then(|p| p.plugin_mut().control_mut())
        {
            control.wakeups(main, out);
        }
    }

    /// Hands the plugin what a wait found (the descriptors in `ready`, the
    /// timers due at `now`) and serves it at once: what the plugin asked of the
    /// host in answer is what [`drain_async_events`](Self::drain_async_events)
    /// tells, so it is told in this pass and not at the next tick.
    pub(crate) fn wake(
        &mut self,
        main: MainThreadMarker,
        ready: &[Ready],
        now: Instant,
    ) -> Vec<News> {
        if let Some(control) = self
            .plugin
            .as_mut()
            .and_then(|p| p.plugin_mut().control_mut())
        {
            control.wake(main, ready, now);
        }
        self.drain_async_events(main)
    }

    /// One idle tick, between messages, on the thread that loaded the plugin:
    /// what the plugin asked of the host ([`drain_async_events`](Self::drain_async_events)),
    /// the open editor's idle (VST2's `effEditIdle`, VST3's run loop), a size
    /// the plugin asked its window to take, and the news that the editor was
    /// closed here. Allocates only to hold what it tells the host.
    pub(crate) fn tick(&mut self, main: MainThreadMarker) -> Vec<News> {
        self.work.run(main);
        self.inform_controller(main);
        self.render.report_losses();
        let mut told = self.drain_async_events(main);
        if self.editor_open
            && let Some(control) = self
                .plugin
                .as_mut()
                .and_then(|p| p.plugin_mut().control_mut())
        {
            control.editor_idle(main);
            if let Some(size) = control.poll_editor_resize_request(main) {
                told.push(News::EditorWantsSize { size });
            }
        }
        if std::mem::take(&mut self.editor_closed_unasked) {
            told.push(News::EditorClosed);
        }
        told
    }

    /// Makes the slots the render thread leaves each block's parameter values
    /// in follow the parameters the plugin lists now, and hands it their writer.
    fn declare_informs(&mut self, main: MainThreadMarker) {
        let Some(control) = self
            .plugin
            .as_mut()
            .and_then(|p| p.plugin_mut().control_mut())
        else {
            return;
        };
        let (writer, reader) =
            tutti_plugin_types::edit_slots(control.parameters(main).iter().map(|p| p.id()));
        self.informs = Some(reader);
        self.render.tell(ToRender::Informs(writer));
    }

    /// Tells the plugin's edit controller of the values the blocks since the
    /// last tick carried, each parameter's newest
    /// ([`Control::inform_controller`](tutti_plugin_types::Control::inform_controller)):
    /// the component has them through the block, and the controller hears them
    /// on the main thread, as a VST3 host informs both halves.
    fn inform_controller(&mut self, main: MainThreadMarker) {
        let (Some(reader), Some(control)) = (
            self.informs.as_mut(),
            self.plugin
                .as_mut()
                .and_then(|p| p.plugin_mut().control_mut()),
        ) else {
            return;
        };
        reader.drain(|id, value| control.inform_controller(main, id, value));
    }

    /// Forgets the values no tick has told the controller of: a state or a
    /// preset load is about to replace them, and a stale one told after it
    /// would put the controller back where it was. What the render thread has
    /// not yet written for a block that began before the load is voided too
    /// ([`Informs`](crate::render::Informs)). Called before the load and again
    /// after it, so a block that began while the load was applied is voided as well.
    fn forget_informs(&mut self) {
        self.render.void_informs();
        if let Some(reader) = self.informs.as_mut() {
            reader.drain(|_, _| {});
        }
    }

    /// Reports what the render thread lost in the session, once it has stopped.
    pub(crate) fn report_final_losses(&mut self) {
        self.render.report_final_losses();
    }

    /// Opens the plugin's editor on the served instance itself: embedded in
    /// `parent` (the host's window, as a number that means the same here), or
    /// the plugin's own window for `None`. The editor's size when embedded.
    fn open_editor(
        &mut self,
        main: MainThreadMarker,
        parent: Option<u64>,
        scale: tutti_plugin_types::ContentScale,
    ) -> PluginResult<Option<Size>> {
        if self.editor_open {
            return Err(EditorError::AlreadyOpen.into());
        }
        let Some(control) = self
            .plugin
            .as_mut()
            .and_then(|p| p.plugin_mut().control_mut())
        else {
            return Err(EditorError::NoPlugin.into());
        };
        let opened = match parent {
            Some(raw) => match WindowHandle::from_remote(raw) {
                Some(window) => control.open_editor(main, window, scale).map(Some),
                None => Err(EditorError::ForeignWindow(raw).into()),
            },
            None => control.open_floating_editor(main).map(|()| None),
        };
        self.editor_open = opened.is_ok();
        opened
    }

    /// Closes the editor, if one is open.
    fn close_editor(&mut self, main: MainThreadMarker) {
        if std::mem::take(&mut self.editor_open)
            && let Some(control) = self
                .plugin
                .as_mut()
                .and_then(|p| p.plugin_mut().control_mut())
        {
            control.close_editor(main);
        }
    }

    /// Asks the open editor to take `size`; the size it took.
    fn set_editor_size(&mut self, main: MainThreadMarker, size: Size) -> PluginResult<Size> {
        if !self.editor_open {
            return Err(EditorError::NotOpen.into());
        }
        let Some(control) = self
            .plugin
            .as_mut()
            .and_then(|p| p.plugin_mut().control_mut())
        else {
            return Err(EditorError::NoPlugin.into());
        };
        control.set_editor_size(main, size)
    }

    /// Lets the plugin go (an unload, or a load that replaces it), closing its
    /// editor first and telling the host it did.
    ///
    /// The Processor is brought home from the render thread before the plugin
    /// is destroyed (a block that arrives meanwhile is silence), and only then
    /// is the render thread's seat taken away: its poll is what returns the
    /// Processor.
    pub(crate) fn drop_plugin(&mut self, main: MainThreadMarker) {
        if self.editor_open {
            self.close_editor(main);
            self.editor_closed_unasked = true;
        }
        if let Some(mut plugin) = self.plugin.take() {
            if !plugin.plugin_mut().quiesce(main) {
                tracing::error!(
                    "plugin-server: the render thread did not return the processor; the plugin is leaked"
                );
                crate::leak::note();
            }
            drop(plugin);
            self.work.run(main);
            self.render.tell(ToRender::Unseat);
        }
    }

    /// Forgets a state staged for a load that has not come: the stage belongs to
    /// the one load that declares it.
    fn discard_staged(&mut self) {
        self.staged = None;
    }

    fn handle_load(
        &mut self,
        main: MainThreadMarker,
        path: std::path::PathBuf,
        format: PluginFormat,
        plugin: Option<PluginId>,
        sample_rate: SampleRate,
        block_size: usize,
        preferred_format: FloatWidth,
        declared: Option<StagedState>,
        interim: &mut dyn FnMut(Working),
    ) -> IpcResult<Reaction> {
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
                return Ok(ControlReply::Error {
                    message: format!(
                        "the load declares a state of {:?} bytes and the server holds {:?}",
                        declared.map(|d| d.len),
                        staged.as_ref().map(Vec::len),
                    ),
                }
                .into());
            }
        }
        let mut restoring = || interim(Working::Restoring);
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
            Err(e) => return Ok(load_failure(&e).into()),
        };
        let Some(id) = plugin.plugin().origin().id().cloned() else {
            return Ok(ControlReply::Error {
                message: "the loaded plugin names no id of its own".into(),
            }
            .into());
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
            return Ok(ControlReply::Error {
                message: format!("the loaded plugin cannot be seated: {e}"),
            }
            .into());
        }
        Ok(ControlReply::PluginLoaded {
            id,
            descriptor: Box::new(descriptor),
            loaded,
            negotiated_format: negotiated,
            state_error: state_error.map(|e| e.to_string()),
        }
        .into())
    }

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
    ///   to run after it ([`RenderLink::barrier`]), then saves. The Processor
    ///   never leaves the render thread.
    /// - A write pending and no block within [`BLOCK_WAIT`]: the host is quiet,
    ///   so the Processor is brought home for the settle, which is silent
    ///   because nothing is playing.
    fn handle_save_state(&mut self, main: MainThreadMarker, state_use: StateUse) -> Reaction {
        let settle = self.write_pending && {
            let id = self.render.barrier();
            !self.render.wait_applied(id, BLOCK_WAIT)
        };
        self.write_pending = false;
        let data = match self.plugin.as_mut() {
            Some(p) => match if settle {
                p.plugin_mut().save_state(main, state_use)
            } else {
                p.plugin_mut().save_state_live(main, state_use)
            } {
                Ok(data) => data,
                // A plugin that fails to serialise is not a transport problem:
                // one `Error` frame, and the host reports it as a rejection.
                Err(e) => {
                    return ControlReply::Error {
                        message: format!("failed to save state: {e}"),
                    }
                    .into();
                }
            },
            None => Vec::new(),
        };
        // Refuse an over-limit state here rather than sending a gigabyte the
        // host is only going to reject.
        if data.len() > MAX_STATE_BYTES {
            return ControlReply::Error {
                message: format!(
                    "plugin state is {} bytes, over the {MAX_STATE_BYTES}-byte limit",
                    data.len()
                ),
            }
            .into();
        }
        Reaction::State(data)
    }

    /// Keeps `state`, which arrived as an object, for the next load. Answers as
    /// a load does, `error: None` meaning staged: nothing is applied until
    /// `LoadPlugin`.
    pub(crate) fn stage_state(&mut self, state: Vec<u8>) -> Reaction {
        self.staged = Some(state);
        ControlReply::StateLoaded { error: None }.into()
    }

    /// Answers a state whose object the transport refused (`error` says why),
    /// the stream still in step. A refused stage leaves nothing staged: the next
    /// load must not restore a stage left by an earlier request.
    pub(crate) fn refuse_state(&mut self, staging: bool, error: String) -> Reaction {
        if staging {
            self.discard_staged();
        }
        ControlReply::StateLoaded { error: Some(error) }.into()
    }

    /// Applies `state`, which arrived as an object, to the plugin that is open.
    ///
    /// Always answers, so the host's waiting caller is never left guessing:
    /// every path produces a `StateLoaded`, "no plugin loaded" included, which
    /// is a refusal rather than a silence.
    pub(crate) fn load_state(
        &mut self,
        main: MainThreadMarker,
        state: &[u8],
        state_use: StateUse,
    ) -> Reaction {
        self.forget_informs();
        let error = match self.plugin.as_mut() {
            None => Some("no plugin is loaded".to_string()),
            Some(plugin) => match plugin.plugin_mut().load_state(main, state, state_use) {
                Ok(()) => None,
                Err(e) => Some(format!("failed to load state: {e}")),
            },
        };
        self.forget_informs();
        self.publish();
        ControlReply::StateLoaded { error }.into()
    }
}

/// An editor call's failure as the text the wire carries: an editor error says
/// itself, not through the "plugin editor error" of the error that wraps it.
fn editor_wire_text(e: &PluginError) -> String {
    match e {
        PluginError::Editor(editor) => editor.to_string(),
        other => other.to_string(),
    }
}

/// The reply to a load that failed. The wire carries a message and no stage, so
/// a loader failure keeps its stage in the text.
fn load_failure(e: &PluginError) -> ControlReply {
    let message = match e {
        PluginError::Load { stage, reason } => format!("{stage}: {reason}"),
        other => other.to_string(),
    };
    ControlReply::Error { message }
}

// Tests — pure dispatch, no tokio, no sockets.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use tutti_plugin_ipc::{AudioReply, AudioRequest};

    /// A session with its render side running beside it, as the server runs
    /// them: a thread steps the side (applies the commands, trades the
    /// Processor) while the test drives the control messages, and a block is
    /// served on the test's own thread (the host's audio connection is not
    /// what these tests are about; `main_loop`'s are).
    struct Rig {
        session: Session,
        side: Arc<Mutex<RenderSide>>,
        stop: Arc<AtomicBool>,
        stepper: Option<std::thread::JoinHandle<()>>,
    }

    impl Rig {
        fn new() -> Self {
            let (session, side) = Session::new().expect("bells");
            let side = Arc::new(Mutex::new(side));
            let stop = Arc::new(AtomicBool::new(false));
            let stepper = std::thread::spawn({
                let (side, stop) = (Arc::clone(&side), Arc::clone(&stop));
                let wake = session.ties().wake.clone();
                move || {
                    while !stop.load(Ordering::Acquire) {
                        {
                            let mut side = side.lock().unwrap();
                            side.drain();
                            side.poll_seat();
                        }
                        // As the render thread waits: for what the session
                        // rings it for.
                        wake.wait(None).expect("a wait");
                    }
                }
            });
            Self {
                session,
                side,
                stop,
                stepper: Some(stepper),
            }
        }

        /// The render side, with what the session queued applied.
        fn side(&self) -> MutexGuard<'_, RenderSide> {
            let mut side = self.side.lock().unwrap();
            side.drain();
            side
        }

        /// Attaches the host's slab, as `SetupSharedMemory` does.
        fn attach(&mut self, slab: tutti_plugin_ipc::SlabWriter) {
            self.session.render.attach_slab(slab).unwrap();
        }

        /// One message of the control connection, on the session.
        fn handle(&mut self, main: MainThreadMarker, msg: ControlRequest) -> IpcResult<Reaction> {
            self.session.handle(main, msg)
        }

        /// One message of the audio connection, on the render side.
        fn handle_audio(&mut self, msg: AudioRequest) -> IpcResult<Option<AudioReply>> {
            let mut side = self.side();
            side.poll_seat();
            side.handle(msg)
        }
    }

    impl std::ops::Deref for Rig {
        type Target = Session;
        fn deref(&self) -> &Session {
            &self.session
        }
    }

    impl std::ops::DerefMut for Rig {
        fn deref_mut(&mut self) -> &mut Session {
            &mut self.session
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            // The plugin goes home before the stepper stops, as the server
            // does at its end.
            self.session.drop_plugin(main_thread());
            self.stop.store(true, Ordering::Release);
            self.session.ties().wake.ring();
            if let Some(stepper) = self.stepper.take() {
                let _ = stepper.join();
            }
        }
    }
    use tutti_fixtures::main_thread;

    // `Features` is named only by `load_clap_plugin_f64`, which carries the
    // same gate.
    use tutti_plugin_ipc::SlabWriter;
    #[cfg(feature = "clap")]
    use tutti_plugin_types::Features;
    use tutti_plugin_types::LoadStage;
    use tutti_plugin_types::{MidiEventVec, NoteExpressionChanges, ParameterChanges};
    #[cfg(feature = "clap")]
    use tutti_units::Normalized;
    use tutti_units::ParamId;

    fn assert_none(r: Reaction) {
        assert!(matches!(r, Reaction::None), "got {r:?}");
    }

    /// What a save in the middle of steady playback did to the blocks around it.
    #[cfg(feature = "clap")]
    struct SteadySave {
        /// Whether each block after the first few was silent.
        silent: Vec<bool>,
        /// The peak of each block after the first few that started before the
        /// save returned, and of each that started after it.
        peaks_before_save: Vec<f32>,
        peaks_after_save: Vec<f32>,
        before: Vec<u8>,
        after: Vec<u8>,
    }

    /// Plays blocks of a constant signal through the probe, which scales it by
    /// its `Gain` parameter and so is never silent, saves the state (once with
    /// `write` first, as a UI parameter write just before a snapshot), and
    /// reports whether any block was silent.
    #[cfg(feature = "clap")]
    fn save_during_playback(write: bool) -> SteadySave {
        use std::sync::atomic::{AtomicBool, Ordering};

        // The first blocks warm the render side up and are dropped from the
        // result; the save waits until this many more have rendered, so blocks
        // from before the save are always in it.
        const WARM_UP: usize = 10;
        const SETTLED: usize = 10;
        // Blocks the player renders between two snapshots, and after the last.
        const BETWEEN_SAVES: usize = 3;
        const AFTER_SAVES: usize = 30;

        const FRAMES: usize = 64;
        // The probe's `Gain` parameter (`params_state::GAIN_PARAM_ID`).
        const GAIN: u32 = 77;
        let _lock = crate::test_utils::plugin_load_lock();
        let _render = crate::test_utils::clap_probe_render_mode(
            tutti_clap_test_plugin::RenderMode::TagPassthrough as u32,
        );
        let _gain = crate::test_utils::clap_switch_until_drop(
            tutti_clap_test_plugin::ClapSwitch::ApplyGain,
            1,
            0,
        );
        let (mut s, mut shm) = load_clap(FloatWidth::F32);
        let before = save_state_bytes(&mut s);

        let side = Arc::clone(&s.side);
        let stop = Arc::new(AtomicBool::new(false));
        let (rendered_one, rendered_blocks) = std::sync::mpsc::channel::<()>();
        let player = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut blocks = Vec::new();
                let mut seq = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let started = tutti_platform::Instant::now();
                    seq += 1;
                    for ch in 0..2 {
                        shm.write::<f32>(seq, ch, &[1.0; FRAMES]).unwrap();
                    }
                    shm.publish(seq);
                    {
                        let mut side = side.lock().unwrap();
                        side.drain();
                        side.poll_seat();
                        side.handle(AudioRequest::ProcessAudio(
                            tutti_plugin_ipc::ProcessAudioData {
                                seq,
                                num_samples: FRAMES,
                                ..tutti_plugin_ipc::ProcessAudioData::empty(
                                    tutti_plugin_types::SampleRate::SR_48K,
                                )
                            },
                        ))
                        .unwrap();
                    }
                    let mut out = vec![0.0f32; FRAMES];
                    let peak = if shm.has_output(seq)
                        && shm.read_output_into::<f32>(seq, 0, &mut out).is_ok()
                    {
                        out.iter().fold(0.0f32, |m, &x| m.max(x.abs()))
                    } else {
                        0.0
                    };
                    blocks.push((started, peak));
                    let _ = rendered_one.send(());
                    // Lets the test's thread take the side's lock between blocks.
                    std::thread::yield_now();
                }
                blocks
            }
        });
        // Each block the player renders is a message. `played` is how many have
        // rendered by now, and `rendered_to` waits for `count` in all; its limit
        // only guards a hang.
        let rendered = std::cell::Cell::new(0usize);
        let played = || {
            while rendered_blocks.try_recv().is_ok() {
                rendered.set(rendered.get() + 1);
            }
            rendered.get()
        };
        let rendered_to = |count: usize| {
            while rendered.get() < count {
                rendered_blocks
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or_else(|_| {
                        panic!("the player rendered {} of {count} blocks", rendered.get())
                    });
                rendered.set(rendered.get() + 1);
            }
        };
        rendered_to(WARM_UP + SETTLED);
        if write {
            let r = s
                .handle_audio(apply_parameter(GAIN, 0.25))
                .unwrap()
                .expect("answered");
            assert!(matches!(r, AudioReply::ParametersApplied { .. }), "{r:?}");
        }
        let after = save_state_bytes(&mut s);
        let saved_at = tutti_platform::Instant::now();
        // Snapshots come round after round in a session, with blocks between
        // them; each one must leave the blocks alone.
        for _ in 0..10 {
            rendered_to(played() + BETWEEN_SAVES);
            save_state_bytes(&mut s);
        }
        rendered_to(played() + AFTER_SAVES);
        stop.store(true, Ordering::Release);
        let blocks: Vec<_> = player.join().unwrap().into_iter().skip(WARM_UP).collect();
        SteadySave {
            silent: blocks.iter().map(|&(_, peak)| peak == 0.0).collect(),
            peaks_before_save: blocks
                .iter()
                .filter(|&&(at, _)| at < saved_at)
                .map(|&(_, peak)| peak)
                .collect(),
            peaks_after_save: blocks
                .iter()
                .filter(|&&(at, _)| at >= saved_at)
                .map(|&(_, peak)| peak)
                .collect(),
            before,
            after,
        }
    }

    /// **A save during steady playback, with a write just before it, returns the
    /// write and silences no block.** The write waits in the Processor's ring
    /// for a block; the save waits for the next block to apply it (a barrier
    /// through the render thread's queue) instead of recalling the Processor,
    /// which would make one block silent.
    #[test]
    #[cfg(feature = "clap")]
    fn a_save_after_a_write_during_playback_sees_it_and_silences_no_block() {
        let r = save_during_playback(true);
        assert_ne!(r.before, r.after, "the save has the write");
        let silent = r.silent.iter().filter(|&&s| s).count();
        assert_eq!(silent, 0, "{silent} blocks were silent around the save");
        // The block the save waited for applied the write before it rendered,
        // so every block that started after the save returned renders at the
        // new gain: the state the save reported is the state that sounds.
        let first = r.peaks_before_save[0];
        assert!(!r.peaks_after_save.is_empty(), "blocks ran after the save");
        assert!(
            r.peaks_after_save.iter().all(|&p| p != first),
            "a block after the save rendered at the old gain {first}: {:?}",
            r.peaks_after_save
        );
    }

    /// **A save with nothing pending recalls nothing**: no block around it is
    /// silent, and the state is the one it was.
    #[test]
    #[cfg(feature = "clap")]
    fn a_save_with_no_write_pending_recalls_nothing() {
        let r = save_during_playback(false);
        assert_eq!(r.before, r.after);
        let silent = r.silent.iter().filter(|&&s| s).count();
        assert_eq!(silent, 0, "{silent} blocks were silent around the save");
    }

    /// **With the host quiet, a save after a write still has the write**: no
    /// block comes to apply it, so the Processor is brought home for the settle.
    #[test]
    #[cfg(feature = "clap")]
    fn a_save_after_a_write_with_no_blocks_sees_it() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let before = save_state_bytes(&mut s);
        let r = s
            .handle_audio(apply_parameter(77, 0.25))
            .unwrap()
            .expect("answered");
        assert!(matches!(r, AudioReply::ParametersApplied { .. }), "{r:?}");
        assert_ne!(before, save_state_bytes(&mut s));
    }

    /// Every request a host can send before a plugin is loaded must be
    /// *answered* — with an absent value, an empty list, or a refusal, but
    /// never with silence. The host blocks on a reply frame for each of the
    /// query-shaped ones, so a missing answer is a hang rather than an error.
    ///
    /// The table is the point: adding a `ControlRequest` variant without adding a
    /// row leaves its no-plugin path uncovered, and the panic messages name the
    /// variant so a failure says which one.
    #[test]
    fn every_request_is_answered_with_no_plugin_loaded() {
        let cases: Vec<(&str, ControlRequest)> = vec![
            (
                "GetParameter",
                ControlRequest::GetParameter {
                    param_id: ParamId::new(0),
                },
            ),
            ("GetParameterList", ControlRequest::GetParameterList),
            (
                "SaveState",
                ControlRequest::SaveState {
                    state_use: StateUse::Project,
                },
            ),
        ];

        for (name, msg) in cases {
            let mut s = Rig::new();
            let r = s
                .handle(main_thread(), msg)
                .unwrap_or_else(|e| panic!("{name} errored with no plugin: {e:?}"));

            if let Reaction::State(bytes) = r {
                assert!(bytes.is_empty(), "{name}: got state bytes");
                continue;
            }
            match r.into_reply() {
                ControlReply::ParameterValue { value } => {
                    assert!(value.is_none(), "{name}: got a value with no plugin")
                }
                ControlReply::ParameterList { parameters } => {
                    assert!(parameters.is_empty(), "{name}: got parameters")
                }
                other => panic!("{name}: unexpected reply {other:?}"),
            }
        }
    }

    /// **A block with no plugin loaded is answered, refused**: the host reads
    /// silence for it. A reset has no answer and no plugin to reach, and is
    /// not an error.
    #[test]
    fn a_block_and_a_reset_with_no_plugin_are_served() {
        let mut s = Rig::new();
        let block = AudioRequest::ProcessAudio(tutti_plugin_ipc::ProcessAudioData {
            seq: 0,
            num_samples: 256,
            midi_events: MidiEventVec::new(),
            param_changes: ParameterChanges::new(),
            note_expression: NoteExpressionChanges::new(),
            ..tutti_plugin_ipc::ProcessAudioData::empty(tutti_plugin_types::SampleRate::SR_48K)
        });
        match s.handle_audio(block).unwrap() {
            Some(AudioReply::AudioProcessed { refused, .. }) => {
                assert!(refused, "a block with no plugin publishes nothing")
            }
            other => panic!("expected a refused block, got {other:?}"),
        }
        assert!(s.handle_audio(AudioRequest::Reset).unwrap().is_none());
    }

    #[test]
    fn set_sample_rate_with_no_plugin_is_answered_without_error() {
        let mut s = Rig::new();
        let r = s
            .handle(
                main_thread(),
                ControlRequest::SetSampleRate {
                    rate: tutti_plugin_types::SampleRate::SR_96K,
                },
            )
            .unwrap()
            .into_reply();
        assert!(matches!(
            r,
            ControlReply::SampleRateSet { rate, error: None } if rate.get() == 96000.0
        ));
    }

    /// A `LoadState` with no plugin loaded answers, and answers with a refusal.
    ///
    /// Answering `Reaction::None` — silence — is the failure this pins. The host
    /// *waits* for this frame, so a silence hangs it until the state timeout;
    /// and paired with a host that assumes success, it reports a state load that
    /// never happened.
    #[test]
    fn load_state_no_plugin_refuses_rather_than_going_silent() {
        let mut s = Rig::new();
        let reply = s
            .load_state(main_thread(), &[1, 2, 3], StateUse::Project)
            .into_reply();
        match reply {
            ControlReply::StateLoaded { error: Some(msg) } => {
                assert!(msg.contains("no plugin"), "unhelpful message: {msg:?}");
            }
            other => panic!("expected a StateLoaded carrying a refusal, got {other:?}"),
        }
    }

    /// **A state whose object the transport refused is answered as a refused
    /// load, and a refused stage leaves nothing staged**: the stage an earlier
    /// request left must not be restored by the load that follows.
    #[test]
    fn a_refused_state_is_answered_and_a_refused_stage_is_discarded() {
        let mut s = Rig::new();
        match s
            .refuse_state(false, "the object is not sealed".into())
            .into_reply()
        {
            ControlReply::StateLoaded { error: Some(msg) } => {
                assert!(msg.contains("not sealed"), "{msg:?}");
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
        assert_state_loaded_reply(s.stage_state(vec![1, 2, 3]));
        let _ = s.refuse_state(true, "the object is not sealed".into());
        assert!(s.staged.is_none(), "a refused stage left the earlier one");
    }

    #[test]
    fn unload_plugin_clears_shm() {
        let mut s = Rig::new();
        let r = s
            .handle(main_thread(), ControlRequest::UnloadPlugin)
            .unwrap();
        assert_none(r);
        assert!(s.plugin.is_none());
        assert!(!s.side().has_slab());
    }

    /// **A plugin's request to be processed, made on a thread of its own, rings
    /// the session's bell once and reaches the host once.** The bell is the one
    /// record of the request: it rings from the plugin's thread, and the pass
    /// that tells the host takes it.
    #[test]
    #[cfg(feature = "clap")]
    fn a_process_request_from_a_plugin_thread_rings_the_bell_and_tells_the_host_once() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let main = main_thread();
        assert!(!s.ties().bell.rung(), "nothing asked yet");
        assert!(
            std::thread::spawn(|| crate::test_utils::clap().request_process_now())
                .join()
                .expect("the plugin's thread"),
            "the plugin made the request"
        );
        assert!(s.ties().bell.rung(), "the request rang the bell");
        let told = |events: &[News]| {
            events
                .iter()
                .filter(|e| matches!(e, News::ProcessRequested))
                .count()
        };
        assert_eq!(told(&s.drain_async_events(main)), 1);
        assert!(!s.ties().bell.rung(), "telling the host took the ring");
        assert_eq!(told(&s.drain_async_events(main)), 0, "once");
    }

    /// **A slab the server cannot open is answered with an error, and the
    /// session goes on**: the host asked for shared memory that does not exist,
    /// and hears that, rather than the server ending, which the host would see as
    /// its crash.
    #[test]
    fn a_slab_that_cannot_be_opened_is_answered_and_the_session_goes_on() {
        let mut s = Rig::new();
        let layout = tutti_plugin_ipc::SlabLayout::new(
            256,
            FloatWidth::F32,
            vec![tutti_plugin_types::ChannelLayout::STEREO],
            vec![tutti_plugin_types::ChannelLayout::STEREO],
        );
        // A slab made for a longer layout than the one announced with it.
        let (_owner, shorter) = AudioSlab::create(layout.clone()).unwrap();
        let longer = SlabLayout::new(
            512,
            FloatWidth::F32,
            vec![tutti_plugin_types::ChannelLayout::STEREO],
            vec![tutti_plugin_types::ChannelLayout::STEREO],
        );
        let reply = s.session.setup_slab(&shorter, longer);
        assert!(
            matches!(reply, Reaction::Reply(ControlReply::Error { .. })),
            "the host hears the refusal"
        );
        assert!(!s.side().has_slab(), "nothing was attached");
        assert!(matches!(
            s.handle(main_thread(), ControlRequest::Shutdown).unwrap(),
            Reaction::Shutdown
        ));
    }

    /// **A setup is answered ready only once the render thread holds the slab**,
    /// and a `SetupSharedMemory` frame that reaches the session without its slab
    /// is refused in a reply.
    #[test]
    fn a_setup_is_ready_with_its_slab_and_refused_without() {
        let mut s = Rig::new();
        let layout = SlabLayout::new(
            256,
            FloatWidth::F32,
            vec![tutti_plugin_types::ChannelLayout::STEREO],
            vec![tutti_plugin_types::ChannelLayout::STEREO],
        );
        let (_owner, handle) = AudioSlab::create(layout.clone()).unwrap();
        assert!(matches!(
            s.session.setup_slab(&handle, layout.clone()),
            Reaction::Reply(ControlReply::SharedMemoryReady)
        ));
        assert!(s.side().has_slab(), "the render thread holds the slab");

        let bare = ControlRequest::SetupSharedMemory {
            slab: handle.wire(),
            layout,
        };
        assert!(matches!(
            s.handle(main_thread(), bare).unwrap(),
            Reaction::Reply(ControlReply::Error { .. })
        ));
    }

    /// **A session whose plugin was lost reports it, and one whose plugin runs
    /// does not**: what the server loop ends on.
    #[test]
    #[cfg(feature = "clap")]
    fn a_session_reports_a_lost_clap_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        assert!(!s.plugin_lost());
        s.lose_plugin();
        assert!(s.plugin_lost());
    }

    /// As above, for VST3.
    #[test]
    #[cfg(feature = "vst3")]
    fn a_session_reports_a_lost_vst3_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_vst3_probe();
        assert!(!s.plugin_lost());
        s.lose_plugin();
        assert!(s.plugin_lost());
    }

    /// **An editor error reaches the wire as its own text**, without the
    /// "plugin editor error" prefix of the error that wraps it.
    #[test]
    fn an_editor_error_is_flattened_to_its_own_text() {
        assert_eq!(
            editor_wire_text(&PluginError::Editor(EditorError::NotOpen)),
            "the editor is not open"
        );
        assert_eq!(
            editor_wire_text(&PluginError::Other("no".into())),
            "plugin error: no"
        );
    }

    /// A window number that means nothing in this process (an AppKit view's
    /// pointer) is refused as such: only macOS has one.
    #[cfg(all(target_os = "macos", feature = "clap"))]
    mod foreign_window {
        use super::*;

        /// **A host window this process cannot embed into is a foreign
        /// window**, named by the number the host sent.
        #[test]
        fn a_window_number_that_means_nothing_here_is_refused_as_foreign() {
            let _lock = crate::test_utils::plugin_load_lock();
            let (mut s, _shm) = load_clap(FloatWidth::F32);
            let raw = 0x40_0001;
            let scale = tutti_plugin_types::ContentScale::IDENTITY;
            assert!(matches!(
                s.open_editor(main_thread(), Some(raw), scale),
                Err(PluginError::Editor(EditorError::ForeignWindow(got))) if got == raw
            ));
        }
    }

    /// **An editor request the session cannot serve names its cause as an
    /// editor error**, not as a platform one: no plugin, an editor already
    /// open, and a resize with none open are three causes.
    #[test]
    fn an_editor_request_that_cannot_be_served_names_its_editor_error() {
        let mut s = Rig::new();
        let scale = tutti_plugin_types::ContentScale::IDENTITY;
        assert!(matches!(
            s.open_editor(main_thread(), None, scale),
            Err(PluginError::Editor(EditorError::NoPlugin))
        ));
        assert!(matches!(
            s.set_editor_size(main_thread(), dpi::PhysicalSize::new(10, 10).into()),
            Err(PluginError::Editor(EditorError::NotOpen))
        ));
        s.editor_open = true;
        assert!(matches!(
            s.open_editor(main_thread(), None, scale),
            Err(PluginError::Editor(EditorError::AlreadyOpen))
        ));
    }

    #[test]
    fn load_plugin_missing_path_errors() {
        let mut s = Rig::new();
        let reply = s
            .handle(
                main_thread(),
                ControlRequest::LoadPlugin {
                    path: std::path::PathBuf::from("/nonexistent/path/plugin.vst3"),
                    format: PluginFormat::Vst3,
                    plugin: None,
                    sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
                    block_size: 512,
                    preferred_format: FloatWidth::F32,
                    state: None,
                },
            )
            .expect("a failed load is a reply, not a server error")
            .into_reply();
        match reply {
            ControlReply::Error { message } => {
                assert!(message.starts_with(&format!("{}:", LoadStage::Scanning)));
                assert!(message.contains("does not exist"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(s.plugin.is_none());
    }

    // --------------------------------------------------------------------
    // CLAP integration tests.
    // --------------------------------------------------------------------

    #[cfg(feature = "clap")]
    /// The reference CLAP plugin, built as a dev-dependency by this same
    /// `cargo test` run. Resolved rather than hard-coded so these tests run on
    /// any machine.
    ///
    fn clap_plugin() -> &'static str {
        crate::test_utils::clap_probe_path()
    }

    /// Load the real CLAP plugin into a Session and attach shared memory.
    /// Returns (session, shm_guard). The guard keeps the creator-side mmap
    /// alive while the session holds its own mapping.
    #[cfg(feature = "clap")]
    fn load_clap(preferred_format: FloatWidth) -> (Rig, SlabWriter) {
        let mut s = Rig::new();
        s.handle(
            main_thread(),
            ControlRequest::LoadPlugin {
                path: std::path::PathBuf::from(clap_plugin()),
                format: PluginFormat::Clap,
                plugin: None,
                sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
                block_size: 512,
                preferred_format,
                state: None,
            },
        )
        .unwrap();

        // The negotiated format, not the requested one, as the host's
        // `setup_shm` sizes the slab.
        let loaded = s.plugin.as_ref().unwrap().plugin().loaded();
        let layout = tutti_plugin_ipc::SlabLayout::new(
            8192,
            s.clock.format,
            loaded.input_widths(),
            loaded.output_widths(),
        );
        let (shm_guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        s.attach(AudioSlab::open(&handle, layout).unwrap());
        (s, shm_guard)
    }

    /// The reply to loading the reference CLAP plugin's file choosing `plugin`.
    #[cfg(feature = "clap")]
    fn load_clap_choosing(plugin: Option<PluginId>) -> ControlReply {
        Rig::new()
            .handle(
                main_thread(),
                ControlRequest::LoadPlugin {
                    path: std::path::PathBuf::from(clap_plugin()),
                    format: PluginFormat::Clap,
                    plugin,
                    sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
                    block_size: 512,
                    preferred_format: FloatWidth::F32,
                    state: None,
                },
            )
            .expect("a load is a reply, not a server error")
            .into_reply()
    }

    /// **A load that chooses a plugin of a file that exports several loads that
    /// one** (task #43): before `LoadPlugin` carried the choice a bundle with
    /// several plugins always loaded its first. The reference CLAP file is a
    /// factory of two; a load that names neither loads the first, one that names
    /// the second loads the second, and one that names a plugin the file does
    /// not hold is refused, not answered with the first.
    #[test]
    #[cfg(feature = "clap")]
    fn a_load_that_chooses_a_plugin_loads_that_one() {
        let _lock = crate::test_utils::plugin_load_lock();
        let id_of = |reply: ControlReply| match reply {
            ControlReply::PluginLoaded { id, .. } => id,
            other => panic!("expected PluginLoaded, got {other:?}"),
        };
        let first = tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8");
        let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID
            .to_str()
            .expect("utf-8");

        let clap = |id: &str| PluginId::Clap(id.into());
        assert_eq!(id_of(load_clap_choosing(None)), clap(first), "no choice");
        assert_eq!(
            id_of(load_clap_choosing(Some(clap(second)))),
            clap(second),
            "the second plugin of the factory"
        );
        match load_clap_choosing(Some(clap("tutti.no-such-plugin"))) {
            ControlReply::Error { message } => {
                assert!(message.contains("tutti.no-such-plugin"), "{message}");
            }
            other => panic!("a plugin the file does not hold must be refused, got {other:?}"),
        }
    }

    /// Run one block of `input` on both channels through `s`'s slab.
    #[cfg(feature = "clap")]
    fn process_block(s: &mut Rig, shm: &mut SlabWriter, seq: u64, input: f64) {
        for ch in 0..2 {
            shm.write::<f64>(seq, ch, &[input; F64_FRAMES]).unwrap();
        }
        shm.publish(seq);
        let reply = s
            .handle_audio(AudioRequest::ProcessAudio(
                tutti_plugin_ipc::ProcessAudioData {
                    seq,
                    num_samples: F64_FRAMES,
                    ..tutti_plugin_ipc::ProcessAudioData::empty(
                        tutti_plugin_types::SampleRate::SR_48K,
                    )
                },
            ))
            .unwrap()
            .expect("answered");
        assert!(
            matches!(reply, AudioReply::AudioProcessed { .. }),
            "{reply:?}"
        );
    }

    /// **A refused rate is answered, silent, and retried by the next rate.**
    /// The plugin here drops
    /// its 64-bit ports, so the refusal leaves it deactivated; it publishes
    /// nothing while refused (the host reads silence), refuses again at the
    /// same rate, and a later rate it can take revives it, with audio at the
    /// right rate and no `deactivate` of the inactive instance.
    #[test]
    #[cfg(feature = "clap")]
    fn a_refused_rate_is_silent_and_a_later_rate_revives_the_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let _render = crate::test_utils::clap_probe_render_mode(
            tutti_clap_test_plugin::RenderMode::TagPassthrough as u32,
        );
        let (mut s, mut shm) = load_clap(FloatWidth::F64);
        assert_eq!(s.clock.format, FloatWidth::F64, "fixture");
        let set = |s: &mut Rig, rate: f64| {
            s.handle(
                main_thread(),
                ControlRequest::SetSampleRate {
                    rate: tutti_plugin_types::SampleRate::of(rate),
                },
            )
            .unwrap()
            .into_reply()
        };

        let accepted = set(&mut s, 48000.0);
        assert!(
            matches!(accepted, ControlReply::SampleRateSet { error: None, .. }),
            "{accepted:?}"
        );
        assert!(
            s.drain_async_events(main_thread()).is_empty(),
            "nothing changed"
        );
        process_block(&mut s, &mut shm, 1, 0.5);
        assert!(shm.has_output(1), "a running plugin publishes");

        // The plugin drops its 64-bit ports: the next rate is refused.
        let dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let refused = set(&mut s, 96000.0);
        assert!(
            matches!(
                &refused,
                ControlReply::SampleRateSet { rate, error: Some(e) }
                    if rate.get() == 96000.0 && !e.is_empty()
            ),
            "the host is told, with a reason: {refused:?}"
        );
        assert!(s.rate_refused.is_some());
        assert!(
            s.drain_async_events(main_thread())
                .iter()
                .any(|m| matches!(m, News::PluginIoChanged { .. })),
            "the host is told the instance can no longer run"
        );
        process_block(&mut s, &mut shm, 2, 0.5);
        assert!(!shm.has_output(2), "a refused plugin publishes nothing");

        // Refused again at the same rate: still silent.
        assert!(matches!(
            set(&mut s, 96000.0),
            ControlReply::SampleRateSet { error: Some(_), .. }
        ));
        process_block(&mut s, &mut shm, 3, 0.5);
        assert!(!shm.has_output(3), "still silent");

        // The ports come back: the next rate revives it.
        drop(dropped);
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let revived = set(&mut s, 88200.0);
        assert!(
            matches!(revived, ControlReply::SampleRateSet { error: None, .. }),
            "{revived:?}"
        );
        assert!(s.rate_refused.is_none());
        process_block(&mut s, &mut shm, 4, 0.5);
        assert!(shm.has_output(4), "a revived plugin publishes again");
        let mut out = vec![0.0f64; F64_FRAMES];
        shm.read_output_into::<f64>(4, 0, &mut out).unwrap();
        let want = 0.5 + f64::from(tutti_clap_test_plugin::probe_tag(0, 0));
        assert!(out.iter().all(|&x| x == want), "{:?}", &out[..4]);
    }

    /// An f64 session with a plugin that advertises no 64-bit port runs in
    /// f32: the request is a preference, and the reference plugin's default
    /// ports are f32-only.
    #[test]
    #[cfg(feature = "clap")]
    fn load_clap_plugin_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (s, _shm) = load_clap(FloatWidth::F64);
        let loaded = s.plugin.as_ref().unwrap().plugin().loaded();
        assert!(!loaded.features.contains(Features::F64_AUDIO));
        assert_eq!(s.clock.format, FloatWidth::F32);
    }

    /// The frames each 64-bit round trip below renders.
    #[cfg(feature = "clap")]
    const F64_FRAMES: usize = 64;

    /// Load the reference plugin with the 64-bit port `mask` into a session
    /// that prefers `format`, in `TagPassthrough` mode
    /// (`out[0][ch] = in[0][ch] + probe_tag(0, ch)`), and run one block with
    /// `input` on both channels through the shared-memory slab, as the host
    /// does. The render mode goes back to `Inert` when the guard drops, on a
    /// failed assertion too.
    #[cfg(feature = "clap")]
    fn clap_round_trip<T>(mask: u32, format: FloatWidth, input: T) -> RoundTrip<T>
    where
        T: tutti_buffers::Float,
    {
        const SEQ: u64 = 1;
        let _ports = crate::test_utils::clap_probe_ports_64bit(mask);
        let _render = crate::test_utils::clap_probe_render_mode(
            tutti_clap_test_plugin::RenderMode::TagPassthrough as u32,
        );
        let (mut s, mut shm) = load_clap(format);
        for ch in 0..shm.layout_ref().input_channels() {
            shm.write::<T>(SEQ, ch, &[input; F64_FRAMES]).unwrap();
        }
        shm.publish(SEQ);
        let reply = s
            .handle_audio(AudioRequest::ProcessAudio(
                tutti_plugin_ipc::ProcessAudioData {
                    seq: SEQ,
                    num_samples: F64_FRAMES,
                    ..tutti_plugin_ipc::ProcessAudioData::empty(
                        tutti_plugin_types::SampleRate::SR_48K,
                    )
                },
            ))
            .unwrap()
            .expect("answered");
        assert!(
            matches!(reply, AudioReply::AudioProcessed { .. }),
            "unexpected reply: {reply:?}"
        );
        assert!(
            shm.has_output(SEQ),
            "the block was not published: the plugin refused it"
        );
        let out = [0, 1].map(|ch| {
            let mut v = vec![T::default(); F64_FRAMES];
            shm.read_output_into::<T>(SEQ, ch, &mut v).unwrap();
            v
        });
        let loaded = s.plugin.as_ref().unwrap().plugin().loaded();
        RoundTrip {
            format: s.clock.format,
            f64_audio: loaded.features.contains(Features::F64_AUDIO),
            inputs: loaded.input_widths(),
            outputs: loaded.output_widths(),
            out,
        }
    }

    /// The probe's call-site record, from the image `load_clap` loaded.
    #[cfg(feature = "clap")]
    fn clap_probe_sites() -> tutti_clap_test_plugin::ThreadCapture {
        crate::test_utils::clap().thread_capture()
    }

    /// **A reset reaches the plugin, between the blocks it was sent between**.
    /// The server dropped `AudioRequest::Reset`, so a seek
    /// left every out-of-process CLAP and VST3 plugin's delay lines, reverb
    /// tails and filter memories sounding across the jump.
    ///
    /// The probe counts its `reset` and `process` calls; after each message
    /// the counts must be the ones that message implies, so the reset is seen
    /// once and in order.
    #[test]
    #[cfg(feature = "clap")]
    fn a_reset_reaches_the_plugin_between_two_blocks() {
        use tutti_clap_test_plugin::Site;

        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap(FloatWidth::F32);
        let visits = |site: Site| clap_probe_sites().sites[site as usize].visits;
        let resets_at_load = visits(Site::Reset);
        let processes_at_load = visits(Site::Process);

        let mut block = |s: &mut Rig, seq: u64| {
            for ch in 0..2 {
                shm.write::<f32>(seq, ch, &[0.0; F64_FRAMES]).unwrap();
            }
            shm.publish(seq);
            let reply = s
                .handle_audio(AudioRequest::ProcessAudio(
                    tutti_plugin_ipc::ProcessAudioData {
                        seq,
                        num_samples: F64_FRAMES,
                        ..tutti_plugin_ipc::ProcessAudioData::empty(
                            tutti_plugin_types::SampleRate::SR_48K,
                        )
                    },
                ))
                .unwrap()
                .expect("answered");
            assert!(
                matches!(reply, AudioReply::AudioProcessed { .. }),
                "{reply:?}"
            );
        };

        block(&mut s, 1);
        assert_eq!(visits(Site::Process), processes_at_load + 1);
        assert_eq!(visits(Site::Reset), resets_at_load, "no reset was sent yet");

        s.handle_audio(AudioRequest::Reset).unwrap();
        assert_eq!(
            visits(Site::Reset),
            resets_at_load + 1,
            "the reset reached the plugin"
        );
        assert_eq!(
            visits(Site::Process),
            processes_at_load + 1,
            "and no block ran"
        );

        block(&mut s, 2);
        assert_eq!(visits(Site::Process), processes_at_load + 2);
        assert_eq!(
            visits(Site::Reset),
            resets_at_load + 1,
            "one reset, between the blocks"
        );
    }

    /// **A block's length is the server's to check.** An empty
    /// block, one past the length the plugin was activated for (`block_size`,
    /// 512 here), and one past anything memory holds are refused before the
    /// plugin, or the scratch, sees them. Each is answered, published nothing
    /// (the host hears silence) and counted. A block within bounds still
    /// renders afterwards.
    #[test]
    #[cfg(feature = "clap")]
    fn a_block_of_a_length_the_plugin_cannot_take_never_reaches_it() {
        use tutti_clap_test_plugin::Site;

        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap(FloatWidth::F32);
        let visits = |site: Site| clap_probe_sites().sites[site as usize].visits;
        let processes_at_load = visits(Site::Process);

        let mut block = |s: &mut Rig, seq: u64, frames: usize| {
            for ch in 0..2 {
                shm.write::<f32>(seq, ch, &[0.0; 512]).unwrap();
            }
            shm.publish(seq);
            let reply = s
                .handle_audio(AudioRequest::ProcessAudio(
                    tutti_plugin_ipc::ProcessAudioData {
                        seq,
                        num_samples: frames,
                        ..tutti_plugin_ipc::ProcessAudioData::empty(
                            tutti_plugin_types::SampleRate::SR_48K,
                        )
                    },
                ))
                .unwrap()
                .expect("answered");
            assert!(
                matches!(reply, AudioReply::AudioProcessed { .. }),
                "every block is answered: {reply:?}"
            );
            shm.has_output(seq)
        };

        for (seq, frames) in [(1, 0), (2, 513), (3, 1 << 40)] {
            assert!(!block(&mut s, seq, frames), "{frames} frames published");
        }
        assert_eq!(s.render.losses().refused_blocks, 3);
        assert_eq!(
            visits(Site::Process),
            processes_at_load,
            "no refused block reached the plugin"
        );

        assert!(block(&mut s, 4, 512), "a block within bounds renders");
        assert_eq!(visits(Site::Process), processes_at_load + 1);
        assert_eq!(s.render.losses().refused_blocks, 3);
    }

    /// **A block never renders into a slab laid out unlike the plugin's ports.**
    /// A plugin that restarts itself with other ports leaves the slab of the
    /// old ones attached until the host sets up another: a block then reads
    /// back as silence, is counted, and never reaches the plugin; the matching
    /// slab renders again.
    #[test]
    #[cfg(feature = "clap")]
    fn a_slab_laid_out_unlike_the_plugins_ports_is_refused() {
        use tutti_clap_test_plugin::Site;
        use tutti_plugin_types::ChannelLayout;

        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, matching) = load_clap(FloatWidth::F32);
        let visits = |site: Site| clap_probe_sites().sites[site as usize].visits;
        let processes_at_load = visits(Site::Process);
        let layout = tutti_plugin_ipc::SlabLayout {
            samples_per_channel: 512,
            format: s.clock.format,
            slots: tutti_plugin_ipc::RING_SLOTS as u32,
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![ChannelLayout::MONO],
        };
        let (mut host, handle) = AudioSlab::create(layout.clone()).unwrap();
        s.attach(AudioSlab::open(&handle, layout).unwrap());

        let block = |s: &mut Rig, host: &mut SlabWriter, seq: u64, channels: usize| {
            for ch in 0..channels {
                host.write::<f32>(seq, ch, &[0.5; 512]).unwrap();
            }
            host.publish(seq);
            let reply = s
                .handle_audio(AudioRequest::ProcessAudio(
                    tutti_plugin_ipc::ProcessAudioData {
                        seq,
                        num_samples: 256,
                        ..tutti_plugin_ipc::ProcessAudioData::empty(
                            tutti_plugin_types::SampleRate::SR_48K,
                        )
                    },
                ))
                .unwrap()
                .expect("answered");
            assert!(matches!(reply, AudioReply::AudioProcessed { .. }));
            host.has_output(seq)
        };

        assert!(
            !block(&mut s, &mut host, 1, 2),
            "published into a mono slab"
        );
        assert_eq!(
            s.render.losses().refused_blocks,
            1,
            "the refusal is counted"
        );
        assert_eq!(
            visits(Site::Process),
            processes_at_load,
            "the block never reached the plugin"
        );

        let (mut matching, handle) = AudioSlab::create(matching.layout()).unwrap();
        s.attach(AudioSlab::open(&handle, matching.layout()).unwrap());
        assert!(
            block(&mut s, &mut matching, 2, 2),
            "the matching slab renders"
        );
        assert_eq!(s.render.losses().refused_blocks, 1);
    }

    /// What [`clap_round_trip`] observed.
    #[cfg(feature = "clap")]
    struct RoundTrip<T> {
        /// The session's negotiated format.
        format: FloatWidth,
        /// The plugin's `F64_AUDIO` bit.
        f64_audio: bool,
        /// The node's graph-facing input bus widths, as the server reports them.
        inputs: tutti_plugin_types::BusChannels,
        /// The node's graph-facing output buses.
        outputs: tutti_plugin_types::BusChannels,
        /// The two output channels.
        out: [Vec<T>; 2],
    }

    /// An f32 session (the default) runs a plugin whose every port can do
    /// 64-bit at f32, and its audio comes back.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f32_session_processes_audio_through_a_64bit_capable_clap_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let RoundTrip {
            format,
            f64_audio,
            out,
            ..
        } = clap_round_trip(both, FloatWidth::F32, 0.5f32);
        assert_eq!(format, FloatWidth::F32);
        assert!(!f64_audio, "activated f64 for an f32 session");
        for (ch, samples) in out.iter().enumerate() {
            let want = 0.5 + tutti_clap_test_plugin::probe_tag(0, ch as u32);
            assert!(
                samples.iter().all(|&x| x == want),
                "channel {ch}: want {want}, got {:?}",
                &samples[..4]
            );
        }
    }

    /// An f64 session runs a plugin whose every port can do 64-bit at f64,
    /// end to end: the input keeps precision f32 cannot carry.
    ///
    /// `0.1 + tag` is correctly rounded and deterministic, so the plugin's
    /// f64 sum is compared exactly against the same sum here; the same sum
    /// through f32 lands on a different value (asserted, so the check cannot
    /// pass on a path that rounds through f32).
    #[test]
    #[cfg(feature = "clap")]
    fn an_f64_session_runs_a_64bit_capable_clap_plugin_in_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let RoundTrip {
            format,
            f64_audio,
            out,
            ..
        } = clap_round_trip(both, FloatWidth::F64, 0.1f64);
        assert_eq!(format, FloatWidth::F64);
        assert!(f64_audio);
        for (ch, samples) in out.iter().enumerate() {
            let tag = tutti_clap_test_plugin::probe_tag(0, ch as u32);
            let want = 0.1f64 + f64::from(tag);
            assert_ne!(want, f64::from(0.1f32 + tag), "the input must not fit f32");
            assert!(
                samples.iter().all(|&x| x == want),
                "channel {ch}: want {want}, got {:?}",
                &samples[..4]
            );
        }
    }

    /// A plugin whose outputs claim 64-bit but whose inputs do not runs at
    /// f32 even in an f64 session: the host hands one width for every buffer.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f64_session_runs_a_clap_plugin_with_f32_inputs_in_f32() {
        let _lock = crate::test_utils::plugin_load_lock();
        let RoundTrip {
            format,
            f64_audio,
            out,
            ..
        } = clap_round_trip(
            tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS,
            FloatWidth::F64,
            0.5f32,
        );
        assert_eq!(format, FloatWidth::F32);
        assert!(!f64_audio);
        let want = 0.5 + tutti_clap_test_plugin::probe_tag(0, 0);
        assert!(out[0].iter().all(|&x| x == want));
    }

    /// An f64 instrument (no input port, 64-bit stereo out) runs end to end
    /// in an f64 session and is handed no input buffer, as CLAP requires
    /// (`audio_inputs_count` equals the plugin's own input port count), and
    /// reports no input bus: a direction with no audio is an empty list.
    ///
    /// The slab's inputs carry `0.1`; `TagPassthrough` adds input port 0 to
    /// output port 0 when the host hands one, so an undeclared input bus
    /// shows up as `0.1 + tag` rather than `tag`. With the bus handed at f64
    /// its `data32` is null, which a plugin that reads `data32` for every
    /// counted input would dereference.
    #[test]
    #[cfg(feature = "clap")]
    fn an_f64_clap_instrument_is_handed_no_input_buffer() {
        let _lock = crate::test_utils::plugin_load_lock();
        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::Instrument as u32,
        );
        let RoundTrip {
            format,
            f64_audio,
            inputs,
            outputs,
            out,
        } = clap_round_trip(
            tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS,
            FloatWidth::F64,
            0.1f64,
        );
        assert_eq!(format, FloatWidth::F64);
        assert!(f64_audio, "an instrument with 64-bit outputs runs at f64");
        assert!(
            inputs.is_empty(),
            "an instrument has no input bus: {inputs:?}"
        );
        assert_eq!(
            outputs.as_slice(),
            &[tutti_plugin_types::ChannelLayout::STEREO]
        );
        for (ch, samples) in out.iter().enumerate() {
            let want = f64::from(tutti_clap_test_plugin::probe_tag(0, ch as u32));
            assert!(
                samples.iter().all(|&x| x == want),
                "channel {ch}: want {want} (no input handed), got {:?}",
                &samples[..4]
            );
        }
    }

    /// The Cutoff parameter of the reference plugin: 100 to 1100, default 600.
    #[cfg(feature = "clap")]
    const CUTOFF: u32 = 101;

    /// **The parameter list is the plugin's own table, in its order.**
    #[test]
    #[cfg(feature = "clap")]
    fn the_parameter_list_is_the_plugins_declared_table_in_its_order() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let reply = s
            .handle(main_thread(), ControlRequest::GetParameterList)
            .unwrap()
            .into_reply();
        let ControlReply::ParameterList { parameters } = reply else {
            panic!("expected ParameterList, got {reply:?}");
        };
        let listed: Vec<u32> = parameters.iter().map(|p| p.id().get()).collect();
        let declared: Vec<u32> = tutti_clap_test_plugin::params_state::probe_params()
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(listed, declared);
    }

    /// **A parameter reads as its declared default**, normalized over its
    /// range: Cutoff's 600 in 100..1100 is one half.
    #[test]
    #[cfg(feature = "clap")]
    fn a_parameter_reads_as_its_declared_default_normalized() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let reply = s
            .handle(
                main_thread(),
                ControlRequest::GetParameter {
                    param_id: ParamId::new(CUTOFF),
                },
            )
            .unwrap()
            .into_reply();
        match reply {
            ControlReply::ParameterValue { value } => {
                assert_eq!(value.map(Normalized::get), Some(0.5));
            }
            other => panic!("expected ParameterValue, got {other:?}"),
        }
    }

    /// **A write sent between blocks reaches the plugin by the format's own
    /// route** (CLAP `params.flush`): the value read back is the one written,
    /// and the answer says it was applied.
    #[test]
    #[cfg(feature = "clap")]
    fn a_parameter_write_reaches_the_plugin_and_reads_back() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap(FloatWidth::F32);
        let param_id = ParamId::new(CUTOFF);
        let r = s
            .handle_audio(apply_parameter(param_id.get(), 0.25))
            .unwrap()
            .expect("answered");
        assert!(matches!(r, AudioReply::ParametersApplied { .. }), "{r:?}");
        assert!(
            run_silent_block(&mut s, &mut shm, 1, 64),
            "the block renders"
        );
        let reply = s
            .handle(main_thread(), ControlRequest::GetParameter { param_id })
            .unwrap()
            .into_reply();
        match reply {
            ControlReply::ParameterValue { value } => {
                assert_eq!(value.map(Normalized::get), Some(0.25));
            }
            other => panic!("expected ParameterValue, got {other:?}"),
        }
    }

    /// A `LoadState` that the plugin accepted.
    ///
    /// Asserts `StateLoaded { error: None }` rather than mere silence, which is
    /// strictly stronger: against an `assert_none`, a refusal is
    /// indistinguishable from a success at this layer.
    #[cfg(feature = "clap")]
    fn assert_state_loaded(r: Reaction) {
        match r.into_reply() {
            ControlReply::StateLoaded { error: None } => {}
            other => panic!("expected an accepted StateLoaded, got {other:?}"),
        }
    }

    /// Pull a SaveState blob out of a session, asserting it is non-empty.
    #[cfg(feature = "clap")]
    fn save_state_bytes(s: &mut Rig) -> Vec<u8> {
        let data = s
            .handle(
                main_thread(),
                ControlRequest::SaveState {
                    state_use: StateUse::Project,
                },
            )
            .unwrap()
            .into_state();
        assert!(!data.is_empty(), "state should be non-empty");
        data
    }

    /// Feed a whole blob in, as the transport does once its object is read, and
    /// return the reaction.
    #[cfg(feature = "clap")]
    fn load_state_bytes(s: &mut Rig, data: &[u8]) -> Reaction {
        s.load_state(main_thread(), data, StateUse::Project)
    }

    /// Stages `data` as a host does before a load, and returns the reaction.
    #[cfg(feature = "clap")]
    fn stage_state_bytes(s: &mut Rig, data: &[u8]) -> Reaction {
        s.stage_state(data.to_vec())
    }

    #[cfg(feature = "clap")]
    fn load_reply(s: &mut Rig, declared: Option<StagedState>) -> Option<String> {
        let reply = s
            .handle(
                main_thread(),
                ControlRequest::LoadPlugin {
                    path: std::path::PathBuf::from(clap_plugin()),
                    format: PluginFormat::Clap,
                    plugin: None,
                    sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
                    block_size: 512,
                    preferred_format: FloatWidth::F32,
                    state: declared,
                },
            )
            .unwrap()
            .into_reply();
        match reply {
            ControlReply::PluginLoaded { state_error, .. } => state_error,
            other => panic!("expected PluginLoaded, got {other:?}"),
        }
    }

    fn load_declaring(s: &mut Rig, declared: Option<StagedState>) -> ControlReply {
        s.handle(
            main_thread(),
            ControlRequest::LoadPlugin {
                path: std::path::PathBuf::from("/nonexistent/plugin.clap"),
                format: PluginFormat::Clap,
                plugin: None,
                sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
                block_size: 512,
                preferred_format: FloatWidth::F32,
                state: declared,
            },
        )
        .unwrap()
        .into_reply()
    }

    /// **A refused load leaves no plugin behind.** The server hosts one
    /// plugin and `LoadPlugin` replaces it; when the second load fails, the
    /// first is gone too, not kept answering for a host that was told the load
    /// failed.
    #[cfg(feature = "clap")]
    #[test]
    fn a_failed_second_load_unloads_the_first() {
        let mut s = Rig::new();
        load_reply(&mut s, None);
        assert!(s.plugin.is_some(), "the first load holds a plugin");
        let reply = load_declaring(&mut s, None);
        assert!(matches!(reply, ControlReply::Error { .. }), "{reply:?}");
        assert!(s.plugin.is_none(), "the refused load left the first plugin");
    }

    fn assert_mismatch(reply: ControlReply) {
        match reply {
            ControlReply::Error { message } => {
                assert!(message.contains("declares a state"), "{message}");
            }
            other => panic!("expected the mismatch refused, got {other:?}"),
        }
    }

    /// **A load restores the state it declares and holds, no other.** A state
    /// declared and none staged, a state staged and none declared, and a length
    /// that disagrees are each refused, and each leaves nothing staged. A load that
    /// declares nothing and holds nothing goes on to load (here: to not find the
    /// plugin).
    #[test]
    fn a_load_restores_only_the_state_it_declares() {
        let mut s = Rig::new();
        assert_mismatch(load_declaring(
            &mut s,
            Some(StagedState {
                len: 3,
                state_use: StateUse::Project,
            }),
        ));

        let mut s = Rig::new();
        assert_state_loaded_reply(s.stage_state(vec![1, 2, 3]));
        assert_mismatch(load_declaring(&mut s, None));
        assert!(s.staged.is_none(), "and the stage is gone");

        let mut s = Rig::new();
        assert_state_loaded_reply(s.stage_state(vec![1, 2, 3]));
        assert_mismatch(load_declaring(
            &mut s,
            Some(StagedState {
                len: 4,
                state_use: StateUse::Project,
            }),
        ));
        assert!(s.staged.is_none());

        let mut s = Rig::new();
        match load_declaring(&mut s, None) {
            ControlReply::Error { message } => {
                assert!(message.contains("does not exist"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    fn assert_state_loaded_reply(r: Reaction) {
        match r.into_reply() {
            ControlReply::StateLoaded { error: None } => {}
            other => panic!("expected a staged state, got {other:?}"),
        }
    }

    /// **A stage belongs to the load that declares it**: an unload that comes
    /// between discards it.
    #[test]
    fn a_stage_does_not_outlive_the_next_request() {
        let mut s = Rig::new();
        assert_state_loaded_reply(s.stage_state(vec![1]));
        let _ = s.handle(main_thread(), ControlRequest::UnloadPlugin);
        assert!(s.staged.is_none(), "an unload discards the stage");
    }

    /// **A state staged before a load is restored by the load**, before the
    /// plugin is activated, and consumed: a plugin's own state goes in and the
    /// reply carries no refusal; bytes it cannot parse are refused in the reply
    /// and the plugin loads at its defaults; and the next load has nothing
    /// staged.
    #[test]
    #[cfg(feature = "clap")]
    fn a_staged_state_is_restored_by_the_next_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut source, _shm) = load_clap(FloatWidth::F32);
        let own = save_state_bytes(&mut source);

        let mut s = Rig::new();
        assert_state_loaded(stage_state_bytes(&mut s, &own));
        assert!(s.staged.is_some(), "the state waits for the load");
        assert_eq!(
            load_reply(
                &mut s,
                Some(StagedState {
                    len: own.len() as u64,
                    state_use: StateUse::Project,
                })
            ),
            None,
            "the plugin took its own state"
        );
        assert!(s.staged.is_none(), "and the load consumed it");

        let mut s = Rig::new();
        assert_state_loaded(stage_state_bytes(&mut s, &[1, 2, 3]));
        assert!(
            load_reply(
                &mut s,
                Some(StagedState {
                    len: 3,
                    state_use: StateUse::Project,
                })
            )
            .is_some(),
            "bytes the plugin cannot parse are refused in the reply"
        );
        assert!(s.plugin.is_some(), "and the plugin is loaded all the same");

        let mut s = Rig::new();
        assert_eq!(
            load_reply(&mut s, None),
            None,
            "nothing staged, nothing refused"
        );
    }

    /// **A stateful load says it is restoring before it does, and a stateless
    /// one never does**: the interim sink hears `Working::Restoring` once, ahead of the
    /// reply.
    #[test]
    #[cfg(feature = "clap")]
    fn a_stateful_load_announces_the_restore() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut source, _shm) = load_clap(FloatWidth::F32);
        let own = save_state_bytes(&mut source);
        let load = |s: &mut Rig, declared: Option<StagedState>| {
            let mut heard = Vec::new();
            let reply = s
                .handle_with(
                    main_thread(),
                    ControlRequest::LoadPlugin {
                        path: std::path::PathBuf::from(clap_plugin()),
                        format: PluginFormat::Clap,
                        plugin: None,
                        sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
                        block_size: 512,
                        preferred_format: FloatWidth::F32,
                        state: declared,
                    },
                    &mut |m| heard.push(m),
                )
                .unwrap()
                .into_reply();
            (heard, reply)
        };
        let mut s = Rig::new();
        assert_state_loaded(stage_state_bytes(&mut s, &own));
        let (heard, reply) = load(
            &mut s,
            Some(StagedState {
                len: own.len() as u64,
                state_use: StateUse::Project,
            }),
        );
        assert!(
            matches!(heard.as_slice(), [Working::Restoring]),
            "{heard:?}"
        );
        assert!(matches!(reply, ControlReply::PluginLoaded { .. }));

        let mut s = Rig::new();
        let (heard, _) = load(&mut s, None);
        assert!(heard.is_empty(), "{heard:?}");
    }

    #[test]
    #[cfg(feature = "clap")]
    fn save_load_state() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let data = save_state_bytes(&mut s);
        let r = load_state_bytes(&mut s, &data);
        assert_state_loaded(r);
    }

    /// Byte-exact round-trip: the bytes a plugin emits must survive the
    /// host<->subprocess IPC boundary and a load+re-save unchanged. Proves
    /// the state path carries the opaque blob verbatim (no UTF-8 assumption,
    /// truncation, or re-encoding) AND that the plugin's own state is stable.
    #[test]
    #[cfg(feature = "clap")]
    fn save_load_state_round_trips_byte_exact() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);

        let first = save_state_bytes(&mut s);
        assert_state_loaded(load_state_bytes(&mut s, &first));
        let second = save_state_bytes(&mut s);

        assert_eq!(
            first,
            second,
            "state blob changed across save → load → save \
             (len {} vs {}) — the IPC path or plugin mutated it",
            first.len(),
            second.len()
        );
    }

    #[test]
    #[cfg(feature = "clap")]
    fn set_sample_rate_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let r = s
            .handle(
                main_thread(),
                ControlRequest::SetSampleRate {
                    rate: tutti_plugin_types::SampleRate::SR_96K,
                },
            )
            .unwrap()
            .into_reply();
        assert!(matches!(r, ControlReply::SampleRateSet { error: None, .. }));
    }

    #[test]
    #[cfg(feature = "clap")]
    fn unload_loaded_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        assert!(s.plugin.is_some());
        assert!(s.side().has_slab());
        let r = s
            .handle(main_thread(), ControlRequest::UnloadPlugin)
            .unwrap();
        assert_none(r);
        assert!(s.plugin.is_none());
        assert!(!s.side().has_slab());
    }

    #[test]
    #[cfg(feature = "clap")]
    fn process_audio_with_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        let reply = s
            .handle_audio(AudioRequest::ProcessAudio(
                tutti_plugin_ipc::ProcessAudioData {
                    seq: 0,
                    num_samples: 512,
                    midi_events: MidiEventVec::new(),
                    param_changes: ParameterChanges::new(),
                    note_expression: NoteExpressionChanges::new(),
                    ..tutti_plugin_ipc::ProcessAudioData::empty(
                        tutti_plugin_types::SampleRate::SR_48K,
                    )
                },
            ))
            .unwrap()
            .expect("answered");
        assert!(
            matches!(reply, AudioReply::AudioProcessed { .. }),
            "unexpected reply: {reply:?}"
        );
    }

    /// The probe bundle `AnyServer::load` picks the format from, as a session
    /// loads it, with a slab sized to what the plugin declared.
    #[cfg(feature = "vst3")]
    fn load_vst3_probe() -> (Rig, SlabWriter) {
        let bundle = tutti_plugin_types::bundle::bundle_dir_of(
            std::path::Path::new(crate::test_utils::vst3_probe_path()),
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .expect("the probe module is inside its bundle")
        .to_path_buf();
        let mut s = Rig::new();
        s.handle(
            main_thread(),
            ControlRequest::LoadPlugin {
                path: bundle,
                format: PluginFormat::Vst3,
                plugin: None,
                sample_rate: tutti_plugin_types::SampleRate::SR_48K,
                block_size: 512,
                preferred_format: FloatWidth::F32,
                state: None,
            },
        )
        .unwrap();
        let loaded = s.plugin.as_ref().unwrap().plugin().loaded().clone();
        let layout = tutti_plugin_ipc::SlabLayout {
            samples_per_channel: 8192,
            format: s.clock.format,
            slots: tutti_plugin_ipc::RING_SLOTS as u32,
            inputs: loaded.input_widths(),
            outputs: loaded.output_widths(),
        };
        let (guard, handle) = AudioSlab::create(layout.clone()).unwrap();
        s.attach(AudioSlab::open(&handle, layout).unwrap());
        (s, guard)
    }

    /// The host's write of `id` to `value`, as the audio lane sends it between
    /// blocks.
    #[cfg(feature = "clap")]
    fn apply_parameter(id: u32, value: f64) -> AudioRequest {
        let mut changes = ParameterChanges::reserved(1, 1);
        assert!(changes.add_change(ParamId::new(id), 0, value));
        AudioRequest::ApplyParameters {
            id: tutti_plugin_ipc::ReqId::new(1),
            param_changes: changes,
        }
    }

    /// One block of `frames` silent frames through `s`'s slab, and whether it
    /// was published.
    #[cfg(any(feature = "vst3", feature = "clap"))]
    fn run_silent_block(s: &mut Rig, shm: &mut SlabWriter, seq: u64, frames: usize) -> bool {
        let layout = shm.layout_ref().clone();
        for ch in 0..layout.input_channels() {
            shm.write::<f32>(seq, ch, &vec![0.0; frames]).unwrap();
        }
        shm.publish(seq);
        let reply = s
            .handle_audio(AudioRequest::ProcessAudio(
                tutti_plugin_ipc::ProcessAudioData {
                    seq,
                    num_samples: frames,
                    ..tutti_plugin_ipc::ProcessAudioData::empty(
                        tutti_plugin_types::SampleRate::SR_48K,
                    )
                },
            ))
            .expect("a failed block does not end the server")
            .expect("answered");
        assert!(
            matches!(reply, AudioReply::AudioProcessed { seq: s, .. } if s == seq),
            "{reply:?}"
        );
        shm.has_output(seq)
    }

    /// **A block the plugin fails is silence and is counted; the server goes
    /// on.** A failure that left `handle` as an error would end the server loop,
    /// and one `kResultFalse` (a preset load, a licence hiccup) would read as a
    /// plugin crash.
    #[test]
    #[cfg(feature = "vst3")]
    fn a_vst3_block_the_plugin_fails_is_silence_and_the_server_goes_on() {
        let _lock = crate::test_utils::plugin_load_lock();
        let failing = {
            // `plugin_load_lock` is held, as every loader test that reads this
            // variable holds it. 4 is `kMisbehaveProcessFails`.
            let mut env = tutti_fixtures::ScopedEnv::new();
            env.set("TUTTI_PROBE_MISBEHAVIOUR", "4");
            let (mut s, mut shm) = load_vst3_probe();
            drop(env);
            for seq in 1..=3 {
                assert!(
                    !run_silent_block(&mut s, &mut shm, seq, 128),
                    "a failed block publishes nothing, so the host reads silence"
                );
            }
            assert_eq!(s.render.losses().failed_blocks, 3);
            assert!(s.drain_async_events(main_thread()).is_empty());
            s.render.losses().failed_blocks
        };
        assert_eq!(failing, 3);
        // The same server code with a plugin that behaves renders and publishes.
        let (mut s, mut shm) = load_vst3_probe();
        assert!(run_silent_block(&mut s, &mut shm, 1, 128));
        assert_eq!(s.render.losses().failed_blocks, 0);
    }

    /// The same for CLAP: a block the plugin fails (`CLAP_PROCESS_ERROR`) is
    /// silence and is counted, and the next block renders.
    ///
    /// A block longer than the activated maximum is refused before any plugin
    /// sees it (`a_block_of_a_length_the_plugin_cannot_take_never_reaches_it`),
    /// so the plugin fails here on its own account, through the probe's status.
    #[test]
    #[cfg(feature = "clap")]
    fn a_clap_block_the_plugin_fails_is_silence_and_the_server_goes_on() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, mut shm) = load_clap(FloatWidth::F32);
        {
            let _error = crate::test_utils::clap_probe_status_mode(
                tutti_clap_test_plugin::StatusMode::Error,
            );
            assert!(!run_silent_block(&mut s, &mut shm, 1, 64));
            assert_eq!(s.render.losses().failed_blocks, 1);
        }
        assert!(
            run_silent_block(&mut s, &mut shm, 2, 64),
            "the next block renders"
        );
        assert_eq!(s.render.losses().failed_blocks, 1);
    }

    /// **An idle dispatch allocates nothing**: the drain every message is
    /// followed by, with the plugin having asked for nothing. The lifecycle is
    /// not entered, so nothing is cloned or queued.
    #[test]
    #[cfg(feature = "vst3")]
    fn an_idle_drain_does_not_allocate_vst3() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_vst3_probe();
        // Warm: the first pass sizes the bridge's request buffer.
        assert!(s.drain_async_events(main_thread()).is_empty());
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..64 {
                assert!(s.drain_async_events(main_thread()).is_empty());
            }
        });
    }

    /// As above, for CLAP.
    #[test]
    #[cfg(feature = "clap")]
    fn an_idle_drain_does_not_allocate_clap() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (mut s, _shm) = load_clap(FloatWidth::F32);
        assert!(s.drain_async_events(main_thread()).is_empty());
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..64 {
                assert!(s.drain_async_events(main_thread()).is_empty());
            }
        });
    }

    /// A plugin that says its parameter list changed gets its parameter
    /// output re-reserved by `drain_async_events`, at the plugin's count.
    ///
    /// The probe's list never really grows, so the output is first shrunk
    /// to one queue, standing in for a load-time count the plugin has since
    /// outgrown; the probe's `restartComponent(kParamTitlesChanged)` is raised
    /// through the host's own handler, as a plugin raises it.
    #[test]
    #[cfg(feature = "vst3")]
    fn a_param_titles_change_reserves_the_parameter_output_again() {
        // `RestartFlags_::kParamTitlesChanged` (`ivsteditcontroller.h`). The
        // `PluginParamTitlesChanged` assertion below pins that this is the bit.
        const K_PARAM_TITLES_CHANGED: i32 = 1 << 4;
        let _lock = crate::test_utils::plugin_load_lock();
        // The load names the `.vst3` bundle, not the module inside it that
        // `vst3_probe_path` names.
        let bundle = tutti_plugin_types::bundle::bundle_dir_of(
            std::path::Path::new(crate::test_utils::vst3_probe_path()),
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .expect("the probe module is inside its bundle")
        .to_path_buf();
        let mut s = Rig::new();
        s.handle(
            main_thread(),
            ControlRequest::LoadPlugin {
                path: bundle,
                format: PluginFormat::Vst3,
                plugin: None,
                sample_rate: tutti_plugin_types::SampleRate::SR_48K,
                block_size: 512,
                preferred_format: FloatWidth::F32,
                state: None,
            },
        )
        .unwrap();
        let params = s
            .plugin
            .as_ref()
            .unwrap()
            .plugin()
            .parameter_count(main_thread());
        assert!(params > 1, "the probe has parameters to refit to");
        assert_eq!(
            s.side().pipeline().param_queues(),
            params,
            "reserved at load"
        );

        s.render.reserve_params(1, ParameterQueue::chunk_room(512));
        assert!(
            s.drain_async_events(main_thread()).is_empty(),
            "nothing raised yet"
        );
        assert_eq!(s.side().pipeline().param_queues(), 1);

        match s.plugin.as_ref().unwrap() {
            AnyServer::Vst3(p) => p.plugin_restart_component(K_PARAM_TITLES_CHANGED),
            #[allow(unreachable_patterns)]
            _ => unreachable!("the probe is a VST3"),
        }
        let messages = s.drain_async_events(main_thread());
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, News::PluginParamTitlesChanged)),
            "the restart reaches the session: {messages:?}"
        );
        assert_eq!(s.side().pipeline().param_queues(), params);
    }

    /// The format's count query, which the load and the refit reserve by,
    /// counts the parameters the list carries (the probes describe every
    /// one, so the two agree exactly).
    #[test]
    #[cfg(all(feature = "clap", feature = "vst3"))]
    fn the_parameter_count_agrees_with_the_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let (s, _shm) = load_clap(FloatWidth::F32);
        let clap = s.plugin.as_ref().unwrap().plugin();
        assert!(clap.parameter_count(main_thread()) > 0);
        assert_eq!(
            clap.parameter_count(main_thread()),
            clap.parameters(main_thread()).len()
        );

        let (vst3, _) = crate::served::Server::<tutti_vst3_host::Vst3Loaded>::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            &tutti_plugin_types::PluginRef::file(crate::test_utils::vst3_probe_path()),
            SampleRate::SR_48K,
            128,
            FloatWidth::F32,
            None,
            &mut || {},
            tutti_plugin_types::Hand::local(),
            crate::ties::ties().expect("bells").0,
        )
        .expect("the vendored VST3 probe loads");
        let vst3: &dyn crate::served::ServerPlugin = &vst3;
        assert!(vst3.parameter_count(main_thread()) > 0);
        assert_eq!(
            vst3.parameter_count(main_thread()),
            vst3.parameters(main_thread()).len()
        );
    }
}
