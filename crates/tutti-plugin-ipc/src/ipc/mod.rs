//! [`Ipc`]: a plugin served by a `plugin-server` process, as a
//! [`Loaded`].
//!
//! It wraps what a served plugin is today: the [`PluginBridge`] (control
//! socket and audio slab), the [`ProcessGuard`] that owns the server, the
//! [`EditorLoader`] the host passed (or none: the editor then lives in the server), and the
//! [`PluginWiring`] descriptor the
//! server reported. [`activate`](Loaded::activate) splits it into an
//! [`IpcProcessor`] (the [`Batcher`] behind
//! [`Processor::process`](tutti_plugin_types::Processor::process)) and an [`IpcController`] (the bridge's control
//! methods behind [`Control`](tutti_plugin_types::Control)).
//!
//! # No format parameter
//!
//! One `Ipc` serves every format by path, as `PluginClient` does: the
//! `plugin-server` loads whichever format the path names, and its AU loader is
//! still its own. It gains `<L: OutOfProcess>` when
//! AU is served through the generic loader.
//!
//! # Threads
//!
//! The served control is **thread-agnostic**: the bridge answers a parameter
//! read, a state save or a render-mode write from any thread. [`Control`](tutti_plugin_types::Control)'s
//! methods take a [`MainThreadMarker`] because the
//! trait is the in-process one, and here the token is ignored, never required:
//! a caller with no token reaches the same bridge through
//! [`Ipc::bridge`] / [`IpcController::bridge`]. Nothing in this module hops to
//! the main thread. A rate, a block ceiling and a quantum are taken without a
//! restart through [`Processor::RETUNE`](tutti_plugin_types::Processor::RETUNE), which is what lets a graph node
//! prepare a served plugin off the main thread.
//!
//! # Reopening
//!
//! [`Reopen::reopen`] takes a token it does not need and gives a fresh,
//! stateless instance; [`Ipc::reopen_restoring`] is the same without the
//! token and with the state restored before the plugin is activated, which is
//! what a fork built on a worker thread needs (`ForkBuild::deferred`). The
//! offline wait of an export's fork is [`Ipc::watch`].

mod control;
mod processor;
#[cfg(test)]
mod tests;

pub use processor::IpcProcessor;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tutti_rt::Exclusive;
use tutti_rt::Mutex;

use tutti_buffers::FloatWidth;
use tutti_platform::{Executor, WeakExecutor};
use tutti_plugin_types::{
    ChunkCounters, Controller, IpcError, IpcResult, Loaded, MainThreadMarker, ParamAccess,
    ParamWriteError, ParameterChanges, ParameterQueue, PluginDescriptor, PluginError, PluginFormat,
    PluginRef, PluginResult, PluginTail, PluginWiring, Prepare, PresetId, ProcessWake, Refused,
    Reopen, Request, RestartRules, ResyncKind, SampleRate, Samples, StateError, StateUse, Stuck,
    Told, WaitSwitch, Widths,
};
use tutti_rt::AtomicCell;
use tutti_units::ParamId;

use crate::subprocess::swap_slab;
use crate::{
    AudioSlab, Batcher, EditorLoader, IpcConfig, IpcEvent, IpcListener, LoadRequest, OfflineWait,
    PluginBridge, ProcessGuard, Replies, Restore, SlabWriter, launch,
};

/// Where a served plugin came from: what [`Ipc::reopen_restoring`] launches
/// again.
#[derive(Clone)]
pub struct LaunchSpec {
    /// The launch's configuration. A reopen gives it a socket of its own.
    pub config: IpcConfig,
    /// The plugin file, and which plugin of it the first launch chose.
    pub origin: PluginRef,
    /// The format that opens it, as the first launch named it.
    pub format: PluginFormat,
    /// Loads the editor-only instance, as the first launch's did; `None`
    /// again for an editor in the server.
    pub load_editor: Option<EditorLoader>,
    /// Reaches the executor that runs the new server's exit watch, as the first
    /// launch's did, while it lives.
    pub executor: WeakExecutor,
}

impl LaunchSpec {
    /// A fresh instance of the plugin, from the same file and configuration
    /// in a server of its own (its own socket), at `rate`, with `state`
    /// restored before it is activated, as the state of a duplicate. See
    /// [`Ipc::reopen_restoring`].
    ///
    /// # Errors
    ///
    /// [`IpcError::ShutDown`] once the executor has shut down, else as
    /// [`Ipc::launch`].
    pub fn reopen(&self, rate: SampleRate, state: Option<&[u8]>) -> IpcResult<Launched> {
        let executor = self.executor.upgrade().ok_or(IpcError::ShutDown)?;
        let config = self.config.clone();
        Ipc::launch(
            config,
            &LoadRequest {
                origin: &self.origin,
                format: self.format,
                rate,
                restore: state.map(|state| Restore {
                    state,
                    state_use: StateUse::Duplicate,
                }),
            },
            self.load_editor.clone(),
            &executor,
        )
    }
}

impl std::fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("path", &self.origin.path())
            .finish_non_exhaustive()
    }
}

/// What a served plugin is, once its server is running and its bridge is up:
/// the input of [`Ipc::new`].
pub struct IpcParts {
    /// The control socket and audio slab, as the client side.
    pub bridge: Arc<PluginBridge>,
    /// The server's lifetime: it dies when the last holder drops.
    pub guard: Arc<ProcessGuard>,
    /// The plugin's catalog identity.
    pub descriptor: PluginDescriptor,
    /// What the server reported at load.
    pub loaded: PluginWiring,
    /// The format the blocks travel in.
    pub format: FloatWidth,
    /// The slab's per-chunk size: the most a chunk can be.
    pub ceiling: usize,
    /// The slab's input writer.
    pub slab: SlabWriter,
    /// The consuming end of the bridge's response ring, which
    /// [`PluginBridge::new`] returns beside the bridge.
    pub replies: Replies,
    /// The rate the server was launched at.
    pub rate: SampleRate,
    /// How many parameters the plugin lists: what the bridge's parameter
    /// buffers are sized by.
    pub parameters: usize,
    /// The per-chunk wait of an offline fork (`Timeouts::call`).
    pub timeout: Duration,
    /// Where it came from, or `None` for one that cannot be reopened.
    pub origin: Option<LaunchSpec>,
}

impl std::fmt::Debug for IpcParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcParts")
            .field("descriptor", &self.descriptor)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

/// What the bridge's thread reports, kept for the control side: the figures
/// the plugin re-reports, and the requests that follow from them.
struct Events {
    latency: AtomicUsize,
    tail: Mutex<PluginTail>,
    requests: Mutex<Vec<Request>>,
    /// What the server answered to the rate and render mode the host told it,
    /// oldest first, until the control side takes them
    /// ([`Control::take_answers`](tutti_plugin_types::Control::take_answers)).
    told: Mutex<Vec<Told>>,
    forward: Mutex<Option<IpcListener>>,
    /// What the plugin's request to be processed again raises, from the
    /// bridge's thread ([`Control::bind_wake`](tutti_plugin_types::Control::bind_wake)).
    wake: OnceLock<ProcessWake>,
    /// The parameters the plugin declares: see [`DeclaredCache`].
    declared: DeclaredCache,
    /// The bridge whose table of the host's parameter slots follows the same
    /// word: the plugin saying its declarations changed.
    audio: crate::AudioBridge,
    /// The buses the plugin last announced (`PluginIoChanged`); `None` until it
    /// announces any, when the buses it had at load stand.
    layout: Mutex<Option<tutti_plugin_types::Layout>>,
}

/// The parameters the plugin declares and whether each is read-only, sorted by
/// id: asked of the server once, then kept, and forgotten when the plugin says
/// its declarations may have changed (a `Resync` of its parameter titles, a
/// reload or its buses) or dies. The write policy reads it, so a write does not
/// cost a round trip.
///
/// `generation` counts the times it was forgotten, changed under the same lock
/// as the list: a refill that began before a forgetting does not store its
/// (older) answer. A change the plugin makes before its notice reaches the
/// client is not seen until the notice arrives; that window is inherent to
/// asking once.
#[derive(Default)]
struct DeclaredCache {
    state: Mutex<DeclaredState>,
}

#[derive(Default)]
struct DeclaredState {
    list: Option<Arc<[(ParamId, ParamAccess)]>>,
    generation: u64,
}

impl DeclaredCache {
    /// The kept list, or the generation a refill must pass to [`store`](Self::store).
    fn get(&self) -> Result<Arc<[(ParamId, ParamAccess)]>, u64> {
        let state = self.state.lock();
        state.list.clone().ok_or(state.generation)
    }

    /// Keeps `list`, read when the generation was `started`, unless a forgetting
    /// has made it older than the plugin's word.
    fn store(&self, started: u64, list: &Arc<[(ParamId, ParamAccess)]>) {
        let mut state = self.state.lock();
        if state.generation == started {
            state.list = Some(Arc::clone(list));
        }
    }

    /// Forgets the list, and makes a refill that is under way not store.
    fn forget(&self) {
        let mut state = self.state.lock();
        state.list = None;
        state.generation += 1;
    }
}

impl Events {
    fn on(&self, event: IpcEvent) {
        match &event {
            IpcEvent::LatencyChanged { samples } => {
                self.latency.store(samples.get(), Ordering::Release);
                self.ask(Request::LatencyChanged);
            }
            IpcEvent::TailChanged { tail } => {
                *self.tail.lock() = *tail;
                self.ask(Request::TailChanged);
            }
            IpcEvent::IoLayout(layout) => {
                *self.layout.lock() = Some(layout.clone());
            }
            IpcEvent::Told(told) => self.told.lock().push(told.clone()),
            // The restart the core plans for it ([`RESTARTS`](Loaded::RESTARTS))
            // lays out the slab, the pipeline and the node for the buses the
            // layout above names; the pump that takes the request reports it.
            // A bus change also changes what the plugin declares (its
            // parameters may follow its buses).
            IpcEvent::Resync(ResyncKind::Io) => {
                self.declared.forget();
                self.audio.invalidate_host_edits();
                self.ask(Request::IoChanged);
            }
            // The declared controller table is read again, through the request
            // the plugin raises for it in process: the pump re-reads the wiring.
            IpcEvent::MidiMapChanged => self.ask(Request::MidiCcAssignmentChanged),
            IpcEvent::ProcessRequested => {
                if let Some(wake) = self.wake.get() {
                    wake.raise();
                }
            }
            // What can change the declarations: the parameter list itself, a
            // rebuilt instance, a bus change (a plugin's parameters may follow
            // its buses), and death (a dead plugin has none). Not `ParamValues`,
            // which every dropped parameter event raises and which says nothing
            // of the list: a forget on it would make the next write a round trip.
            IpcEvent::Resync(ResyncKind::ParamTitles | ResyncKind::Reloaded)
            | IpcEvent::Crashed { .. } => {
                self.declared.forget();
                self.audio.invalidate_host_edits();
            }
            // A transport request reaches the application through the
            // listener, in order; nothing is queued for it here.
            _ => {}
        }
        let forward = self.forward.lock().clone();
        if let Some(forward) = forward {
            forward(event);
        }
    }

    fn ask(&self, request: Request) {
        self.requests.lock().push(request);
    }
}

/// What the three halves of a served plugin share.
pub(crate) struct Link {
    pub(crate) bridge: Arc<PluginBridge>,
    pub(crate) guard: Arc<ProcessGuard>,
    pub(crate) descriptor: PluginDescriptor,
    pub(crate) loaded: PluginWiring,
    pub(crate) format: FloatWidth,
    pub(crate) ceiling: usize,
    pub(crate) parameters: usize,
    pub(crate) timeout: Duration,
    pub(crate) origin: Option<LaunchSpec>,
    pub(crate) counters: Arc<ChunkCounters>,
    /// The slab `counters` asks whether a chunk was answered: the one the
    /// current activation renders through.
    answering: Arc<Mutex<Arc<AudioSlab>>>,
    events: Arc<Events>,
    /// The rate the server runs at.
    rate: AtomicCell<SampleRate>,
    /// A fork's watch: the latches it reads back, and whether the pipeline
    /// waits for each chunk (an offline fork).
    wait: Mutex<Option<Watch>>,
}

impl Link {
    /// The plugin as the host knows it now: what it reported at load, with the
    /// buses it announced since. An activation reads it once and lays out the
    /// slab, the pipeline and the node's ports from that one value.
    pub(crate) fn current(&self) -> PluginWiring {
        let mut loaded = self.loaded.clone();
        if let Some(layout) = self.layout() {
            loaded.inputs = layout.inputs;
            loaded.outputs = layout.outputs;
        }
        loaded
    }

    /// The buses the plugin last announced, if it announced any.
    pub(crate) fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.events.layout.lock().clone()
    }

    /// The latency the plugin reports now.
    pub(crate) fn latency(&self) -> Samples {
        Samples::new(self.events.latency.load(Ordering::Acquire))
    }

    /// The plugin's declared parameters, sorted by id, from the cache the server's
    /// last answer filled; asks the server only when there is none (the first
    /// write, or the first after a resync). News the plugin sent that the bridge
    /// has not yet processed is settled first, so a resync the bridge is about to
    /// deliver is not missed (`AudioBridge::news_settled`; news not yet read off the
    /// connection is the one window left, as for any asked-once list). Checked in
    /// this order: a crashed plugin is [`Lost`](ParamWriteError::Lost) (and its
    /// cache is forgotten); one that does not settle or answer in time is
    /// [`Unanswered`](ParamWriteError::Unanswered).
    ///
    /// **It waits**: the settle sleep-polls for as long as the bridge has news
    /// pending, up to the link's timeout, and a bridge wedged inside its listener
    /// makes the write fail `Unanswered` after that. Control thread only; the
    /// realtime path reads no cache.
    ///
    /// # Errors
    ///
    /// As above, naming `id`, the parameter the caller was writing.
    pub(crate) fn declared(
        &self,
        id: ParamId,
    ) -> Result<Arc<[(ParamId, ParamAccess)]>, ParamWriteError> {
        if self.bridge.audio().is_crashed() {
            self.events.declared.forget();
            return Err(ParamWriteError::Lost(id));
        }
        if !self.bridge.audio().news_settled(self.timeout) {
            return Err(if self.bridge.audio().is_crashed() {
                ParamWriteError::Lost(id)
            } else {
                ParamWriteError::Unanswered(id)
            });
        }
        let started = match self.events.declared.get() {
            Ok(kept) => return Ok(kept),
            Err(generation) => generation,
        };
        let Some(infos) = self.bridge.audio().parameters() else {
            return Err(if self.bridge.audio().is_crashed() {
                ParamWriteError::Lost(id)
            } else {
                ParamWriteError::Unanswered(id)
            });
        };
        let mut sorted: Vec<(ParamId, ParamAccess)> = infos
            .iter()
            .map(|info| (info.id(), ParamAccess::of(info)))
            .collect();
        sorted.sort_unstable_by_key(|&(id, _)| id);
        let list: Arc<[(ParamId, ParamAccess)]> = sorted.into();
        self.events.declared.store(started, &list);
        // The host's parameter slots are built from the list the policy asked for.
        self.bridge
            .audio()
            .declare_host_edits(list.iter().map(|&(id, _)| id));
        Ok(list)
    }

    pub(crate) fn tail(&self) -> PluginTail {
        *self.events.tail.lock()
    }

    pub(crate) fn rate(&self) -> SampleRate {
        self.rate.load()
    }

    pub(crate) fn set_rate(&self, rate: SampleRate) {
        self.rate.store(rate);
    }

    pub(crate) fn watch(&self) -> Option<Watch> {
        self.wait.lock().clone()
    }

    /// The queues and points of the bridge's parameter buffers for chunks of
    /// `chunk` frames: a queue per listed parameter and the spares the
    /// node's delivery may learn an address into.
    pub(crate) fn param_room(&self, chunk: usize) -> (usize, usize) {
        (
            self.parameters + ParameterChanges::SPARE_QUEUES,
            ParameterQueue::chunk_room(chunk),
        )
    }

    /// Reserves the bridge's parameter buffers for chunks of `chunk` frames.
    /// Control thread: allocates.
    pub(crate) fn reserve(&self, chunk: usize) {
        let (queues, points) = self.param_room(chunk);
        self.bridge.audio().reserve_param_changes(queues, points);
    }
}

/// A fork's watch on its pipeline ([`Ipc::watch`]).
#[derive(Clone)]
pub(crate) struct Watch {
    pub(crate) wait: Arc<OfflineWait>,
}

/// A served plugin that is configured and not rendering.
///
/// See the module docs. Built by [`Ipc::launch`] (a new server) or
/// [`Ipc::new`] (a server already running).
pub struct Ipc {
    pub(crate) link: Arc<Link>,
    slab: SlabWriter,
    /// The consuming end of the bridge's response ring, handed to each
    /// activation's pipeline and back.
    /// An [`Exclusive`] for `Ipc: Sync`, which `impl AnyThread for Ipc` (`Send + Sync`)
    /// and the contract tests that call `Control` on an `Ipc` require, and a
    /// ring reader is not `Sync`: an activation takes it by value (`into_inner`).
    /// Boxed, the one box of the reader: its cached indices are large, and an
    /// `Ipc` is the `Ok` and `Err` payload of an activation
    /// (`result_large_err`).
    replies: Box<Exclusive<Replies>>,
    /// The sequence the slab's next chunk carries: the slab outlives each
    /// activation's batcher, so the sequence does too.
    next_seq: u64,
    /// A slab setup failed, so the server may render through a slab the host
    /// does not hold: the next activation sets one up even for the layout the
    /// host holds.
    slab_unsure: bool,
}

impl std::fmt::Debug for Ipc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ipc")
            .field("name", &self.link.descriptor.name)
            .finish_non_exhaustive()
    }
}

/// What a reopen did with the state it was given.
#[derive(Debug)]
pub struct Launched {
    /// The fresh instance, not activated.
    pub ipc: Ipc,
    /// `None` when no state was given, else the refusal (the
    /// plugin is then loaded at its defaults, which beats no plugin).
    pub restored: Option<std::result::Result<(), StateError>>,
}

/// How many parameters the server lists for the plugin `bridge` serves.
///
/// # Errors
///
/// [`IpcError::ParametersUnlisted`] when the server gives no list. A plugin with
/// no parameters lists none, which is an answer.
fn listed_parameters(bridge: &PluginBridge) -> IpcResult<usize> {
    bridge
        .audio()
        .parameters()
        .map(|list| list.len())
        .ok_or(IpcError::ParametersUnlisted)
}

impl Ipc {
    /// A served plugin over a running server. Installs the bridge's listener
    /// (the figures the plugin re-reports); a host that wants the events too
    /// forwards them with [`forward_events`](Self::forward_events).
    pub fn new(parts: IpcParts) -> Self {
        let IpcParts {
            bridge,
            guard,
            descriptor,
            loaded,
            format,
            ceiling,
            slab,
            replies,
            rate,
            parameters,
            timeout,
            origin,
        } = parts;
        let counters = Arc::new(ChunkCounters::default());
        // The slab the pipeline reads, which an activation may replace: read
        // by the control side's diagnostics only, never by the audio thread.
        let answering = Arc::new(Mutex::new(Arc::clone(slab.slab())));
        {
            let answering = Arc::clone(&answering);
            counters.attach(move |seq| answering.lock().has_output(seq));
            counters.attach_skipped(bridge.audio().skipped_cell());
            counters.attach_commands_refused(bridge.audio().commands_refused_cell());
            counters.attach_responses_lost(bridge.audio().responses_lost_cell());
            counters.attach_leaked_buffers(bridge.audio().leaked_buffers_cell());
            counters.attach_detached(guard.watch().detached_cell());
            if let Some(refused) = bridge.editor().mirror_transfers_refused_cell() {
                counters.attach_mirror_transfers_refused(refused);
            }
            if let Some(dropped) = bridge.editor().editor_requests_dropped_cell() {
                counters.attach_editor_requests_dropped(dropped);
            }
        }
        let events = Arc::new(Events {
            latency: AtomicUsize::new(loaded.latency_samples.get()),
            tail: Mutex::new(loaded.tail),
            requests: Mutex::new(Vec::new()),
            told: Mutex::new(Vec::new()),
            forward: Mutex::new(None),
            wake: OnceLock::new(),
            declared: DeclaredCache::default(),
            audio: bridge.audio().clone(),
            layout: Mutex::new(None),
        });
        let listening = Arc::clone(&events);
        bridge
            .audio()
            .set_listener(Some(Arc::new(move |event| listening.on(event))));
        let link = Link {
            bridge,
            guard,
            descriptor,
            loaded,
            format,
            ceiling,
            parameters,
            timeout,
            origin,
            counters,
            answering,
            events,
            rate: AtomicCell::new(rate),
            wait: Mutex::new(None),
        };
        Self {
            link: Arc::new(link),
            slab,
            replies: Box::new(Exclusive::new(replies)),
            next_seq: 1,
            slab_unsure: false,
        }
    }

    /// Launches a `plugin-server` for the plugin `request` names and returns the
    /// plugin loaded in it, with what became of its state if it had one: the
    /// state is restored **before the plugin is activated**, the order every
    /// format allows and a plugin that takes its mode from the state at
    /// activation needs.
    ///
    /// Blocks while the server launches and handshakes. Needs no main thread.
    ///
    /// `load_editor` is where the editor lives: `Some` loads an editor-only
    /// second instance in this process (CLAP, VST3, AU), `None` opens it on the
    /// served instance in the server, parented to the host's window.
    ///
    /// `executor` runs the server's exit watch; the plugin's origin keeps a weak
    /// handle to it, so a reopen watches its server on the same one while it lives.
    ///
    /// # Errors
    ///
    /// The server cannot be found or spawned, the handshake fails or times
    /// out, the plugin fails to load, the bridge cannot be set up, or the server
    /// does not list the plugin's parameters
    /// ([`IpcError::ParametersUnlisted`]).
    pub fn launch(
        config: IpcConfig,
        request: &LoadRequest<'_>,
        load_editor: Option<EditorLoader>,
        executor: &Executor,
    ) -> IpcResult<Launched> {
        let server = launch(&config, request, executor)?;
        let LoadRequest { format, rate, .. } = *request;
        // The plugin the server opened, so a reopen and the editor instance
        // load that one, though the request chose none.
        let origin = &request.origin.identified(server.id.clone());
        let restored = server.state_restore.clone();
        // Guarded since the spawn: an `Err` from `PluginBridge::new` drops the
        // guard, which kills and reaps the child. The bridge shares the guard's
        // watch, so the stream's end and the process's end set one latch.
        let mut guard = server.server;
        let (bridge, thread, replies) = PluginBridge::new(
            executor,
            server.audio,
            server.control,
            origin.clone(),
            format,
            load_editor.clone(),
            rate,
            Arc::clone(guard.watch()),
            config.timeouts,
        )?;
        guard.attach(thread);
        // The parameter list, asked here and by `over`, while loading blocks
        // anyway: the bridge's buffers are sized by it at activation, which
        // may not make the trip.
        let parameters = listed_parameters(&bridge)?;
        let ceiling = config.max_buffer_size.min(tutti_plugin_types::MAX_CHUNK);
        let timeout = config.timeouts.call;
        let ipc = Self::new(IpcParts {
            bridge,
            guard: Arc::new(guard),
            descriptor: server.descriptor,
            loaded: server.loaded,
            format: server.format,
            ceiling,
            slab: server.slab,
            replies,
            rate,
            parameters,
            timeout,
            origin: Some(LaunchSpec {
                config,
                origin: origin.clone(),
                format,
                load_editor,
                executor: executor.downgrade(),
            }),
        });
        Ok(Launched { ipc, restored })
    }

    /// A fresh instance of this plugin, from the same file and configuration
    /// in a server of its own (its own socket), at the rate this one runs at,
    /// with `state` restored before it is activated. Not activated, and with
    /// none of this one's other state.
    ///
    /// **Token-free**, which [`Reopen::reopen`] cannot be: it blocks for a
    /// server launch and a state load, half a second or more, and needs
    /// nothing of the main thread, so a fork that builds on a worker (the
    /// graph's `ForkBuild::deferred`) calls it there. The state is read from
    /// the live instance on the control thread first
    /// ([`IpcController::save_state`](tutti_plugin_types::Control::save_state), or the bridge's
    /// own).
    ///
    /// # Errors
    ///
    /// This plugin has no origin (it was built by [`Ipc::new`] with none), or
    /// as [`launch`](Self::launch).
    pub fn reopen_restoring(&self, state: Option<&[u8]>) -> IpcResult<Launched> {
        let Some(origin) = &self.link.origin else {
            return Err(IpcError::NoOrigin);
        };
        origin.reopen(self.link.rate(), state)
    }

    /// Loads the preset `id` into this plugin: `false` when it refused it. No
    /// main thread: the served control is thread-agnostic. A served fork
    /// restores with this, then [`load_state`](Self::load_state), after the
    /// server has loaded the state and activated the plugin at launch.
    pub fn load_preset(&self, id: &PresetId) -> bool {
        self.link.bridge.audio().load_preset(id.clone())
    }

    /// Loads `state` into this plugin, into its audio instance alone (an
    /// instance no editor mirrors, a fresh fork's), so it needs no main
    /// thread.
    ///
    /// # Errors
    ///
    /// The plugin is dead, refused the state, or the transfer stalled.
    pub fn load_state(&self, state: &[u8], state_use: StateUse) -> Result<(), StateError> {
        self.link.bridge.audio().load_state(state, state_use)
    }

    /// Watches this plugin as a fork: the returned [`OfflineWait`] holds what
    /// the pipeline latches and the latency the fork's graph is planned
    /// against. A retune settles the server's queued rate and mode changes and
    /// records that latency (`IpcProcessor::retune_live`), live as offline: the
    /// health reports a later move as a fault.
    ///
    /// The pipeline waits for each chunk, up to the configuration's timeout,
    /// only while the wait is armed, and the [`WaitSwitch`] returned beside it
    /// is the only writer: the one the fork's node owns, which its `prepare`
    /// writes (armed for a graph prepared offline, the offline mode of a forked
    /// instance, and disarmed otherwise, so the audio thread of a realtime
    /// graph never blocks). The wait only reads it, so nothing that holds the
    /// wait can arm it. The plugin's own render mode is separate: telling it
    /// offline arms nothing. Call before [`activate`](Loaded::activate).
    pub fn watch(&mut self) -> (Arc<OfflineWait>, WaitSwitch) {
        let (wait, switch) = OfflineWait::new(
            self.link.timeout,
            Arc::clone(self.link.bridge.audio().watch()),
        );
        let wait = Arc::new(wait);
        self.link
            .bridge
            .audio()
            .offer_refusals_to(Arc::clone(&wait));
        *self.link.wait.lock() = Some(Watch {
            wait: Arc::clone(&wait),
        });
        (wait, switch)
    }

    /// Sends every event the bridge's thread reports (a latency change, a
    /// parameter the plugin moved, a resync, a refused rate, a crash) to
    /// `listener` as well. Invoked on the bridge thread: cheap, and never
    /// blocking. `None` stops forwarding.
    pub fn forward_events(&self, listener: Option<IpcListener>) {
        *self.link.events.forward.lock() = listener;
    }

    /// The bridge: the control socket and audio slab, for a caller that has
    /// no [`MainThreadMarker`] to hold (the served control is thread-agnostic).
    pub fn bridge(&self) -> &Arc<PluginBridge> {
        &self.link.bridge
    }

    /// What the plugin is: its catalog identity.
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.link.descriptor
    }

    /// What the server reported at load.
    pub fn loaded(&self) -> &PluginWiring {
        &self.link.loaded
    }

    /// Where it came from, for a fork to reopen (`None`: built by
    /// [`Ipc::new`] with none).
    pub fn origin(&self) -> Option<&LaunchSpec> {
        self.link.origin.as_ref()
    }

    /// The slab's per-chunk size: the most a chunk can be.
    pub fn ceiling(&self) -> usize {
        self.link.ceiling
    }

    /// The format the blocks travel in.
    pub fn format(&self) -> FloatWidth {
        self.link.format
    }

    /// The rate the pipeline's blocks are at: the last one asked of the
    /// server, which a standing refusal says it does not run at.
    pub fn rate(&self) -> SampleRate {
        self.link.rate()
    }

    /// The server's lifetime guard.
    pub fn guard(&self) -> &Arc<ProcessGuard> {
        &self.link.guard
    }

    /// Another view of this plugin's control side, for a handle: it shares the
    /// link, so it answers from the same bridge and ends the same server
    /// ([`Health::terminate`](tutti_plugin_types::Health::terminate)).
    pub fn controller(&self) -> IpcController {
        IpcController {
            link: Arc::clone(&self.link),
            activated: None,
        }
    }

    /// The pipeline's counters: the chunk in flight and the chunks the server
    /// had not answered. For the node's controls.
    pub fn chunk_counters(&self) -> &Arc<ChunkCounters> {
        &self.link.counters
    }
}

/// The main-thread half of an activated served plugin: the bridge's control
/// methods behind [`Control`](tutti_plugin_types::Control), and the latency and tail it re-reports behind
/// [`Controller`].
#[derive(Clone)]
pub struct IpcController {
    pub(crate) link: Arc<Link>,
    /// The buses the activation this controller belongs to was laid out for:
    /// what its [`wiring`](tutti_plugin_types::Control::wiring) answers, whatever the plugin has announced
    /// since. `None` for a view that belongs to no activation, which answers
    /// the buses announced last.
    activated: Option<Arc<PluginWiring>>,
}

impl std::fmt::Debug for IpcController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcController")
            .field("name", &self.link.descriptor.name)
            .finish_non_exhaustive()
    }
}

impl Ipc {
    /// What `Control::wiring` answers: the buses announced last.
    pub(crate) fn wiring_now(&self) -> PluginWiring {
        self.link.current()
    }
}

impl IpcController {
    /// Hands the server's plugin the app's controller entries and returns who
    /// applies them ([`Control::set_app_midi_map`](tutti_plugin_types::Control::set_app_midi_map)
    /// with no main-thread marker, which the wire does not need): for a fork
    /// built off the main thread.
    pub fn hand_app_midi_map(
        &self,
        app: &[tutti_plugin_types::MidiAssignment],
    ) -> tutti_plugin_types::MidiMapApplied {
        self.link.bridge.audio().set_app_midi_map(app)
    }

    /// What `Control::wiring` answers: the buses of the activation this
    /// controller belongs to, or the ones announced last for a view of none.
    pub(crate) fn wiring_now(&self) -> PluginWiring {
        self.activated
            .as_deref()
            .cloned()
            .unwrap_or_else(|| self.link.current())
    }

    /// The buses the plugin has now: those it last announced
    /// (`PluginIoChanged`), else those it reported at load: the trigger for a
    /// restart, not the buses of an activation. The slab, the pipeline and the
    /// node's ports are laid out from the one reading an activation takes
    /// (`Control::wiring` of its controller).
    #[must_use]
    pub fn layout(&self) -> tutti_plugin_types::Layout {
        let loaded = self.link.current();
        tutti_plugin_types::Layout {
            inputs: loaded.inputs,
            outputs: loaded.outputs,
        }
    }

    /// The bridge, for a caller that has no [`MainThreadMarker`] to hold.
    pub fn bridge(&self) -> &Arc<PluginBridge> {
        &self.link.bridge
    }

    /// The server's OS process id, fixed at launch.
    pub fn server_pid(&self) -> Option<u32> {
        self.link.guard.pid()
    }
}

impl Controller for IpcController {
    type Loaded = Ipc;
    type Processor = IpcProcessor;

    /// The plugin's own latency as it reports it now. The pipeline's chunk,
    /// which out-of-process audio adds (it is submitted now and collected a
    /// chunk later), is the Processor's ([`Processor::latency`](tutti_plugin_types::Processor::latency)): the node
    /// declares both, and that is what turns the chunk into compensated delay.
    fn latency(&self, _main: MainThreadMarker) -> Samples {
        self.link.latency()
    }

    fn tail(&self, _main: MainThreadMarker) -> PluginTail {
        self.link.tail()
    }
}

impl Ipc {
    /// [`Loaded::activate`] with no token: the served activation reads none,
    /// so a node over a served plugin is built on any thread (a fork's
    /// deferred build, a host with no main thread).
    ///
    /// # Errors
    ///
    /// The server refused the rate: the plugin comes back loaded, still at the
    /// rate it ran at.
    pub fn activate_anywhere(
        self,
        prepare: Prepare,
    ) -> Result<(IpcProcessor, IpcController), Refused<Self>> {
        let Self {
            link,
            slab,
            replies,
            next_seq,
            slab_unsure,
        } = self;
        let replies = (*replies).into_inner();
        // The one reading of the buses this activation lays out: the slab, the
        // pipeline's widths and (through `Control::wiring` at the same restart)
        // the node's ports follow it. A change after it is the next restart's.
        let snapshot = link.current();
        let slab = match swap_slab(
            &link.bridge,
            &snapshot,
            link.format,
            link.ceiling,
            slab.layout_ref(),
            slab_unsure,
        ) {
            Ok(None) => slab,
            Ok(Some(new)) => {
                *link.answering.lock() = Arc::clone(new.slab());
                // The slab the server renders through now; the old one goes
                // with the writer, after the server's ack.
                drop(slab);
                new
            }
            Err(why) => {
                return Err(Refused {
                    plugin: Some(Self {
                        link,
                        slab,
                        replies: Box::new(Exclusive::new(replies)),
                        next_seq,
                        // The server may have taken the slab before the
                        // answer was lost.
                        slab_unsure: true,
                    }),
                    reason: PluginError::Other(why.to_string()),
                });
            }
        };
        let rate = prepare.rate();
        // The server hears the rate whenever it may not run at it: a rate the
        // link was not at, or one a refusal stands against (a retune moves
        // the link's rate when it asks). Its answer is the activation's: a
        // refusal is `Refused`, and nothing else reports it.
        if rate != link.rate() || link.bridge.audio().rate_refusal().is_some() {
            match link.bridge.audio().set_sample_rate_now(rate) {
                Ok(()) => link.set_rate(rate),
                Err(refusal) => {
                    return Err(Refused {
                        plugin: Some(Self {
                            link,
                            slab,
                            replies: Box::new(Exclusive::new(replies)),
                            next_seq,
                            slab_unsure,
                        }),
                        reason: PluginError::Bridge(Box::new(IpcError::RateRefused(refusal))),
                    });
                }
            }
        }
        // The widths are the slab's own, which is the snapshot's: one source.
        let mut batcher = Batcher::new(
            slab.layout_ref().input_channels(),
            slab.layout_ref().output_channels(),
            link.format,
            link.ceiling,
            slab,
            replies,
        )
        .starting_at(next_seq)
        .with_counters(Arc::clone(&link.counters));
        batcher.prepare(
            Samples::new(prepare.max_block().get()),
            prepare.quantum().map(tutti_plugin_types::MaxBlock::samples),
        );
        if let Some(Watch { wait }) = link.watch() {
            // A wait already latched (a reactivation after a give-up or a
            // death) stays on the live bound: the batcher collects at once, and
            // the bridge must not hold one budget per chunk for a server that
            // did not answer. Unarmed, it waits for nothing.
            link.bridge.audio().set_reply_wait(wait.reply_wait());
            batcher.set_offline_wait(wait);
        } else {
            link.bridge.audio().set_reply_wait(None);
        }
        link.reserve(batcher.chunk());
        let processor = IpcProcessor::new(Arc::clone(&link), batcher);
        Ok((
            processor,
            IpcController {
                link,
                activated: Some(Arc::new(snapshot)),
            },
        ))
    }
}

impl Loaded for Ipc {
    type Processor = IpcProcessor;
    type Controller = IpcController;

    /// A change of the plugin's buses restarts it: the activation lays out a
    /// slab, a pipeline and a node for the new ones. A rate, a block and a
    /// latency change are re-read or taken live.
    const RESTARTS: RestartRules = RestartRules {
        io: true,
        latency: false,
    };

    /// The width the load settled on. The slab and the server's pipeline run
    /// that width and cannot run the other without a reload, and the load reply
    /// names the settled width, not the plugin's capability, so a load that
    /// asked for 32-bit reports 32-bit.
    fn widths(&self) -> Widths {
        match self.link.format {
            FloatWidth::F32 => Widths::F32,
            FloatWidth::F64 => Widths::BOTH,
        }
    }

    /// Builds the pipeline for `prepare`'s block and the Processor over it.
    /// The server is told no rate here: it was launched at one, and a node
    /// that follows another tells it through [`Processor::RETUNE`](tutti_plugin_types::Processor::RETUNE).
    fn activate(
        self,
        _main: MainThreadMarker,
        prepare: Prepare,
    ) -> Result<(IpcProcessor, IpcController), Refused<Self>> {
        self.activate_anywhere(prepare)
    }

    fn deactivate(
        _main: MainThreadMarker,
        processor: IpcProcessor,
        controller: IpcController,
    ) -> Result<Self, Stuck<IpcProcessor, IpcController>> {
        if !Arc::ptr_eq(&processor.link, &controller.link) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other(
                    "a processor and a controller of two activations".into(),
                ),
            });
        }
        let (slab, next_seq, replies) = processor.into_parts();
        Ok(Self {
            link: controller.link,
            slab,
            replies: Box::new(Exclusive::new(replies)),
            next_seq,
            slab_unsure: false,
        })
    }
}

impl Reopen for Ipc {
    /// [`Ipc::reopen_restoring`] with no state. The token is not needed.
    fn reopen(&self, _main: MainThreadMarker) -> PluginResult<Self> {
        self.reopen_restoring(None)
            .map(|reopened| reopened.ipc)
            .map_err(|e| PluginError::Other(e.to_string()))
    }
}
