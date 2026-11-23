//! The client of an out-of-process plugin ([`PluginClient`]): the generic plugin node
//! (`tutti_plugin_host::PluginNode`) over the out-of-process hosting
//! (`tutti_plugin_ipc::Ipc`), with the control surface the host keeps beside it.
//!
//! There is one node. `PluginClient` is what loads and launches the
//! `plugin-server`, builds the node over its [`Ipc`] (the
//! pipeline, the MIDI-out, the transport walk across a chunk, the parameter
//! delivery are the generic node's and `Ipc`'s own), and keeps what is not the
//! node's: the [`PluginControls`], the handle's sinks, and a fork source.
//!
//! # Loaded, then bound
//!
//! A client is born [`Unbound`] ([`PluginClient::new`]): a loaded plugin with
//! its control surface, not yet a node. [`bind`](PluginClient::bind) turns it
//! into a [`PluginClient<Bound>`], which holds the node and is the only state
//! that goes into a graph ([`tutti_graph::IntoNode`]). Inserting it hands back
//! its [`PluginControls`]:
//!
//! ```no_run
//! use tutti_graph::Editor;
//! use tutti_plugin::PluginClient;
//! use tutti_plugin_host::PluginControls;
//! use tutti_graph::NodeKey;
//! use tutti_timeline::TempoReader;
//! fn insert(client: PluginClient, tempo: TempoReader, editor: &mut Editor) -> PluginControls {
//!     editor.insert(NodeKey(1), "plugin", client.bind(tempo)).controls
//! }
//! ```
//!
//! An unbound plugin can be neither inserted into a graph —
//!
//! ```compile_fail,E0277
//! use tutti_graph::Editor;
//! use tutti_plugin::PluginClient;
//! use tutti_graph::NodeKey;
//! fn insert(client: PluginClient, editor: &mut Editor) {
//!     // `PluginClient` is `PluginClient<Unbound>`: not a node.
//!     editor.insert(NodeKey(1), "plugin", client);
//! }
//! ```
//!
//! — nor boxed as a node to be processed by hand:
//!
//! ```compile_fail,E0277
//! use tutti_graph::Node;
//! use tutti_plugin::PluginClient;
//! fn boxed(client: PluginClient) -> Box<dyn Node> {
//!     Box::new(client)
//! }
//! ```
//!
//! Inserting a bound client hands back its [`PluginControls`] (its
//! `IntoNode::Controls`) and a fork source, so the host never needs to reach
//! into the graph for the node again.
//!
//! # What the node reads
//!
//! - **Audio**, planar `f32`, through the pipelined IPC
//!   `Batcher` (`IpcProcessor`'s): whole chunks of one device callback (the
//!   graph's `MaxBlock` when the host does not say), so the plugin's latency
//!   is its own plus one callback. The `f64` wire conversion stays inside it.
//! - **The transport**, from each chunk's frame of the block's `Env`
//!   (`transport_source`), plus the meter installed on its controls.
//! - **MIDI, parameter ramps, chords and scales** on its event input, sent
//!   with the chunk their frames fall in. A clip installed on the MIDI port is
//!   still polled out of band (see the `shape` docs of the generic node).
//!
//! Subprocess lifetime is `ProcessGuard`, held behind `Arc` here and in
//! [`PluginHandle`], so the subprocess dies when the
//! LAST Arc drops. Main-thread editor / state / parameter access is on
//! [`PluginHandle`].

pub(crate) mod fork;

#[cfg(test)]
mod ipc_node_tests;
#[cfg(test)]
mod mock_server;
#[cfg(test)]
mod param_delivery_tests;
// The block-budget suite driven by a REAL plugin subprocess, as opposed to
// `tutti-plugin-ipc`'s `process_pipeline_tests` mock servers. A unit test rather than an
// integration one: it reads `PluginBridge`'s block counts and drives a graph built
// in the test. `clap` because it needs a `plugin-server` built with that loader.
#[cfg(all(test, feature = "clap"))]
mod fork_late_rate_tests;
#[cfg(all(test, feature = "clap"))]
mod real_stall_tests;
// `IpcController`'s two surfaces against the real plugin, which answers every
// call (`tutti-plugin-ipc`'s mock server answers a few).
#[cfg(all(test, feature = "clap"))]
mod served_control_tests;

use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_buffers::FloatWidth;
use tutti_graph::{IntoNode, Node, NodeParts, Shape};
use tutti_platform::Executor;
use tutti_plugin_db::Loadable;
use tutti_plugin_host::{
    InProcessClient, InvalidateSink, ParameterChangeSink, PluginControls, PluginHandle, PluginNode,
    Rebinder, RefreshSink, TransportSink, client_over,
};
use tutti_plugin_ipc::{
    Ipc, IpcConfig, IpcController, IpcEvent, LaunchSpec, Launched, LoadRequest, PluginBridge,
    Restore,
};
use tutti_plugin_types::{
    Features, IpcError, IpcResult, MainWorkHandle, MaxBlock, ParameterChanges, PluginDescriptor,
    PluginInvalidation, PluginRef, PluginWiring, Prepare, ResyncClass,
};
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

use self::fork::{ForkWatch, ServedFork};

/// The typestate of a [`PluginClient`] that is loaded but not yet a graph node.
///
/// [`PluginClient::new`] returns this state; [`PluginClient::bind`] leaves it.
#[derive(Debug)]
pub struct Unbound {
    /// The generic client over the out-of-process plugin, activated at load. Boxed: it
    /// is tens of KiB, and the client moves several times on its way into a
    /// graph (a debug build's frames hold a copy each).
    client: Box<InProcessClient<Ipc>>,
}

/// The typestate of a [`PluginClient`] that holds its node and can go into a
/// graph.
///
/// Reached through [`PluginClient::bind`]. Only `PluginClient<Bound>` is an
/// [`IntoNode`].
pub struct Bound {
    /// The generic node over the out-of-process hosting: what the graph runs. Boxed
    /// for the reason [`Unbound`]'s client is.
    node: Box<PluginNode<Ipc>>,
    /// The project's tempo map reader the node was bound to, kept so a fork
    /// clones it on the control side (its own registration on the same cell).
    tempo: tutti_timeline::TempoReader,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound").finish_non_exhaustive()
    }
}

/// An out-of-process plugin as a graph node: [`Unbound`] when loaded,
/// [`Bound`] once it can go into a graph.
///
/// [`PluginClient::new`] launches the plugin-server subprocess and loads the
/// plugin. [`bind`](PluginClient::bind) hands the project's tempo map to the
/// node and returns a `PluginClient<Bound>`, the only state that implements
/// [`IntoNode`]. Inserting it into a graph hands back
/// its [`PluginControls`], so the host never needs to reach into the graph for
/// the node again. Most hosts get a client through
/// [`Plugin::open`](crate::catalog::Plugin::open) instead of calling `new`.
///
/// The node processes planar `f32` audio in chunks of one device callback
/// through a pipelined IPC path, so its latency is the plugin's own plus one
/// callback ([`PluginControls::declared_latency`] reports both). MIDI,
/// parameter automation, harmony and note expression come in through the
/// node's inputs and [`PluginControls`]; the transport is read from each
/// block. Editor, state and main-thread parameter access are on
/// [`PluginHandle`]. The subprocess
/// exits when the node and every handle built from it have been dropped.
///
/// Not `Clone`: a node exists once, in the graph that owns it.
///
/// # Examples
///
/// ```no_run
/// use tutti_graph::Editor;
/// use tutti_plugin::PluginClient;
/// use tutti_plugin_host::PluginControls;
/// use tutti_graph::NodeKey;
/// use tutti_timeline::TempoReader;
/// fn insert(client: PluginClient, tempo: TempoReader, editor: &mut Editor) -> PluginControls {
///     editor.insert(NodeKey(1), "plugin", client.bind(tempo)).controls
/// }
/// ```
pub struct PluginClient<S = Unbound> {
    bridge: Arc<PluginBridge>,
    descriptor: PluginDescriptor,
    loaded: PluginWiring,
    format: FloatWidth,
    /// Total input ports (main + sidechain/aux) and output ports: the node's
    /// audio widths.
    inputs: usize,
    outputs: usize,
    /// The input slots, meter, latency, tail and sample rate — every cell
    /// shared with the node and the handles a host keeps, so a host holding
    /// [`controls`](Self::controls) reaches the running node without it.
    controls: PluginControls,
    /// Observers for plugin-originated unsolicited events. Shared with
    /// `PluginHandle` so callers can register callbacks via the handle
    /// and still see events driven by the bridge thread.
    param_sink: ParameterChangeSink,
    refresh_sink: RefreshSink,
    invalidate_sink: InvalidateSink,
    transport_sink: TransportSink,
    /// The out-of-process plugin's control side (its bridge and the server's lifetime
    /// guard), shared with `PluginClient::handle`.
    served: IpcController,
    /// Where this instance came from, so a fork can load another of it.
    origin: LaunchSpec,
    /// The plugin this is, with the id it reports.
    plugin: PluginRef,
    /// On a **fork** only: whether it has failed while rendering
    /// ([`fork_health`](PluginClient::fork_health)).
    fork_watch: Option<Arc<ForkWatch>>,
    /// [`Unbound`] or [`Bound`].
    state: S,
}

impl<S> std::fmt::Debug for PluginClient<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginClient")
            .field("name", &self.descriptor.name)
            .field("state", &std::any::type_name::<S>())
            .finish_non_exhaustive()
    }
}

/// Where the bridge's events go: the cells and sinks the node's controls and
/// subscribers read.
struct EventRoute {
    reports: tutti_plugin_host::PluginReports,
    params: ParameterChangeSink,
    refresh: RefreshSink,
    invalidate: InvalidateSink,
    transport: TransportSink,
    chunks: Arc<tutti_plugin_types::ChunkCounters>,
}

impl EventRoute {
    /// Puts one event of the bridge in its cell or sink.
    fn hear(&self, ev: IpcEvent) {
        let ev = match plain_invalidation(ev) {
            Ok(invalidation) => return self.invalidate.fire(invalidation),
            Err(ev) => ev,
        };
        match ev {
            IpcEvent::LatencyChanged { samples } => {
                let reported = PluginInvalidation::Latency { samples };
                self.reports.take_report(&reported);
                self.invalidate.fire(reported);
            }
            IpcEvent::TailChanged { tail } => {
                let reported = PluginInvalidation::Tail { tail };
                self.reports.take_report(&reported);
                self.invalidate.fire(reported);
            }
            IpcEvent::ParameterChanged { edit } => {
                self.params.fire(edit);
            }
            IpcEvent::Resync(kind) => match kind.classify() {
                ResyncClass::Refresh(r) => self.refresh.fire(r),
                ResyncClass::Invalidate(i) => self.invalidate.fire(i),
            },
            IpcEvent::ServerLosses(losses) => self.chunks.note_server_losses(losses),
            // The wake it raises is bound through `Control::bind_wake` and
            // raised by `Ipc` before this listener runs.
            IpcEvent::ProcessRequested => {}
            // Fired above.
            IpcEvent::StateNotRestored { .. }
            | IpcEvent::RenderModeNotApplied { .. }
            | IpcEvent::WouldNotStop { .. }
            | IpcEvent::RateRefusal(_)
            | IpcEvent::Crashed { .. } => {}
            // The layout is read from the link when the `Io` resync that follows it lands.
            IpcEvent::IoLayout(_) => {}
            IpcEvent::Transport { request } => self.transport.fire(request),
        }
    }
}

/// The invalidation an event of the bridge is, when it is that and nothing
/// else: no cell is updated alongside it. `Crashed` is structural and
/// terminal, and the crash flag it mirrors was already set by the bridge
/// thread before it fired (see `thread::crash`).
fn plain_invalidation(ev: IpcEvent) -> Result<PluginInvalidation, IpcEvent> {
    match ev {
        IpcEvent::StateNotRestored { reason } => {
            Ok(PluginInvalidation::StateNotRestored { reason })
        }
        IpcEvent::RenderModeNotApplied { mode } => {
            Ok(PluginInvalidation::RenderModeNotApplied { mode })
        }
        IpcEvent::WouldNotStop { reason } => Ok(PluginInvalidation::Stuck { reason }),
        IpcEvent::RateRefusal(Some(refusal)) => Ok(PluginInvalidation::RateRefused(refusal)),
        IpcEvent::RateRefusal(None) => Ok(PluginInvalidation::RateRestored),
        IpcEvent::Crashed { cause } => Ok(PluginInvalidation::Crashed { cause }),
        other => Err(other),
    }
}

impl PluginClient<Unbound> {
    /// Spawns the plugin-server, loads `plugin` and returns the loaded
    /// plugin, [`Unbound`].
    ///
    /// `plugin` names the plugin in its file ([`PluginRef::id`]) and the
    /// format that opens it: a scanned plugin's id picks it in a bundle that
    /// exports several, and [`PluginRef::file`] is the explicit "the file's
    /// first". The server opens the file as that format, whatever its extension.
    ///
    /// Blocks while the subprocess launches and completes its handshake. The
    /// `work` is the collector of the plugin's owner, which the editor instance
    /// hands its close to; `executor` runs the server's exit watch. The
    /// subprocess exits when the graph has dropped the node and every
    /// [`PluginHandle`] built from this
    /// client has been dropped.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::NotOutOfProcess`] for a VST2 file (it runs in the
    /// host's process: see `Plugin::open`), and a [`IpcError`] if the
    /// plugin-server cannot be found or spawned, the handshake fails or times out, the
    /// plugin fails to load (a ref's id that names no plugin of the file is the
    /// format's own load failure), the IPC bridge cannot be set up, or the
    /// server does not list the plugin's parameters
    /// ([`IpcError::ParametersUnlisted`]).
    pub fn new(
        config: IpcConfig,
        executor: &Executor,
        work: &MainWorkHandle,
        plugin: &Loadable,
        sample_rate: SampleRate,
    ) -> IpcResult<Self> {
        Self::open(config, executor, work, plugin, sample_rate, None).map(|(client, _)| client)
    }

    /// [`new`](Self::new), restoring `state` into the plugin **before it is
    /// activated**, the order every format allows and a plugin that takes its
    /// mode from the state at activation needs. The second value is what became
    /// of it: `None` when none was given, else the plugin's refusal as text (the
    /// plugin is then loaded at its defaults, which beats no plugin). The
    /// choice of plugin is kept for a fork, which loads the same plugin again.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn open(
        config: IpcConfig,
        executor: &Executor,
        work: &MainWorkHandle,
        plugin: &Loadable,
        sample_rate: SampleRate,
        state: Option<&[u8]>,
    ) -> IpcResult<(
        Self,
        Option<std::result::Result<(), tutti_plugin_types::StateError>>,
    )> {
        plugin.check()?;
        let format = crate::host::placement::format_of(plugin)?;
        let plugin = plugin.plugin();
        let plugin_path = plugin.path().to_path_buf();
        // A VST2 plugin is not served (its editor and audio are one object):
        // refused here, typed, before a server is started for it.
        if format == tutti_plugin_types::PluginFormat::Vst2 {
            return Err(IpcError::NotOutOfProcess {
                path: plugin_path,
                format: tutti_plugin_types::PluginFormat::Vst2,
            });
        }
        let origin = plugin.clone();
        let Launched { ipc, restored } = Ipc::launch(
            config,
            &LoadRequest {
                origin: &origin,
                format,
                rate: sample_rate,
                restore: state.map(|state| Restore {
                    state,
                    state_use: tutti_plugin_types::StateUse::Project,
                }),
            },
            Some(tutti_plugin_ipc::EditorLoader::new(
                crate::format::gui::load_gui_instance,
                work.clone(),
            )),
            executor,
        )?;
        Self::over(ipc).map(|client| (client, restored))
    }

    /// The client over an out-of-process plugin that is running: the controls and the
    /// sinks the bridge's events go to, and the generic node, activated at the
    /// slab's chunk ceiling (a graph's `prepare` then narrows it). Needs no
    /// main thread.
    ///
    /// # Errors
    ///
    /// [`IpcError::ParametersUnlisted`] when the server does not list the
    /// plugin's parameters, and the other failures of setting up the node.
    fn over(ipc: Ipc) -> IpcResult<Self> {
        let bridge = Arc::clone(ipc.bridge());
        let descriptor = ipc.descriptor().clone();
        let loaded = ipc.loaded().clone();
        let rate = ipc.rate();
        // The plugin's file, when the ipc knows it: the path of a failure here.
        // The pipeline's chunk ceiling, matching the slab (`Ipc` clamps it to
        // `MAX_CHUNK` for the same reason): the raw `max_buffer_size` would
        // stage 8192 samples per channel to ship 64.
        let prepare = MaxBlock::new(Samples::new(ipc.ceiling()))
            .map(|block| Prepare::new(rate, block))
            .ok_or(IpcError::BlockUnsupported {
                frames: ipc.ceiling(),
            })?;

        let (controls, reports) = PluginControls::with_chunks(
            loaded.latency_samples,
            loaded.tail,
            rate,
            Arc::clone(ipc.chunk_counters()),
        );
        controls
            .delivery_counters()
            .attach_bridge_midi(bridge.audio().dropped_midi_cell());
        let param_sink = ParameterChangeSink::new();
        let refresh_sink = RefreshSink::new();
        let invalidate_sink = InvalidateSink::new();
        let transport_sink = TransportSink::new();

        // Route the bridge's events into the shared cells and sinks, through
        // `Ipc::forward_events`. The listener runs on the bridge thread and
        // must be cheap. A latency change reaches the controls (`take_report`), so
        // `PluginControls::declared_latency` reads the new figure, then fires
        // the *invalidate* sink (latency is a structural invalidation — it
        // re-plans PDC, which a host does with `Editor::set_latency`). Resync
        // signals split by consequence into the refresh (cosmetic) vs
        // invalidate (structural) sinks via `ResyncKind::classify`.
        let route = EventRoute {
            reports,
            params: param_sink.clone(),
            refresh: refresh_sink.clone(),
            invalidate: invalidate_sink.clone(),
            transport: transport_sink.clone(),
            chunks: Arc::clone(ipc.chunk_counters()),
        };
        ipc.forward_events(Some(Arc::new(move |ev| route.hear(ev))));

        // The parameter list, once, while loading blocks anyway: the node's
        // per-parameter delivery is sized from it. A `prepare` may not make
        // this round trip; a load already has.
        let params: Vec<ParamId> = bridge
            .audio()
            .parameters()
            .ok_or(IpcError::ParametersUnlisted)?
            .into_iter()
            .map(|p| p.id())
            .collect();

        let origin = ipc.origin().cloned().ok_or(IpcError::NoOrigin)?;
        // The ref carries the id the server loaded, so a state it saves and a
        // hand-inserted node name it as a scan would.
        let plugin = origin.origin.clone();
        let format = ipc.format();
        let served = ipc.controller();
        // Report the FULL input width (main + sidechain/aux input buses) so a
        // graph edge into port 1 lands on a real sidechain port.
        let (inputs, outputs) = (loaded.total_inputs(), loaded.total_outputs());
        let (processor, controller) = ipc
            .activate_anywhere(prepare)
            .map_err(|refused| IpcError::from(refused.reason))?;
        let client = client_over::<Ipc, f32>(
            processor,
            controller,
            prepare,
            descriptor.clone(),
            &loaded,
            params.iter().copied(),
            ParameterChanges::SPARE_QUEUES,
            Some(controls.clone()),
        )
        .map_err(tutti_plugin_host::ClientOverError::into_bridge)?;
        Ok(Self {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            transport_sink,
            served,
            origin,
            plugin,
            fork_watch: None,
            state: Unbound {
                client: Box::new(client),
            },
        })
    }

    /// Hands the node the reader of the project's tempo map it tells the
    /// plugin the tempo from (and CLAP its slope), lent once per block, and
    /// returns the [`PluginClient<Bound>`] that can go into a graph.
    ///
    /// Call on the control thread. Insert the result with `Editor::insert`,
    /// which prepares it and hands back its [`PluginControls`].
    ///
    /// Install per-block sources before or after — they live on shared cells
    /// ([`controls`](Self::controls)), so an install through a handle reaches
    /// the running node.
    ///
    /// `Session` hands every plugin it inserts a reader of its own
    /// (`TransportSettings::tempo_reader`); a host with no session builds one
    /// over a fixed map ([`TempoReader::fixed`](tutti_timeline::TempoReader::fixed)).
    /// A fork of the node clones it.
    #[must_use]
    pub fn bind(self, tempo: tutti_timeline::TempoReader) -> PluginClient<Bound> {
        let Self {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            transport_sink,
            served,
            origin,
            plugin,
            fork_watch,
            state: Unbound { client },
        } = self;

        PluginClient {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            transport_sink,
            served,
            origin,
            plugin,
            fork_watch,
            state: Bound {
                node: Box::new((*client).bind(tempo.clone())),
                tempo,
            },
        }
    }
}

impl PluginClient<Bound> {
    /// The builder of the node that replaces this one when the plugin changes
    /// its audio ports ([`Rebinder::prepare`]). Made on the call from what the
    /// bound node holds, so take it before the client goes into a graph
    /// (`into_parts` consumes the client) and keep it beside the node's
    /// controls.
    ///
    /// The rebinder keeps the plugin instance alive until it is dropped.
    #[must_use]
    pub fn rebinder(&self) -> Rebinder<Ipc> {
        self.state.node.rebinder()
    }
}

impl<S> PluginClient<S> {
    /// The plugin this client is: its file, its format and the id it reports.
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The shape the plugin's node declares: its channels as audio ports, one
    /// MIDI event input, one MIDI event output if it has MIDI-out, and its
    /// latency and tail now (the node's `shape`). Read before binding, so a
    /// host can tell a replacement's ports from the running node's.
    pub fn declared_shape(&self) -> Shape {
        let midi_out = u16::from(self.loaded.features.contains(Features::MIDI_OUT));
        let width = |n: usize| ChannelLayout::from_count(u16::try_from(n).unwrap_or(u16::MAX));
        // Ports only: the node states the plugin's buses when it is bound.
        Shape::EMPTY
            .ports(width(self.inputs), width(self.outputs))
            .events(1, midi_out)
            .param_events()
            .latency(self.controls.declared_latency())
            .tail(self.controls.tail())
    }

    /// The presets the plugin lists, asked of its server: a round trip that
    /// takes no main thread, bounded by the parameter wait. `None` when the
    /// server is gone or did not answer.
    #[must_use]
    pub fn presets(&self) -> Option<Vec<tutti_plugin_types::PluginPreset>> {
        self.bridge.audio().presets()
    }

    /// The state and preset requests of this plugin, as [`PluginAnswers`]: a
    /// cloneable, `Send` handle that outlives the client's move into a graph.
    pub fn answers(&self) -> PluginAnswers {
        PluginAnswers {
            bridge: Arc::clone(&self.bridge),
        }
    }

    /// The main-thread control handle for this plugin: editor, parameters,
    /// state.
    ///
    /// Call this before moving the client into a graph. The handle shares the
    /// subprocess's lifetime with the client, and the subprocess backend
    /// honors every capability, including the editor.
    pub fn handle(&self) -> PluginHandle {
        let served = Arc::new(self.served.clone());
        let pid = served.server_pid();
        let handle = PluginHandle::from_backend(
            Arc::clone(&served),
            Some(served),
            self.descriptor().clone(),
            self.loaded().clone(),
            self.param_sink().clone(),
        )
        .refresh_sink(self.refresh_sink().clone())
        .invalidate_sink(self.invalidate_sink().clone())
        .transport_sink(self.transport_sink().clone());
        match pid {
            Some(pid) => handle.with_server_pid(pid),
            None => handle,
        }
    }

    pub(crate) fn param_sink(&self) -> &ParameterChangeSink {
        &self.param_sink
    }

    pub(crate) fn refresh_sink(&self) -> &RefreshSink {
        &self.refresh_sink
    }

    pub(crate) fn invalidate_sink(&self) -> &InvalidateSink {
        &self.invalidate_sink
    }

    pub(crate) fn transport_sink(&self) -> &TransportSink {
        &self.transport_sink
    }

    /// Returns a handle on this node's host-side controls that stays valid
    /// after the node moves into a graph.
    ///
    /// The controls cover the input slots, meter, latency, tail and sample
    /// rate. Inserting a bound client hands the same handle back.
    ///
    /// Every cell in it is shared with this node, so a host takes this and
    /// drives the running plugin through it from then on, instead of reaching
    /// back into the graph for the node. See [`PluginControls`].
    pub fn controls(&self) -> PluginControls {
        self.controls.clone()
    }

    /// Returns the plugin's catalog identity (id, name, vendor, version, class,
    /// editor).
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Returns what the plugin reported at load: per-bus channel widths,
    /// latency and `f64` support.
    pub fn loaded(&self) -> &PluginWiring {
        &self.loaded
    }

    /// Returns the number of audio input ports: main plus sidechain/aux buses.
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// Returns the number of audio output ports.
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// Returns `true` once the connection to the plugin-server has failed (for
    /// example, the subprocess crashed); the node then outputs silence.
    pub fn is_crashed(&self) -> bool {
        self.bridge.audio().is_crashed()
    }

    /// The bridge, for the tests that script or read the server.
    #[cfg(all(test, feature = "clap"))]
    pub(crate) fn bridge(&self) -> Arc<PluginBridge> {
        Arc::clone(&self.bridge)
    }
}

/// A bound plugin as a graph node: the node itself (the generic
/// `PluginNode<Ipc>`), the [`PluginControls`] a host drives it through from
/// then on, and a [`ForkSource`](tutti_graph::ForkSource) that forks it by state
/// transfer (a fresh instance in its own subprocess, loaded with the live
/// instance's saved state).
///
/// Only [`Bound`] implements this: an unbound plugin is not a node.
impl IntoNode for PluginClient<Bound> {
    type Controls = PluginControls;

    fn into_node(self) -> (Box<dyn Node>, PluginControls) {
        let controls = self.controls();
        (self.state.node, controls)
    }

    fn into_parts(self) -> NodeParts<PluginControls> {
        let fork = ServedFork::of(&self);
        let (node, controls) = self.into_node();
        NodeParts::new(node, controls, Some(Box::new(fork)), None)
    }
}

/// An out-of-process plugin's state and preset requests, each answered into a oneshot
/// receiver: a request is queued with the plugin's bridge and the call returns,
/// with no wait and no main thread. **The bridge's IPC reader thread fills the
/// sender** with the server's reply, and **its `plugin-control` lane thread
/// fires the deadline**: a server that never answers is cut off there, and the
/// lane sends [`StateError::Stalled`] (or [`PresetError::Stalled`]) into the
/// sender. A sender dropped unanswered (the bridge ended or the server died)
/// cancels the receiver.
///
/// Made by [`PluginClient::answers`]. Cheap to clone; it keeps the bridge, not
/// the plugin's server, alive.
///
/// [`StateError::Stalled`]: tutti_plugin_types::StateError::Stalled
/// [`PresetError::Stalled`]: tutti_plugin_types::PresetError::Stalled
#[derive(Clone)]
pub struct PluginAnswers {
    bridge: Arc<PluginBridge>,
}

impl std::fmt::Debug for PluginAnswers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginAnswers").finish_non_exhaustive()
    }
}

pub use tutti_plugin_types::Answer;

impl PluginAnswers {
    /// Queues the save of the plugin's state for `state_use`.
    pub fn save_state(
        &self,
        state_use: tutti_plugin_types::StateUse,
    ) -> Answer<Result<Vec<u8>, tutti_plugin_types::StateError>> {
        self.bridge.audio().save_state_answer(state_use)
    }

    /// Queues the restore of `data`, a project's state. The server may apply it
    /// after its answer is no longer awaited.
    pub fn load_state(&self, data: &[u8]) -> Answer<Result<(), tutti_plugin_types::StateError>> {
        self.bridge
            .audio()
            .load_state_answer(data, tutti_plugin_types::StateUse::Project)
    }

    /// Queues the load of the preset `id`. The server may load it after its
    /// answer is no longer awaited.
    pub fn load_preset(
        &self,
        id: &tutti_plugin_types::PresetId,
    ) -> Answer<Result<(), tutti_plugin_types::PresetError>> {
        self.bridge.audio().load_preset_answer(id.clone())
    }

    /// Loads `data` into the plugin's in-process editor instance, the one an
    /// Audio Unit's editor runs in, so its display follows a state the server
    /// took ([`load_state`](Self::load_state)). On the main thread.
    pub fn mirror_state(&self, main: tutti_plugin_types::MainThreadMarker, data: &[u8]) {
        self.bridge.mirror_state(main, data);
    }
}

#[cfg(test)]
mod event_route_tests {
    use super::*;

    /// **The bridge's notices reach the host as the invalidations the session
    /// maps**: a refused render mode keeps its mode, and a restart the plugin
    /// would not make is `Stuck` with its reason.
    #[test]
    fn a_refused_render_mode_and_a_restart_the_plugin_would_not_make_map_to_their_invalidations() {
        let mode = plain_invalidation(IpcEvent::RenderModeNotApplied {
            mode: tutti_plugin_types::RenderMode::Offline,
        });
        assert!(
            matches!(
                mode,
                Ok(PluginInvalidation::RenderModeNotApplied {
                    mode: tutti_plugin_types::RenderMode::Offline
                })
            ),
            "{mode:?}"
        );
        let stuck = plain_invalidation(IpcEvent::WouldNotStop {
            reason: "inside a block".into(),
        });
        assert!(
            matches!(&stuck, Ok(PluginInvalidation::Stuck { reason }) if reason == "inside a block"),
            "{stuck:?}"
        );
        assert!(
            plain_invalidation(IpcEvent::ProcessRequested).is_err(),
            "an event that is not an invalidation is handed back"
        );
    }

    /// **The server's loss counts the bridge heard are what the controls read**,
    /// and a later report replaces the earlier (the figures are cumulative).
    #[test]
    fn the_servers_losses_reach_the_chunk_counters() {
        let chunks = Arc::new(tutti_plugin_types::ChunkCounters::default());
        let (controls, reports) = PluginControls::with_chunks(
            Samples::new(0),
            tutti_plugin_types::PluginTail::default(),
            SampleRate::SR_48K,
            Arc::clone(&chunks),
        );
        let route = EventRoute {
            reports,
            params: ParameterChangeSink::new(),
            refresh: RefreshSink::new(),
            invalidate: InvalidateSink::new(),
            transport: TransportSink::new(),
            chunks: Arc::clone(&chunks),
        };
        route.hear(IpcEvent::ServerLosses(tutti_plugin_types::ServerLosses {
            commands_refused: 2,
            edits_refused: 1,
            refused_off_thread_calls: tutti_units::Calls::new(4),
        }));
        assert_eq!(
            controls.server_losses(),
            tutti_plugin_types::ServerLosses {
                commands_refused: 2,
                edits_refused: 1,
                refused_off_thread_calls: tutti_units::Calls::new(4),
            }
        );
        route.hear(IpcEvent::ServerLosses(tutti_plugin_types::ServerLosses {
            commands_refused: 5,
            edits_refused: 1,
            refused_off_thread_calls: tutti_units::Calls::new(4),
        }));
        assert_eq!(controls.server_losses().commands_refused, 5);
    }
}
