//! The plugin as a pool entry: [`Session::plugin_entry`] declares it, the pool
//! loads it, [`Session::add_pooled_plugin`] puts its node in the graph, and
//! [`Session::plugin`] is its handle.
//!
//! The entry's identity is the [`PluginId`] and the [`Hosting`]. The id is
//! resolved and bound against the catalog when the entry is built; the load
//! then opens the plugin, the state going in before it is activated. A served
//! plugin loads on a worker; an in-process one finishes on the main thread, in
//! an [`update`](Session::update) there.
//!
//! What the pool holds is a plugin opened and not yet in a graph
//! ([`LoadedPlugin`]). The node takes it out once; the entry stays, and the
//! node holds it, so an entry the app stops declaring is released with its
//! node. Removing the node leaves the entry loaded with its plugin gone: the
//! entry reloads (a changed identity, or [`retry`](Session::retry) on a lost
//! one), and the new plugin is the next node's.
//!
//! Whatever lands under a key a node is over (a changed identity, a
//! [`retry`](Session::retry) after a refusal) is swapped into the node, if its
//! audio buses and MIDI output match the node's (a crossfade when the shapes
//! fit the graph's check, else a plain swap). The swap waits for a graph that
//! is re-preparing and, for an in-process plugin, for an update on the main
//! thread: a session never updated there does not swap one, and the entry keeps
//! the landed plugin until it is. A plugin whose ports differ fails the entry
//! with [`LoadError::PluginPorts`] and the node keeps the plugin it has:
//! re-inserting a node with new ports is a later change.

use std::cell::RefCell;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tutti_buffers::ChannelLayout;
use tutti_plugin::catalog::ResolvedHosting;
use tutti_plugin_db::{BindError, Catalog, Hosting, Loadable};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::{Features, IpcError, MainThreadMarker, MainWorkHandle, PluginFailure};
use tutti_plugin_types::{PluginDescriptor, PluginId, PluginRef};
use tutti_rt::LockExt;
use tutti_units::SampleRate;

use super::{AnyPlugin, HostPlugin, Mirror, Open, Origin, PluginLoad, PluginLoaded};
use crate::PluginPresets;
use crate::peer::{ResourceIds, sealed};
use crate::{
    Declared, GiveUp, HandleError, Hold, HoldKind, IntoPool, Key, Life, LoadError, Peer, PoolEntry,
    PoolKey, PoolLoad, PoolSet, Presence, ReplaceRefused, Resource, ResourceId, Session,
};
use crate::{OtherPeer, PeerState, SavedPeer};

/// What a pooled plugin's load needs of the session that declared it.
#[derive(Clone, Debug)]
struct Context {
    rate: SampleRate,
    work: MainWorkHandle,
    ids: ResourceIds,
    /// Whether the session recovers a plugin that dies at its node.
    recovery: bool,
}

/// A plugin opened and not yet in a graph: what the node takes.
struct Install {
    plugin: HostPlugin,
    mirror: Option<Mirror>,
    origin: Origin,
}

/// A value that drops on the main thread when it must.
///
/// Dropped on another thread while `main_only`, its value is handed to the
/// session's main-work collector to drop at the next tick on the main thread
/// ([`MainWorkHandle::defer_drop`]); otherwise it drops where it is. The pool
/// drops a released or replaced entry on whatever thread runs
/// [`Session::update`] or [`Session::pool`].
struct Affine<T: Send + 'static> {
    value: Option<T>,
    work: MainWorkHandle,
    main_only: bool,
}

impl<T: Send + 'static> Affine<T> {
    fn new(value: T, work: &MainWorkHandle, main_only: bool) -> Self {
        Self {
            value: Some(value),
            work: work.clone(),
            main_only,
        }
    }

    fn get(&self) -> &T {
        self.value.as_ref().expect("held until dropped")
    }

    /// The value, which now drops where its new owner says.
    fn into_inner(mut self) -> T {
        self.value.take().expect("held until taken")
    }
}

impl<T: Send + 'static> Drop for Affine<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        if self.main_only && MainThreadMarker::new().is_none() {
            self.work.defer_drop(move |_main| drop(value));
        } else {
            drop(value);
        }
    }
}

/// The reading of whether a pooled plugin's live part is gone, shared by the
/// loaded value, the node's record and the handles.
///
/// It holds the plugin's handle only while the plugin is in the entry; the
/// node takes it out ([`release_handle`](Self::release_handle)), so a handle
/// to the entry never keeps a server alive past its node.
struct Standing {
    /// The loaded plugin instance this standing belongs to.
    instance: ResourceId,
    handle: Mutex<Option<Affine<PluginHandle>>>,
    /// The session recovers a plugin that dies at its node.
    recovery: bool,
    /// A node hosts the plugin (and so recovery can act on it).
    hosted: AtomicBool,
    /// The session saw the bridge dead (once a node hosts the plugin), and why.
    dead: Mutex<Option<PluginFailure>>,
    /// Recovery gave up on it, and why.
    gave_up: Mutex<Option<GiveUp>>,
}

impl Standing {
    /// The bridge is dead: seen by the session at the node, or by the handle
    /// while the plugin is still in the entry.
    fn crashed(&self) -> bool {
        self.dead.lock_recover().is_some()
            || self
                .handle
                .lock_recover()
                .as_ref()
                .is_some_and(|handle| handle.get().is_crashed())
    }

    /// The bridge's lifecycle: recovery gave up, or the server is dead and
    /// nothing is bringing it back (recovery is off, or no node hosts the
    /// plugin for recovery to act on). A plugin that recovery reloads in place
    /// reads live throughout.
    fn lost(&self) -> bool {
        self.gave_up.lock_recover().is_some()
            || (self.crashed() && !(self.recovery && self.hosted.load(Ordering::Acquire)))
    }

    /// Why the plugin is not live: that recovery gave up, else the failure of
    /// the bridge the session or the entry's handle saw.
    fn cause(&self) -> PluginCause {
        if let Some(why) = self.gave_up.lock_recover().clone() {
            return PluginCause::GaveUp(why);
        }
        let failure = self.dead.lock_recover().clone().or_else(|| {
            self.handle
                .lock_recover()
                .as_ref()
                .and_then(|handle| handle.get().status().cause().cloned())
        });
        PluginCause::Died(failure.unwrap_or(PluginFailure::Unreported))
    }

    /// The node has the plugin: its death is seen there from now on.
    fn release_handle(&self) {
        let released = self.handle.lock_recover().take();
        drop(released);
    }
}

/// The node's claim on its entry: the hold that keeps the entry from being
/// released while the node lives, and the plugin's standing. Dropped with the
/// node's record.
pub(crate) struct PooledLink {
    pub(crate) key: PoolKey,
    _hold: Hold,
    standing: Arc<Standing>,
}

/// A node's hold on one plugin instance, read from another thread: which
/// instance it is, and whether a node still hosts it.
#[derive(Clone)]
pub(crate) struct Hosted(Arc<Standing>);

impl Hosted {
    /// The plugin instance the node hosts: the one its loads reach.
    pub(crate) fn instance(&self) -> ResourceId {
        self.0.instance
    }

    /// Whether a node still hosts the instance: not once the node is removed,
    /// or a swap replaced the plugin.
    pub(crate) fn is_hosted(&self) -> bool {
        self.0.hosted.load(Ordering::Acquire)
    }
}

impl PooledLink {
    /// The node's hold on its plugin instance.
    pub(crate) fn hosted(&self) -> Hosted {
        Hosted(Arc::clone(&self.standing))
    }

    /// Recovery gave up on the plugin.
    pub(crate) fn gave_up(&self, why: GiveUp) {
        *self.standing.gave_up.lock_recover() = Some(why);
    }

    /// The session saw the plugin's bridge dead, for `failure`.
    pub(crate) fn died(&self, failure: PluginFailure) {
        self.standing.dead.lock_recover().get_or_insert(failure);
    }
}

impl Drop for PooledLink {
    fn drop(&mut self) {
        self.standing.hosted.store(false, Ordering::Release);
    }
}

/// What a plugin says of itself: one snapshot, taken at the load.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    plugin: PluginRef,
    descriptor: PluginDescriptor,
    hosting: Hosting,
    server_pid: Option<u32>,
    presets: PluginPresets,
}

impl PluginInfo {
    /// The plugin the load opened: its file, its format and its id there.
    #[must_use]
    pub const fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The plugin's descriptor: name, vendor, version.
    #[must_use]
    pub const fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// The `plugin-server` process that hosts the plugin, `None` for an
    /// in-process plugin.
    #[must_use]
    pub const fn server_pid(&self) -> Option<u32> {
        self.server_pid
    }

    /// Where the plugin runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`].
    #[must_use]
    pub const fn hosting(&self) -> Hosting {
        self.hosting
    }

    /// What the plugin does with presets, and its list. The list belongs to this
    /// plugin instance: read once for it ([`PluginPresets::is_read`]), never per
    /// call, and a reload makes a new instance with a new info.
    #[must_use]
    pub const fn presets(&self) -> &PluginPresets {
        &self.presets
    }
}

/// Why a pooled plugin is not live.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PluginCause {
    /// The plugin's server died and nothing brought it back: recovery is off,
    /// or no node hosts the plugin. The failure is the bridge's latched cause.
    Died(PluginFailure),
    /// Recovery spent its budget and gave up.
    GaveUp(GiveUp),
}

/// A plugin the pool loaded: [`PoolEntry::Loaded`] of [`PoolPluginEntry`].
///
/// An opened plugin that no graph holds yet, its parameter mirror, and what
/// the handles read of it. The parts that only the main thread may drop
/// (an in-process plugin) go to the main thread when it is dropped on another.
pub struct LoadedPlugin {
    id: ResourceId,
    info: Arc<PluginInfo>,
    standing: Arc<Standing>,
    /// The plugin's ports as a node over it would have them.
    ports: PluginPorts,
    /// Hosted in this process: its swap runs on the main thread only.
    in_process: bool,
    /// What the node takes, once.
    install: RefCell<Option<Affine<Install>>>,
}

impl fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("id", &self.id)
            .field("info", &self.info)
            .field("lost", &self.standing.lost())
            .finish_non_exhaustive()
    }
}

impl LoadedPlugin {
    /// `main` is the proof of the main thread where the load lands there (an
    /// in-process plugin), and `None` for a served plugin.
    fn new(
        cx: &Context,
        open: Open,
        loadable: Loadable,
        config: AudioConfig,
        main: Option<MainThreadMarker>,
    ) -> Result<Self, LoadError> {
        let Open {
            plugin,
            restore: _,
            mirror,
        } = open;
        let plugin = plugin.map_err(plugin_failed)?;
        let hosting = match plugin.hosting() {
            ResolvedHosting::OutOfProcess => Hosting::OutOfProcess,
            ResolvedHosting::InProcess => Hosting::InProcess,
        };
        let served = hosting == Hosting::OutOfProcess;
        let handle = plugin.handle().clone();
        let ports = ports_of(&handle);
        let id = cx.ids.mint();
        let presets = PluginPresets::new(handle.preset_support(), id);
        if presets.support().can_list() {
            match main {
                Some(main) => presets.fill(handle.presets(main)),
                // A served plugin's list is a round trip that takes the main
                // thread's proof: the first update there reads it. The closure
                // holds a clone of the plugin's handle until that update runs,
                // so a session never updated on the main thread keeps it.
                None => {
                    let (list, handle) = (presets.clone(), handle.clone());
                    cx.work.defer(move |main| list.fill(handle.presets(main)));
                }
            }
        }
        let info = PluginInfo {
            plugin: plugin.plugin().clone(),
            descriptor: handle.descriptor().clone(),
            hosting,
            server_pid: handle.server_pid(),
            presets,
        };
        let main_only = !served;
        Ok(Self {
            id,
            info: Arc::new(info),
            standing: Arc::new(Standing {
                instance: id,
                handle: Mutex::new(Some(Affine::new(handle, &cx.work, main_only))),
                recovery: cx.recovery,
                hosted: AtomicBool::new(false),
                dead: Mutex::new(None),
                gave_up: Mutex::new(None),
            }),
            ports,
            in_process: !served,
            install: RefCell::new(Some(Affine::new(
                Install {
                    plugin,
                    mirror,
                    origin: Origin { config, loadable },
                },
                &cx.work,
                main_only,
            ))),
        })
    }

    /// The plugin and what opened it, once: `None` after a node took it.
    fn take_install(&self) -> Option<Install> {
        self.install.borrow_mut().take().map(Affine::into_inner)
    }

    /// Whether a node has not taken the plugin yet.
    fn has_install(&self) -> bool {
        self.install.borrow().is_some()
    }
}

impl Drop for LoadedPlugin {
    /// The value is going: its handles keep no server alive past it.
    fn drop(&mut self) {
        self.standing.release_handle();
    }
}

/// The buses a pooled plugin brings to a node: what a swap compares, so a
/// plugin whose ports differ from its node's is not swapped in.
///
/// The width of each input bus and each output bus, in bus order, and whether
/// the plugin has a MIDI output. Equal totals with another split of buses
/// (a main bus of 4 against a main of 2 and a sidechain of 2) differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginPorts {
    /// The width of each input bus.
    pub inputs: Vec<ChannelLayout>,
    /// The width of each output bus.
    pub outputs: Vec<ChannelLayout>,
    /// Whether the plugin has a MIDI output.
    pub midi_out: bool,
}

impl fmt::Display for PluginPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "inputs {:?}, outputs {:?}{}",
            self.inputs,
            self.outputs,
            if self.midi_out { ", MIDI out" } else { "" }
        )
    }
}

/// The ports a node over `handle`'s plugin has: the one reading a swap
/// compares, for the node's plugin and the incoming one alike.
fn ports_of(handle: &PluginHandle) -> PluginPorts {
    let loaded = handle.loaded();
    PluginPorts {
        inputs: loaded.input_widths(),
        outputs: loaded.output_widths(),
        midi_out: loaded.features.contains(Features::MIDI_OUT),
    }
}

fn plugin_failed(error: IpcError) -> LoadError {
    LoadError::Plugin(Arc::new(error))
}

/// A plugin that loads through the pool: [`Session::plugin_entry`]'s entry.
///
/// Its identity is the [`PluginId`] and the [`Hosting`]; its initial state is
/// applied by the load and never compared.
#[derive(Clone, Debug)]
pub struct PoolPluginEntry {
    id: PluginId,
    resolved: Result<Loadable, BindError>,
    hosting: Hosting,
    state: Option<Arc<PeerState>>,
    cx: Context,
}

impl PoolEntry for PoolPluginEntry {
    type Identity = (PluginId, Hosting);
    type Loaded = LoadedPlugin;
    type Handle = PooledPlugin;

    fn identity(&self) -> (PluginId, Hosting) {
        (self.id.clone(), self.hosting)
    }

    fn load(&self) -> PoolLoad<LoadedPlugin> {
        let cx = self.cx.clone();
        let loadable = match &self.resolved {
            Ok(loadable) => loadable.clone().hosting(self.hosting),
            Err(unresolved) => {
                let unresolved = unresolved.clone();
                return PoolLoad::on_worker(move || Err(LoadError::Unresolved(unresolved)));
            }
        };
        let placement = tutti_plugin::catalog::placement(&loadable);
        let request = PluginLoad {
            loadable,
            rate: cx.rate,
            config: AudioConfig::default(),
            state: self.state.clone(),
            work: cx.work.clone(),
        };
        match placement {
            Err(refused) => PoolLoad::on_worker(move || Err(plugin_failed(refused))),
            // The worker checks the catalog's refusal and leaves the open for
            // the main thread, which an in-process plugin is confined to.
            Ok(ResolvedHosting::InProcess) => PoolLoad::on_worker_then_main(
                move || Ok(request.run()),
                move |loaded: PluginLoaded, main| land(&cx, loaded, Some(main)),
            ),
            Ok(ResolvedHosting::OutOfProcess) => {
                PoolLoad::on_worker(move || land(&cx, request.run(), None))
            }
        }
    }

    fn handle(loaded: &LoadedPlugin, life: &Life) -> PooledPlugin {
        PooledPlugin {
            id: loaded.id,
            info: Arc::clone(&loaded.info),
            standing: Arc::clone(&loaded.standing),
            life: life.clone(),
        }
    }

    fn is_lost(loaded: &LoadedPlugin) -> bool {
        loaded.standing.lost()
    }
}

fn land(
    cx: &Context,
    loaded: PluginLoaded,
    main: Option<MainThreadMarker>,
) -> Result<LoadedPlugin, LoadError> {
    let (loadable, config, open) = loaded.finish(main);
    LoadedPlugin::new(cx, open, loadable, config, main)
}

/// A plugin to keep loaded, under `key`: what [`Session::plugin_entry`]
/// returns, for [`Session::pool`].
#[must_use = "an entry is declared by passing it to Session::pool"]
#[derive(Clone, Debug)]
pub struct PluginDeclaration<K> {
    key: K,
    entry: PoolPluginEntry,
}

impl<K> PluginDeclaration<K> {
    /// The plugin hosted as `hosting` ([`Hosting::Auto`] until this says
    /// otherwise): part of its identity.
    pub fn hosting(mut self, hosting: Hosting) -> Self {
        self.entry.hosting = hosting;
        self
    }

    /// With `state` restored at the load, before the plugin is activated. It
    /// is not identity: the load applies it, and a declaration with another
    /// state under the same key keeps the plugin it has. A refusal by the
    /// plugin is logged, and it loads at its defaults.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another peer
    /// than the one the entry resolved to. An entry that resolved to none is
    /// not checked: its load fails.
    pub fn initial_state(mut self, state: PeerState) -> Result<Self, OtherPeer> {
        if let Ok(loadable) = &self.entry.resolved {
            state.check_for(&SavedPeer::Plugin(loadable.plugin().clone()))?;
        }
        self.entry.state = Some(Arc::new(state));
        Ok(self)
    }
}

impl<K: Ord + Clone + fmt::Debug + Send + Sync + 'static> IntoPool<K> for PluginDeclaration<K> {
    fn into_pool(self, set: &mut PoolSet<K>) {
        // One instance per key: a plugin is never shared.
        Declared::new(self.key, self.entry).into_pool(set);
    }
}

/// A pooled plugin's handle: [`Session::plugin`].
///
/// It exists from the entry's first landing and owns nothing: the plugin lives
/// in its node, and a handle never keeps its server alive. A handle belongs to
/// one load of the entry: once the entry is released or reloaded (a changed
/// identity, a [`retry`](Session::retry)) it reads [`Presence::Closed`], and
/// [`Session::plugin`] gives the new one.
#[derive(Clone)]
pub struct PooledPlugin {
    id: ResourceId,
    info: Arc<PluginInfo>,
    standing: Arc<Standing>,
    life: Life,
}

impl fmt::Debug for PooledPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PooledPlugin")
            .field("id", &self.id)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl sealed::Sealed for PooledPlugin {}

impl Resource for PooledPlugin {
    fn id(&self) -> ResourceId {
        self.id
    }
}

impl Peer for PooledPlugin {
    type Info = PluginInfo;
    type Cause = PluginCause;

    fn info(&self) -> Arc<PluginInfo> {
        Arc::clone(&self.info)
    }

    fn presence(&self) -> Presence<PluginCause> {
        self.life.presence(|| {
            if self.standing.lost() {
                Presence::Lost(self.standing.cause())
            } else {
                Presence::Live
            }
        })
    }
}

/// Why [`Session::add_pooled_plugin`] added no node.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddPluginError<K: fmt::Debug> {
    /// The key has no loaded plugin entry.
    #[error(transparent)]
    Entry(#[from] HandleError<K>),
    /// The entry's plugin is in a node already, or was and the node is gone:
    /// an entry has one plugin, and a new one comes with a reload.
    #[error("the plugin of {0:?} is already in a node, or was")]
    Installed(K),
    /// The entry's plugin is lost; [`retry`](Session::retry) loads it again.
    #[error("the plugin of {0:?} is lost")]
    Lost(K),
}

/// What a swap to a landed plugin came to.
enum Swap {
    /// The node has the new plugin, or no node waits for it.
    Done,
    /// The graph is re-preparing, or the swap is the main thread's and this
    /// update is not: try again at the next update.
    Waiting,
    /// The landing (its generation) cannot be swapped in: the entry fails.
    Refused(u64, LoadError),
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// A plugin to keep loaded under `key`: declare it with
    /// [`pool`](Self::pool), and the session loads it.
    ///
    /// `id` is resolved and bound against `catalog` now
    /// ([`Catalog::resolve`]), and the entry keeps the answer: a plugin that is
    /// missing or in two files fails the load ([`LoadError::Unresolved`]),
    /// and [`retry`](Self::retry) reuses this answer, so it does not see a
    /// plugin installed since. Declare the entry again, built from a catalog
    /// that holds it. The load runs at the session's rate and under the
    /// catalog's blacklist. A served plugin loads on a worker; an in-process
    /// one ([`PluginDeclaration::hosting`], or VST2) finishes in an
    /// [`update`](Self::update) on the main thread, and [`pool`](Self::pool)
    /// refuses it with [`PoolError::NeedsMainThread`](crate::PoolError::NeedsMainThread)
    /// when none will run. Two keys declared with the same id load two plugins.
    ///
    /// The loaded plugin has no node: [`add_pooled_plugin`](Self::add_pooled_plugin)
    /// puts it in the graph.
    pub fn plugin_entry<P>(
        &self,
        key: P,
        id: &PluginId,
        catalog: &Catalog,
    ) -> PluginDeclaration<P> {
        PluginDeclaration {
            key,
            entry: PoolPluginEntry {
                id: id.clone(),
                resolved: catalog.resolve(id).bind(),
                hosting: Hosting::Auto,
                state: None,
                cx: Context {
                    rate: self.graph.prepared().sample_rate(),
                    work: self.plugins.work.clone(),
                    ids: self.pool.resource_ids.clone(),
                    recovery: self.plugins.recovery.is_some(),
                },
            },
        }
    }

    /// The handle of the plugin entry under `key`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of
    /// entry; [`HandleError::NotLoaded`] before the load lands, or after it
    /// failed.
    pub fn plugin<P>(&self, key: &P) -> Result<PooledPlugin, HandleError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.handle::<P, PoolPluginEntry>(key)
    }

    /// Puts the plugin of the entry under `key` in the graph, unwired: its
    /// node, which holds the entry (the pool keeps it, whether or not the
    /// app still declares it, until the node is removed).
    ///
    /// The one way a plugin enters the graph. An in-process plugin was opened
    /// on the main thread by the load, so this call needs no main thread.
    /// An entry has one plugin: after the node is removed the entry stays
    /// loaded and this call is [`AddPluginError::Installed`] until the entry
    /// reloads. See the module for the swap when the entry's identity changes.
    ///
    /// # Errors
    ///
    /// [`AddPluginError::Entry`] when the key has no loaded plugin entry;
    /// [`AddPluginError::Installed`] when a node took its plugin already or a
    /// node links the key (the landed plugin is the node's swap, which this
    /// call leaves alone);
    /// [`AddPluginError::Lost`] when the plugin is lost (it stays so until
    /// [`retry`](Self::retry)).
    pub fn add_pooled_plugin<P>(&mut self, key: &P) -> Result<Key<AnyPlugin>, AddPluginError<P>>
    where
        P: crate::AppKey,
    {
        let pool_key = PoolKey::new(key);
        if self
            .pool
            .entries
            .loaded::<P, PoolPluginEntry>(key)?
            .standing
            .lost()
        {
            return Err(AddPluginError::Lost(key.clone()));
        }
        let hold = self
            .pool
            .entries
            .hold(&pool_key, HoldKind::Node)
            .ok_or_else(|| HandleError::NotPooled(key.clone()))?;
        // A node over the key has the plugin the entry's next one swaps in.
        let linked = self.graph.plugins.values().any(|hosted| {
            hosted
                .pooled
                .as_ref()
                .is_some_and(|link| link.key == pool_key)
        });
        if linked {
            return Err(AddPluginError::Installed(key.clone()));
        }
        let loaded = self.pool.entries.loaded::<P, PoolPluginEntry>(key)?;
        let Some(install) = loaded.take_install() else {
            return Err(AddPluginError::Installed(key.clone()));
        };
        let standing = Arc::clone(&loaded.standing);
        let Install {
            plugin,
            mirror,
            origin,
        } = install;
        let (node, rebinder) = self.install_plugin(plugin, mirror, origin);
        self.link_pooled(node, pool_key, hold, standing, rebinder);
        Ok(node)
    }

    /// Records that `node` is over the pool entry `key`: the plugin is the
    /// node's now, so the entry's handle to it is let go.
    fn link_pooled(
        &mut self,
        node: Key<AnyPlugin>,
        key: PoolKey,
        hold: Hold,
        standing: Arc<Standing>,
        rebinder: tutti_plugin::catalog::PluginRebinder,
    ) {
        standing.hosted.store(true, Ordering::Release);
        standing.release_handle();
        if let Some(hosted) = self.graph.plugins.get_mut(&node.erase()) {
            hosted.pooled = Some(PooledLink {
                key,
                _hold: hold,
                standing,
            });
            hosted.set_rebinder(rebinder);
        }
    }

    /// Swaps the node of every plugin entry whose load landed to the plugin it
    /// brought, and those still waiting (a graph that was re-preparing, or an
    /// in-process plugin and an update off the main thread). An entry whose
    /// swap cannot be made fails: [`LoadError::PluginPorts`] for other ports,
    /// [`LoadError::PluginSwap`] for a graph that refused. The failure follows
    /// the landing's event in the same update, and the unused plugin drops with
    /// the entry's value.
    pub(crate) fn swap_pooled_plugins(&mut self) {
        // A landing with no node over the key (a first load) is not a swap:
        // `swap_pooled` finds no link and does nothing.
        self.plugins
            .pooled_swaps
            .extend(self.pool.entries.landed_keys());
        for key in std::mem::take(&mut self.plugins.pooled_swaps) {
            match self.swap_pooled(&key) {
                Swap::Done => {}
                Swap::Waiting => {
                    self.plugins.pooled_swaps.insert(key);
                }
                Swap::Refused(generation, error) => {
                    self.pool.entries.refuse_landed(&key, generation, error);
                }
            }
        }
    }

    fn swap_pooled(&mut self, key: &PoolKey) -> Swap {
        let Some((node, hold, was)) = self.graph.plugins.iter().find_map(|(node, hosted)| {
            let link = hosted.pooled.as_ref().filter(|link| link.key == *key)?;
            Some((*node, link._hold.clone(), ports_of(&hosted.handle)))
        }) else {
            return Swap::Done;
        };
        let Some((generation, loaded)) = self.pool.entries.landing_of::<PoolPluginEntry>(key)
        else {
            return Swap::Done;
        };
        if !loaded.has_install() {
            return Swap::Done;
        }
        // A replacement with other ports would leave the node's edges on ports
        // that are gone and refuse every later commit.
        if loaded.ports != was {
            let got = loaded.ports.clone();
            return Swap::Refused(generation, LoadError::PluginPorts { got, was });
        }
        // An in-process plugin binds, and the plugin it replaces closes, on the
        // main thread only.
        let off_main = loaded.in_process && MainThreadMarker::new().is_none();
        if off_main || self.graph.is_repreparing() {
            return Swap::Waiting;
        }
        let standing = Arc::clone(&loaded.standing);
        let Some(Install { plugin, origin, .. }) = loaded.take_install() else {
            return Swap::Done;
        };
        let (fade, law) = (tutti_graph::DECLICK, tutti_graph::FadeLaw::default());
        let meter = self.transport.settings().project_meter().clone();
        // The graph is not re-preparing (checked above), so it never hands the
        // plugin back as busy.
        let swapped = match plugin.into_client() {
            Ok(client) => {
                client.controls().set_meter(meter);
                let swapped = self
                    .graph
                    .replace_plugin_with_rebinder(node, client, fade, law);
                if swapped.is_ok()
                    && let Some(hosted) = self.graph.plugins.get_mut(&node)
                {
                    hosted.origin = Some(origin);
                }
                swapped.map_err(|refused| match refused {
                    ReplaceRefused::Failed(refused) => refused,
                    ReplaceRefused::Busy(_) => unreachable!("the graph is not re-preparing"),
                })
            }
            Err(plugin) => {
                plugin.node_controls().set_meter(meter);
                self.graph
                    .replace_hosted_plugin(node, plugin, fade, law)
                    .map_err(|refused| match refused {
                        ReplaceRefused::Failed(refused) => refused,
                        ReplaceRefused::Busy(_) => unreachable!("the graph is not re-preparing"),
                    })
            }
        };
        match swapped {
            Ok(rebinder) => {
                self.link_pooled(Key::new(node), key.clone(), hold, standing, rebinder);
                Swap::Done
            }
            Err(refused) => Swap::Refused(generation, LoadError::PluginSwap(Arc::new(refused))),
        }
    }
}
