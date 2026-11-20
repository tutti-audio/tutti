//! [`Plugin`]: one loaded plugin, whatever format it is and wherever it runs.
//!
//! Loading returns one owned type rather than a `(node, PluginHandle)` pair
//! because the hostings are different concrete node types
//! (`PluginClient<Bound>` and `InProcessClient`), and because a plugin is
//! more than its node: it also takes per-block MIDI, chord/scale context and
//! note expression, reached here through capability accessors.

use crate::host::placement::placement;
use tutti_plugin_db::{Hosting, Loadable};
use tutti_plugin_types::PluginRef;
use tutti_plugin_types::{MainThreadMarker, MainWorkHandle};
use tutti_rt::LockExt;

use crate::host::node::PluginClient;
use tutti_plugin_host::{
    AnyInProcessClient, Commit, InProcessNode, InProcessRebinder, PluginHandle,
};
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::IpcResult;
use tutti_plugin_types::PluginFormat;
use tutti_plugin_types::{Features, LoadedPlugin, PluginDescriptor, PluginRole};
use tutti_units::SampleRate;

/// The audio node, wherever it runs.
///
/// Private: which arm a plugin landed in is exactly the detail this type exists
/// to stop leaking ([`Plugin::hosting`] says where it runs, and nothing else
/// depends on it).
///
/// Every arm is boxed. An enum is as large as its largest variant, and these
/// differ by ~9 KiB (a served client is ~15 KiB, an in-process one ~6 KiB), so
/// an unboxed enum would make every `Plugin` (and every `IpcResult`
/// carrying one) pay the larger figure.
enum Placed {
    Served(Box<PluginClient>),
    /// Built only where a format feature is on.
    #[allow(dead_code)]
    InProcess(AnyInProcessClient),
}

/// A loaded plugin: its audio node, its control surface, and the per-block
/// inputs it can accept.
///
/// Created by [`Plugin::open`] or `PluginLoad::run`.
/// Where a plugin runs is its load's [`Hosting`]: by default VST3, CLAP and AU
/// plugins run in a `plugin-server` subprocess and VST2 (with the `vst2`
/// feature) in the host process; [`Loadable::hosting`] asks for the other
/// where the format allows it. Which one a plugin landed in does not change
/// this API ([`hosting`](Self::hosting) says).
///
/// A plugin becomes a graph node by binding it to the project's tempo map
/// ([`bind`](Self::bind)): `editor.insert(key, kind, plugin.bind(tempo))`. Keep a clone of [`handle`](Self::handle)
/// first if you need the control surface afterwards; the plugin stays alive
/// while either the node or a handle does.
///
/// # Examples
///
/// ```no_run
/// use tutti_units::SampleRate;
/// use tutti_plugin::catalog::{Loadable, Plugin, PluginRef};
///
/// let file = PluginRef::file("/usr/lib/vst3/MyPlugin.vst3");
/// let (_collector, work) = tutti_plugin::MainWork::new();
/// let plugin = Plugin::open(None, &work, &Loadable::uncatalogued(file), SampleRate::SR_48K)?;
/// println!("{} ({:?})", plugin.descriptor().name, plugin.role());
/// if plugin.takes_midi() {
///     // wire a MIDI source to the node's event input
/// }
/// # Ok::<(), tutti_plugin_types::IpcError>(())
/// ```
pub struct Plugin {
    placed: Placed,
    handle: PluginHandle,
    /// The plugin that was opened, with the id the loaded plugin reports.
    plugin: PluginRef,
}

/// Reports identity and where the plugin runs, not the node's guts: the
/// hostings wrap live subprocess and FFI state that has no useful `Debug`.
impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.descriptor().name)
            .field("format", &self.descriptor().class.format_name())
            .field("hosting", &self.hosting())
            .finish_non_exhaustive()
    }
}

impl Plugin {
    /// Opens a plugin with the default bridge settings.
    ///
    /// ```no_run
    /// # fn ex() -> Result<(), Box<dyn std::error::Error>> {
    /// use tutti_plugin::catalog::{Catalog, CatalogConfig, Plugin, PluginId};
    /// use tutti_plugin_db::PluginDb;
    /// # let dir = std::path::PathBuf::from("/my/app/state");
    /// let catalog = Catalog::new(PluginDb::new(), CatalogConfig::new(dir), |_| Ok(()));
    /// let foo = catalog
    ///     .resolve(&"vst3.00112233-44556677-8899AABB-CCDDEEFF".parse::<PluginId>()?)
    ///     .bind()?;
    /// let (_collector, work) = tutti_plugin::MainWork::new();
    /// let plugin = Plugin::open(None, &work, &foo, tutti_units::SampleRate::SR_48K)?;
    /// # Ok(()) }
    /// ```
    ///
    /// `sample_rate` is the rate the plugin is opened at.
    ///
    /// `plugin` names the plugin: its file, the format that opens it and which
    /// plugin of the file: a bundle that exports several opens the one its id
    /// names, not the first. The [`Hosting`] is the load's. `Auto` runs VST2 (with
    /// the `vst2` feature) in the host process, since its `AEffect` fuses
    /// editor and audio processor into one instance, and every other format in
    /// a `plugin-server` subprocess with an IPC bridge. This call blocks while
    /// the subprocess starts and the plugin loads, so call it off the audio
    /// thread.
    ///
    /// `main` is the proof that the caller is on the main thread. Only an
    /// in-process load needs it, and refuses to load without it; a plugin
    /// served from a subprocess loads from any thread, so a caller that is not
    /// on the main thread passes `None`.
    ///
    /// The load asks `plugin`'s blacklist where it opens the plugin, so a plugin
    /// blacklisted since it was resolved is refused.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::Blacklisted`](tutti_plugin_types::IpcError::Blacklisted)
    /// for a blacklisted plugin,
    /// [`IpcError::ServerNotFound`](tutti_plugin_types::IpcError::ServerNotFound)
    /// when the `plugin-server` binary cannot be located,
    /// [`IpcError::LoadFailed`](tutti_plugin_types::IpcError::LoadFailed)
    /// when the plugin does not load, [`IpcError::NotOutOfProcess`] for a
    /// VST2 plugin whose load asks for [`Hosting::OutOfProcess`],
    /// [`IpcError::NotInProcess`] for a format this build has no in-process
    /// host for, and a connection, protocol or timeout variant when the
    /// subprocess cannot be reached.
    ///
    /// [`IpcError::NotOutOfProcess`]: tutti_plugin_types::IpcError::NotOutOfProcess
    /// [`IpcError::NotInProcess`]: tutti_plugin_types::IpcError::NotInProcess
    pub fn open(
        main: Option<MainThreadMarker>,
        work: &MainWorkHandle,
        plugin: &Loadable,
        sample_rate: SampleRate,
    ) -> IpcResult<Self> {
        Self::open_with(main, work, &AudioConfig::default(), plugin, sample_rate)
    }

    /// Opens a plugin file with explicit bridge settings.
    ///
    /// See [`open`](Self::open).
    ///
    /// # Errors
    ///
    /// The same as [`open`](Self::open).
    pub(crate) fn open_with(
        main: Option<MainThreadMarker>,
        work: &MainWorkHandle,
        audio: &AudioConfig,
        plugin: &Loadable,
        sample_rate: SampleRate,
    ) -> IpcResult<Self> {
        Self::open_with_state(main, work, audio, plugin, sample_rate, None)
            .map(|(plugin, _)| plugin)
    }

    /// `open_with`, restoring `state` into the plugin
    /// **before it is activated**: every format takes a state on a plugin that is
    /// open and not yet active, and a plugin may take its mode from the state at
    /// activation. The second value is what became of it: `None` when none was
    /// given, else the plugin's refusal as text (the plugin is then loaded at its
    /// defaults, which beats no plugin).
    ///
    /// # Errors
    ///
    /// The same as [`open`](Self::open).
    pub fn open_with_state(
        main: Option<MainThreadMarker>,
        work: &MainWorkHandle,
        audio: &AudioConfig,
        plugin: &Loadable,
        sample_rate: SampleRate,
        state: Option<&[u8]>,
    ) -> IpcResult<(Self, Option<std::result::Result<(), String>>)> {
        if placement(plugin)? == Hosting::InProcess {
            // A served load asks the blacklist in `PluginClient::open`; a
            // session load has asked it once already, before it defers.
            plugin.check()?;
            // An in-process plugin loads in this process, on the main thread:
            // the caller says it is there by passing the token.
            let Some(main) = main else {
                return Err(tutti_plugin_types::IpcError::LoadFailed {
                    path: Some(plugin.plugin().path().to_path_buf()),
                    stage: tutti_plugin_types::LoadStage::Opening,
                    reason: "a plugin hosted in the host process loads on the main thread \
                             and needs it"
                        .into(),
                });
            };
            return Self::open_in_process(
                main,
                work,
                plugin.plugin(),
                crate::host::placement::format_of(plugin)?,
                sample_rate,
                state,
            );
        }

        let ((client, restored), handle) =
            PluginClient::open(audio.to_bridge_config()?, work, plugin, sample_rate, state).map(
                |(client, restored)| {
                    let handle = client.handle();
                    ((client, restored), handle)
                },
            )?;
        Ok((Self::from_subprocess(client, handle), restored))
    }

    /// The in-process load, by format.
    #[allow(unused_variables)]
    fn open_in_process(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
        format: PluginFormat,
        sample_rate: SampleRate,
        state: Option<&[u8]>,
    ) -> IpcResult<(Self, Option<std::result::Result<(), String>>)> {
        match format {
            #[cfg(feature = "vst2")]
            PluginFormat::Vst2 => Self::load_in_process::<tutti_vst2_host::Vst2Loaded>(
                main,
                work,
                plugin,
                sample_rate,
                state,
            ),
            #[cfg(feature = "clap")]
            PluginFormat::Clap => Self::load_in_process::<tutti_clap_host::ClapLoaded>(
                main,
                work,
                plugin,
                sample_rate,
                state,
            ),
            #[cfg(feature = "vst3")]
            PluginFormat::Vst3 => Self::load_in_process::<tutti_vst3_host::Vst3Loaded>(
                main,
                work,
                plugin,
                sample_rate,
                state,
            ),
            #[cfg(all(feature = "au", target_os = "macos"))]
            PluginFormat::AudioUnit => Self::load_in_process::<tutti_au_host::AuLoaded>(
                main,
                work,
                plugin,
                sample_rate,
                state,
            ),
            // `Loadable::placement` refused every format with no host built in; this
            // arm is the same refusal, for a caller that skipped it.
            _ => Err(tutti_plugin_types::IpcError::NotInProcess {
                path: plugin.path().to_path_buf(),
                format: "plugin",
            }),
        }
    }

    /// Loads the format `L` in this process, restoring `state` before it is
    /// activated.
    #[cfg(any(
        feature = "vst2",
        feature = "clap",
        feature = "vst3",
        all(feature = "au", target_os = "macos")
    ))]
    fn load_in_process<L>(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
        sample_rate: SampleRate,
        state: Option<&[u8]>,
    ) -> IpcResult<(Self, Option<std::result::Result<(), String>>)>
    where
        L: tutti_plugin_types::InProcess + 'static,
        L::Processor: 'static,
    {
        let mut restore = state.map(|data| {
            move |loaded: &mut L| {
                tutti_plugin_types::Control::load_state(
                    loaded,
                    main,
                    data,
                    tutti_plugin_types::StateUse::Project,
                )
                .map_err(tutti_plugin_types::StateError::from)
            }
        });
        let (client, handle, restored) = tutti_plugin_host::load_client_restoring(
            main,
            work,
            plugin,
            sample_rate,
            restore.as_mut().map(|r| r as _),
        )
        .map_err(|e| e.into_bridge(plugin.path()))?;
        // The ref carries the id the load opened, so a state the plugin saves
        // names it as a scan would.
        let plugin = client.origin().cloned().unwrap_or_else(|| plugin.clone());
        Ok((
            Self {
                placed: Placed::InProcess(AnyInProcessClient::new(client)),
                handle,
                plugin,
            },
            // The served path's restore result is the server's text, so this
            // one is text too.
            restored.map(|restored| restored.map_err(|refused| refused.to_string())),
        ))
    }

    /// Wrap a subprocess client.
    fn from_subprocess(client: PluginClient, handle: PluginHandle) -> Self {
        Self {
            plugin: client.plugin().clone(),
            placed: Placed::Served(Box::new(client)),
            handle,
        }
    }

    /// Where the plugin runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`], never `Auto`.
    pub fn hosting(&self) -> Hosting {
        match &self.placed {
            Placed::Served(_) => Hosting::OutOfProcess,
            Placed::InProcess(_) => Hosting::InProcess,
        }
    }

    // ---- Identity ---------------------------------------------------------

    /// The plugin that was opened: the file, the format, and the id the loaded
    /// plugin reports (filled in when the open was by file alone).
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// Returns the main-thread control surface: editor, parameters, state.
    ///
    /// Cheap to clone, and shares the plugin's lifetime — the plugin dies when
    /// the last handle *or* `Plugin` drops.
    pub fn handle(&self) -> &PluginHandle {
        &self.handle
    }

    /// Returns the catalog identity: name, vendor, native classification.
    pub fn descriptor(&self) -> &PluginDescriptor {
        self.handle.descriptor()
    }

    /// Returns the load-time wiring: bus widths, latency, tail, capabilities.
    pub fn loaded(&self) -> &LoadedPlugin {
        self.handle.loaded()
    }

    /// Returns what this plugin is (instrument, effect, …), normalized across
    /// formats.
    pub fn role(&self) -> PluginRole {
        self.descriptor().class.role()
    }

    // ---- Per-block inputs -------------------------------------------------

    /// Whether the plugin accepts this input, i.e. did not answer `false`.
    ///
    /// Shares `is_declined` with the [`PluginClient`] accessors, so the two
    /// agree about what an unprobed capability means.
    fn accepts(&self, f: Features) -> bool {
        !crate::host::node::is_declined(self.loaded(), f)
    }

    /// Returns whether the plugin takes MIDI, that is, it did not decline MIDI
    /// input.
    ///
    /// When `true`, wire MIDI to its node's event input.
    pub fn takes_midi(&self) -> bool {
        self.accepts(Features::MIDI_IN)
    }

    /// Returns whether the plugin sends MIDI, that is, it did not decline MIDI
    /// output.
    ///
    /// When `true`, its node's event output carries it.
    pub fn sends_midi(&self) -> bool {
        self.accepts(Features::MIDI_OUT)
    }

    /// Tells the plugin whether it is being rendered under realtime pressure.
    ///
    /// Set this **before** pulling blocks for an offline bounce: a plugin may
    /// spend more per block when it knows there is no deadline, and three of
    /// the four formats can only take the change while the plugin is
    /// deactivated. Leaving it unset renders the live-quality result into a
    /// file the user asked to be exact.
    ///
    /// `false` if the plugin declared it does not honour a render-mode change
    /// — a CLAP plugin that does not implement `clap.render`, or an AU without
    /// `kAudioUnitProperty_OfflineRender`. That is a refusal, not a failure:
    /// such a plugin renders identically either way, which is exactly what
    /// declining the extension means.
    #[must_use = "a false return means the plugin declined the render mode and nothing was applied"]
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        if !self.accepts(Features::RENDER_MODE) {
            return false;
        }
        match &self.placed {
            Placed::Served(c) => c.set_render_mode(mode),
            Placed::InProcess(c) => c.set_render_mode(mode),
        }
    }

    /// Gives the plugin the project meter, for the time signature and bar of
    /// the transport it is told. The node's meter follows the project meter
    /// the host publishes edits to, so an edit reaches the running plugin
    /// with nothing re-installed.
    ///
    /// The transport itself needs no install: every plugin node reads it from
    /// each block's graph context once it is in a graph (the transport its
    /// input was rendered at).
    ///
    /// Returns `false` if the plugin declared it does not want a transport
    /// snapshot.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_meter(&self, meter: tutti_transport::ProjectMeter) -> bool {
        if !self.accepts(Features::TRANSPORT) {
            return false;
        }
        self.meter().set_meter(meter);
        true
    }

    /// Drops the meter; the plugin is told 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.meter().clear_meter();
    }

    /// The node's meter cell, as a handle of its own, for either hosting:
    /// what a host keeps to install the meter after the plugin has moved
    /// into a graph (the same cell [`set_meter`](Self::set_meter) writes).
    pub fn meter(&self) -> tutti_plugin_host::PluginTimeSignature {
        match &self.placed {
            Placed::Served(c) => c.controls().meter(),
            Placed::InProcess(c) => c.meter(),
        }
    }

    /// What the node counts and holds, for either hosting: its meter cell and
    /// the delivery counters (events it dropped, parameter points it sent,
    /// merged or refused). The same cells the node writes, so a host keeps
    /// this to read them after the plugin has moved into a graph.
    pub fn node_controls(&self) -> tutti_plugin_host::NodeControls {
        match &self.placed {
            Placed::Served(c) => (*c.controls()).clone(),
            Placed::InProcess(c) => c.controls(),
        }
    }

    /// Returns whether the plugin takes chord and scale context (it declared
    /// sequencer context).
    ///
    /// When `true`, wire a `HarmonyNode` (from `tutti-midi-runtime`) to its
    /// event input. `false` for a plugin that declined, and for an in-process
    /// node, whose event input takes only MIDI.
    pub fn takes_harmony(&self) -> bool {
        match &self.placed {
            Placed::Served(c) => c.takes_harmony(),
            Placed::InProcess(_) => false,
        }
    }

    // ---- The graph node ---------------------------------------------------

    /// Binds the plugin to the reader of the project's tempo map it tells the
    /// plugin the tempo from (and CLAP its slope), wherever it
    /// runs: [`PluginClient::bind`] for a subprocess plugin,
    /// `InProcessClient::bind` for an in-process one. The result is the graph
    /// node ([`BoundPlugin`]); `Session::add_pooled_plugin` binds every plugin with
    /// the session's reader, and a host with no session passes a
    /// [`TempoReader::fixed`](tutti_transport::TempoReader::fixed) one.
    #[must_use]
    pub fn bind(self, tempo: tutti_transport::TempoReader) -> BoundPlugin {
        self.bind_with_rebinder(tempo).0
    }

    /// [`bind`](Self::bind), and the builder of the node that replaces this one
    /// when the plugin changes its audio ports, over the same plugin, binding
    /// its successors to `tempo` too.
    #[must_use]
    pub fn bind_with_rebinder(
        self,
        tempo: tutti_transport::TempoReader,
    ) -> (BoundPlugin, PluginRebinder) {
        let (bound, rebinder) = match self.placed {
            Placed::Served(c) => {
                let bound = c.bind(tempo);
                let rebinder = PluginRebinder::served(&bound);
                (BoundPlaced::Served(Box::new(bound)), rebinder)
            }
            Placed::InProcess(c) => {
                let (node, rebinder) = c.bind(tempo);
                (
                    BoundPlaced::InProcess(node),
                    PluginRebinder::in_process(rebinder),
                )
            }
        };
        (BoundPlugin { bound }, rebinder)
    }

    /// Returns the node as the concrete out-of-process [`PluginClient`],
    /// unbound.
    ///
    /// For a host that wants the typed client: its
    /// [`controls`](PluginClient::controls) before insertion, or its own
    /// insertion path ([`bind`](PluginClient::bind) then `Editor::insert`,
    /// which hands back its controls and the fork source that forks it by
    /// state transfer).
    ///
    /// Clone the [`handle`](Self::handle) first if the control surface is
    /// needed after this. Boxed, because a `PluginClient` is several KiB.
    ///
    /// # Errors
    ///
    /// Returns the plugin itself, unchanged (boxed, as the client is), when it
    /// is not a `PluginClient`: an in-process instance. Bind it with
    /// [`bind`](Self::bind) and insert the [`BoundPlugin`]
    /// (`editor.insert(key, kind, plugin.bind(tempo))`), which hands the
    /// editor its fork source (a fork by state transfer, in this process).
    pub fn into_client(self) -> std::result::Result<Box<PluginClient>, Box<Self>> {
        match self.placed {
            Placed::Served(c) => Ok(c),
            Placed::InProcess(_) => Err(Box::new(self)),
        }
    }
}

/// A [`Plugin`] bound to the project's tempo map ([`Plugin::bind`]): the graph
/// node, wherever it runs: `editor.insert(key, kind,
/// plugin.bind(tempo))`.
///
/// Consuming and explicit: a plugin *has* a node, it is not one. Wire the
/// per-block inputs before inserting. The [`PluginHandle`] survives
/// independently; clone it from [`handle`](Plugin::handle) first if the
/// control surface is needed after the node moves into the graph.
///
/// The controls are the subprocess node's [`PluginControls`]; `None` for an
/// in-process plugin, whose meter is reached through [`Plugin::meter`]
/// (take it before binding; its own `IntoNode` hands back its
/// `NodeControls`, which carry its parameter counters too). Either
/// hands the editor its fork source: a fork by state transfer, into a new
/// `plugin-server` for a subprocess plugin, and a second instance in this
/// process for an in-process VST2 one.
///
/// [`PluginControls`]: tutti_plugin_host::PluginControls
pub struct BoundPlugin {
    bound: BoundPlaced,
}

/// Builds the node that replaces a plugin's node when the plugin changes its
/// audio ports ([`Plugin::bind_with_rebinder`]): an in-process plugin's successor
/// is over the same instance, and the Processor moves to it with no
/// restart. A served plugin's launch fixed its shared-memory slab, so ports
/// of other widths are refused by name and nothing is built.
///
/// `Clone`: every clone drives the same plugin.
#[derive(Clone)]
pub struct PluginRebinder {
    inner: std::sync::Arc<std::sync::Mutex<dyn Rebind>>,
}

/// A node [`PluginRebinder::prepare`] built over the plugin's new ports, and
/// the commit that moves the plugin's Processor to it.
///
/// Put the node in the graph ([`into_parts`](Self::into_parts)), and commit
/// only if that succeeded: dropping the commit changes nothing in the plugin.
#[derive(Debug)]
#[must_use = "a node renders nothing until its move is committed"]
pub struct Rebuilt {
    bound: BoundPlugin,
    /// The in-process node's controls; a served node shares the cells of the
    /// client's [`PluginControls`](tutti_plugin_host::PluginControls), which stay.
    controls: Option<tutti_plugin_host::NodeControls>,
    commit: Commit,
}

impl Rebuilt {
    /// The node, to swap in for the one it replaces; its controls (its meter
    /// cell and its parameter delivery's counters); and the commit.
    pub fn into_parts(self) -> (BoundPlugin, Option<tutti_plugin_host::NodeControls>, Commit) {
        (self.bound, self.controls, self.commit)
    }
}

/// One hosting's rebinder, behind the format.
trait Rebind: Send {
    fn layout(&self) -> Option<tutti_plugin_types::Layout>;
    fn settled(&self) -> tutti_plugin_host::Settlement;
    /// Ticks the plugin's lifecycle where its handle does not.
    fn tick(&self, _main: MainThreadMarker) {}
    fn prepare(&self) -> std::result::Result<Rebuilt, tutti_plugin_host::RebindError>;
}

/// An in-process plugin's rebinder, in any format.
struct InProcessRebind(InProcessRebinder);

impl Rebind for InProcessRebind {
    fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.0.layout()
    }

    fn settled(&self) -> tutti_plugin_host::Settlement {
        self.0.settled()
    }

    fn prepare(&self) -> std::result::Result<Rebuilt, tutti_plugin_host::RebindError> {
        let rebuilt = self.0.prepare()?;
        Ok(Rebuilt {
            bound: BoundPlugin {
                bound: BoundPlaced::InProcess(rebuilt.node),
            },
            controls: Some(rebuilt.controls),
            commit: rebuilt.commit,
        })
    }
}

/// A served plugin's rebinder. The node it builds goes into the graph with the
/// served fork source of the node it replaces (which reopens the plugin from
/// where it came, whatever its buses are now).
struct ServedRebind {
    rebinder: tutti_plugin_host::Rebinder<tutti_plugin_ipc::Ipc>,
    fork: crate::host::node::fork::ServedFork,
}

impl Rebind for ServedRebind {
    fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.rebinder.layout()
    }

    fn tick(&self, main: MainThreadMarker) {
        self.rebinder.tick(main);
    }

    fn settled(&self) -> tutti_plugin_host::Settlement {
        self.rebinder.settled()
    }

    fn prepare(&self) -> std::result::Result<Rebuilt, tutti_plugin_host::RebindError> {
        let (node, pending) = self.rebinder.prepare()?;
        Ok(Rebuilt {
            bound: BoundPlugin {
                bound: BoundPlaced::ServedNode(Box::new(node), Box::new(self.fork.clone())),
            },
            controls: None,
            commit: Commit::new(move |main| pending.commit(main)),
        })
    }
}

impl std::fmt::Debug for PluginRebinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRebinder").finish_non_exhaustive()
    }
}

impl PluginRebinder {
    /// The rebinder of an in-process plugin.
    fn in_process(rebinder: InProcessRebinder) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(InProcessRebind(rebinder))),
        }
    }

    /// The rebinder of a served plugin's bound client
    /// ([`PluginClient::rebinder`](crate::PluginClient::rebinder)).
    #[must_use]
    pub fn served(client: &crate::PluginClient<crate::host::node::Bound>) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(ServedRebind {
                rebinder: client.rebinder(),
                fork: crate::host::node::fork::ServedFork::of(client),
            })),
        }
    }

    /// The rebinder, whatever a panic left in its lock: it holds no state of
    /// its own.
    fn lock(&self) -> std::sync::MutexGuard<'_, dyn Rebind + 'static> {
        self.inner.lock_recover()
    }

    /// The ports the plugin has now, as its lifecycle last read them. `None`
    /// for a lost plugin.
    #[must_use]
    pub fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.lock().layout()
    }

    /// Ticks the plugin's lifecycle where its handle does not (a served
    /// plugin's): the restart a change of buses plans runs from here. Main
    /// thread.
    pub fn tick(&self, main: MainThreadMarker) {
        self.lock().tick(main);
    }

    /// Where the Processor's way is after the last commit.
    #[must_use]
    pub fn settled(&self) -> tutti_plugin_host::Settlement {
        self.lock().settled()
    }

    /// A node over the plugin's ports as they are now: see
    /// [`Rebinder::prepare`](tutti_plugin_host::Rebinder::prepare). Nothing is
    /// changed in the plugin until the [`Rebuilt`]'s commit, after the node is
    /// in the graph.
    ///
    /// # Errors
    ///
    /// The refusals of [`Rebinder::prepare`](tutti_plugin_host::Rebinder::prepare).
    pub fn prepare(&self) -> std::result::Result<Rebuilt, tutti_plugin_host::RebindError> {
        self.lock().prepare()
    }
}

enum BoundPlaced {
    Served(Box<PluginClient<crate::host::node::Bound>>),
    /// A node rebuilt over a served plugin's new buses, and the fork source of
    /// the node it replaces.
    ServedNode(
        Box<tutti_plugin_host::PluginNode<tutti_plugin_ipc::Ipc>>,
        Box<crate::host::node::fork::ServedFork>,
    ),
    /// Built only where a format feature is on.
    #[allow(dead_code)]
    InProcess(InProcessNode),
}

impl std::fmt::Debug for BoundPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundPlugin").finish_non_exhaustive()
    }
}

impl tutti_graph::IntoNode for BoundPlugin {
    type Controls = Option<tutti_plugin_host::PluginControls>;

    fn into_node(self) -> (Box<dyn tutti_graph::Node>, Self::Controls) {
        let tutti_graph::NodeParts { node, controls, .. } = self.into_parts();
        (node, controls)
    }

    fn into_parts(self) -> tutti_graph::NodeParts<Self::Controls> {
        match self.bound {
            BoundPlaced::Served(c) => {
                let parts = tutti_graph::IntoNode::into_parts(*c);
                tutti_graph::NodeParts {
                    node: parts.node,
                    controls: Some(parts.controls),
                    fork: parts.fork,
                }
            }
            // The controls are the client's, shared with this node's cells: the
            // session keeps the ones it has.
            BoundPlaced::ServedNode(node, fork) => tutti_graph::NodeParts {
                node,
                controls: None,
                fork: Some(fork),
            },
            // An in-process node has no `PluginControls` (its meter is reached
            // through `Plugin::meter`, taken before binding).
            BoundPlaced::InProcess(node) => {
                let parts = node.into_parts();
                tutti_graph::NodeParts {
                    node: parts.node,
                    controls: None,
                    fork: parts.fork,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::host::node::is_declined;
    use tutti_plugin_types::{Features, LoadedPlugin};

    /// `Plugin`'s capability gate is the one the node accessors use.
    ///
    /// The installers on `Plugin` dispatch across the hostings, so they cannot
    /// share the accessors themselves — but they must not restate the rule.
    /// This pins that the shared predicate is what both consult, so a change to
    /// "declined" reaches the unified surface too.
    #[test]
    fn the_unified_surface_gates_on_the_same_predicate_as_the_node() {
        let declined = LoadedPlugin {
            probed: Features::TRANSPORT,
            features: Features::empty(),
            ..Default::default()
        };
        assert!(is_declined(&declined, Features::TRANSPORT));

        let advertised = LoadedPlugin {
            probed: Features::TRANSPORT,
            features: Features::TRANSPORT,
            ..Default::default()
        };
        assert!(!is_declined(&advertised, Features::TRANSPORT));

        // Unprobed stays open.
        assert!(!is_declined(&LoadedPlugin::default(), Features::TRANSPORT));
    }
}
