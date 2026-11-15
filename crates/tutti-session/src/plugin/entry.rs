//! The plugin as a pool entry: [`Session::plugin_entry`] declares it, the pool
//! loads it, [`Session::add_pooled_plugin`] puts its node in the graph, and
//! [`Session::pooled_plugin`] is its handle.
//!
//! The entry's identity is the [`PluginId`] and the [`Hosting`]. The load is
//! the one a [`PluginLoad`] runs: resolved by the catalog when the entry is
//! built, bound inside the job, the state going in before the plugin is
//! activated. A served plugin loads on a worker; an in-process one finishes on
//! the main thread, in an [`update`](Session::update) there.
//!
//! What the pool holds is a plugin opened and not yet in a graph
//! ([`LoadedPlugin`]). The node takes it out once; the entry stays, and the
//! node holds it, so an entry the app stops declaring is released with its
//! node. A changed identity under the key loads the new plugin, and the node
//! swaps to it when it lands.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_plugin::catalog::{
    AudioConfig, BindError, Catalog, Hosting, Loadable, PluginDescriptor, PluginId, PluginRef,
    PluginState,
};
use tutti_plugin::error::StateError;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{BridgeError, MainThreadMarker, MainWorkHandle};
use tutti_types::SampleRate;

use super::{HostPlugin, Mirror, Open, Origin, PluginLoad, PluginLoaded, PluginNode};
use crate::peer::{ResourceIds, sealed};
use crate::pool::Hold;
use crate::{
    Declared, HandleError, IntoPool, Key, Life, Load, LoadError, Peer, PoolEntry, PoolEvent,
    PoolKey, PoolSet, Presence, Resource, ResourceId, Session, SessionEvent,
};

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
struct Standing {
    handle: Affine<PluginHandle>,
    /// The session recovers a plugin that dies at its node.
    recovery: bool,
    /// A node hosts the plugin (and so recovery can act on it).
    hosted: AtomicBool,
    /// Recovery gave up on it.
    gave_up: AtomicBool,
}

impl Standing {
    /// The bridge's lifecycle: the server is dead and nothing is bringing it
    /// back (recovery is off, or no node hosts it for recovery to act on), or
    /// recovery gave up.
    fn lost(&self) -> bool {
        self.gave_up.load(Ordering::Acquire)
            || (self.handle.get().is_crashed()
                && !(self.recovery && self.hosted.load(Ordering::Acquire)))
    }

    fn cause(&self) -> PluginCause {
        if self.gave_up.load(Ordering::Acquire) {
            PluginCause::GaveUp
        } else {
            PluginCause::Died
        }
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

impl PooledLink {
    /// Recovery gave up on the plugin.
    pub(crate) fn gave_up(&self) {
        self.standing.gave_up.store(true, Ordering::Release);
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
}

impl PluginInfo {
    /// The plugin the load opened: its file, its format and its id there.
    #[must_use]
    pub const fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The plugin's descriptor: id, name, vendor, version.
    #[must_use]
    pub const fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Where the plugin runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`].
    #[must_use]
    pub const fn hosting(&self) -> Hosting {
        self.hosting
    }
}

/// Why a pooled plugin is not live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PluginCause {
    /// The plugin's server died and nothing brought it back: recovery is off,
    /// or no node hosts the plugin.
    Died,
    /// Recovery spent its budget and gave up.
    GaveUp,
}

/// A plugin the pool loaded: [`PoolEntry::Loaded`] of [`PluginEntry`].
///
/// An opened plugin that no graph holds yet, its parameter mirror, and what
/// the handles read of it. The parts that only the main thread may drop
/// (an in-process plugin) go to the main thread when it is dropped on another.
pub struct LoadedPlugin {
    id: ResourceId,
    info: Arc<PluginInfo>,
    standing: Arc<Standing>,
    /// Served by a `plugin-server`: its node can swap to a reloaded plugin.
    served: bool,
    /// What the node takes, once.
    install: Mutex<Option<Affine<Install>>>,
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
    fn new(
        cx: &Context,
        open: Open,
        loadable: Loadable,
        config: AudioConfig,
    ) -> Result<Self, LoadError> {
        let Open {
            plugin,
            restore: _,
            mirror,
        } = open;
        let plugin = plugin.map_err(plugin_failed)?;
        let hosting = plugin.hosting();
        let served = hosting == Hosting::OutOfProcess;
        let handle = plugin.handle().clone();
        let info = PluginInfo {
            plugin: plugin.plugin().clone(),
            descriptor: handle.descriptor().clone(),
            hosting,
        };
        let main_only = !served;
        Ok(Self {
            id: cx.ids.clone().mint(),
            info: Arc::new(info),
            standing: Arc::new(Standing {
                handle: Affine::new(handle, &cx.work, main_only),
                recovery: cx.recovery,
                hosted: AtomicBool::new(false),
                gave_up: AtomicBool::new(false),
            }),
            served,
            install: Mutex::new(Some(Affine::new(
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
        self.install
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map(Affine::into_inner)
    }
}

fn plugin_failed(error: BridgeError) -> LoadError {
    LoadError::Plugin(Arc::new(error))
}

/// A plugin that loads through the pool: [`Session::plugin_entry`]'s entry.
///
/// Its identity is the [`PluginId`] and the [`Hosting`]; its initial state is
/// applied by the load and never compared.
#[derive(Clone, Debug)]
pub struct PluginEntry {
    id: PluginId,
    resolved: Result<Loadable, BindError>,
    hosting: Hosting,
    state: Option<PluginState>,
    cx: Context,
}

impl PoolEntry for PluginEntry {
    type Identity = (PluginId, Hosting);
    type Loaded = LoadedPlugin;
    type Handle = PooledPlugin;

    fn identity(&self) -> (PluginId, Hosting) {
        (self.id.clone(), self.hosting)
    }

    fn load(&self) -> Load<LoadedPlugin> {
        let cx = self.cx.clone();
        let loadable = match &self.resolved {
            Ok(loadable) => loadable.clone().hosting(self.hosting),
            Err(unresolved) => {
                let unresolved = unresolved.clone();
                return Load::on_worker(move || Err(LoadError::Unresolved(unresolved)));
            }
        };
        let placement = loadable.placement();
        let request = PluginLoad {
            loadable,
            rate: cx.rate,
            config: AudioConfig::default(),
            state: self.state.clone(),
            work: cx.work.clone(),
        };
        match placement {
            Err(refused) => Load::on_worker(move || Err(plugin_failed(refused))),
            // The worker checks the catalog's refusal and leaves the open for
            // the main thread, which an in-process plugin is confined to.
            Ok(Hosting::InProcess) => Load::on_worker_then_main(
                move || Ok(request.run()),
                move |loaded: PluginLoaded, main| land(&cx, loaded, Some(main)),
            ),
            Ok(_) => Load::on_worker(move || land(&cx, request.run(), None)),
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
    LoadedPlugin::new(cx, open, loadable, config)
}

/// A plugin to keep loaded, under `key`: what [`Session::plugin_entry`]
/// returns, for [`Session::pool`].
#[must_use = "an entry is declared by passing it to Session::pool"]
#[derive(Clone, Debug)]
pub struct PluginDeclaration<K> {
    key: K,
    entry: PluginEntry,
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
    /// [`StateError::OtherPlugin`] when `state` was saved from another plugin
    /// than the one the entry resolved to. An entry that resolved to none is
    /// not checked: its load fails.
    pub fn initial_state(mut self, state: PluginState) -> Result<Self, StateError> {
        if let Ok(loadable) = &self.entry.resolved {
            state.check_for(loadable.plugin())?;
        }
        self.entry.state = Some(state);
        Ok(self)
    }
}

impl<K: Ord + Clone + fmt::Debug + Send + Sync + 'static> IntoPool<K> for PluginDeclaration<K> {
    fn into_pool(self, set: &mut PoolSet<K>) {
        // One instance per key: a plugin is never shared.
        Declared::new(self.key, self.entry).into_pool(set);
    }
}

/// A pooled plugin's handle: [`Session::pooled_plugin`].
///
/// It exists from the entry's first landing, owns nothing, and follows the
/// entry across a reload. Once the entry is released or swapped to another
/// identity it reads [`Presence::Closed`].
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
    /// The entry's plugin is already in a node.
    #[error("the plugin of {0:?} is already in a node")]
    Installed(K),
}

impl Session {
    /// A plugin to keep loaded under `key`: declare it with
    /// [`pool`](Self::pool), and the session loads it.
    ///
    /// `id` is resolved by `catalog` now ([`Catalog::resolve`]) and bound
    /// inside the load job, so a plugin that is missing or in two files
    /// fails the load ([`LoadError::Unresolved`]); a retry resolves the same
    /// answer. The load runs at the session's rate and under the catalog's
    /// blacklist. A served plugin loads on a worker; an in-process one
    /// ([`PluginDeclaration::hosting`], or VST2) finishes in an
    /// [`update`](Self::update) on the main thread, and [`pool`](Self::pool)
    /// refuses it with [`PoolError::NeedsMainThread`](crate::PoolError::NeedsMainThread)
    /// when none will run.
    ///
    /// The loaded plugin has no node: [`add_pooled_plugin`](Self::add_pooled_plugin)
    /// puts it in the graph.
    pub fn plugin_entry<K>(
        &self,
        key: K,
        id: &PluginId,
        catalog: &Catalog,
    ) -> PluginDeclaration<K> {
        PluginDeclaration {
            key,
            entry: PluginEntry {
                id: id.clone(),
                resolved: catalog.resolve(id).bind(),
                hosting: Hosting::Auto,
                state: None,
                cx: Context {
                    rate: self.graph.prepared().sample_rate(),
                    work: self.work.clone(),
                    ids: self.resource_ids.clone(),
                    recovery: self.recovery.is_some(),
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
    pub fn pooled_plugin<K>(&self, key: &K) -> Result<PooledPlugin, HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        self.pool.handle::<K, PluginEntry>(key)
    }

    /// Puts the plugin of the entry under `key` in the graph, unwired: its
    /// node, which holds the entry (the pool keeps it, whether or not the
    /// app still declares it, until the node is removed).
    ///
    /// The one way a pooled plugin enters the graph; it installs as
    /// [`add_plugin`](Self::add_plugin) does. When the entry reloads under a
    /// changed identity, the node swaps to the new plugin
    /// (`Graph::crossfade_plugin`: a crossfade when the shapes fit, else a
    /// plain swap) when it lands; an in-process plugin's node keeps the plugin
    /// it has.
    ///
    /// # Errors
    ///
    /// [`AddPluginError::Entry`] when the key has no loaded plugin entry;
    /// [`AddPluginError::Installed`] when a node took its plugin already.
    pub fn add_pooled_plugin<K>(&mut self, key: &K) -> Result<Key<PluginNode>, AddPluginError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let pool_key = PoolKey::new(key);
        let loaded = self.pool.loaded::<K, PluginEntry>(key)?;
        let hold = self
            .pool
            .hold(&pool_key)
            .ok_or_else(|| HandleError::NotPooled(key.clone()))?;
        let install = loaded
            .take_install()
            .ok_or_else(|| AddPluginError::Installed(key.clone()))?;
        let standing = Arc::clone(&loaded.standing);
        let Install {
            plugin,
            mirror,
            origin,
        } = install;
        let node = self.install_plugin(plugin, mirror, origin);
        self.link_pooled(node, pool_key, hold, standing);
        Ok(node)
    }

    /// Records that `node` is over the pool entry `key`.
    fn link_pooled(
        &mut self,
        node: Key<PluginNode>,
        key: PoolKey,
        hold: Hold,
        standing: Arc<Standing>,
    ) {
        standing.hosted.store(true, Ordering::Release);
        if let Some(hosted) = self.graph.plugins.get_mut(&node.erase()) {
            hosted.pooled = Some(PooledLink {
                key,
                _hold: hold,
                standing,
            });
        }
    }

    /// Swaps the node of every plugin entry that reloaded to the plugin the
    /// reload brought. A node of an in-process plugin keeps its plugin.
    pub(crate) fn swap_pooled_plugins(&mut self, events: &[SessionEvent]) {
        for event in events {
            let SessionEvent::Pool {
                key,
                event: PoolEvent::Reloaded,
            } = event
            else {
                continue;
            };
            let Some((node, hold)) = self.graph.plugins.iter().find_map(|(node, hosted)| {
                let link = hosted.pooled.as_ref().filter(|link| link.key == *key)?;
                Some((*node, link._hold.clone()))
            }) else {
                continue;
            };
            let Some(loaded) = self.pool.loaded_of::<PluginEntry>(key) else {
                continue;
            };
            if !loaded.served {
                tracing::warn!(
                    "plugin {node:?}: its entry reloaded, and an in-process node keeps its plugin"
                );
                continue;
            }
            let standing = Arc::clone(&loaded.standing);
            let Some(Install { plugin, origin, .. }) = loaded.take_install() else {
                continue;
            };
            let Ok(client) = plugin.into_client() else {
                continue;
            };
            client
                .controls()
                .set_meter(self.click_settings.project_meter());
            match self.graph.crossfade_plugin(
                node,
                client,
                tutti_graph::DECLICK,
                tutti_graph::FadeLaw::default(),
            ) {
                Ok(_) => {
                    if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                        hosted.origin = Some(origin);
                    }
                    self.link_pooled(Key::new(node), key.clone(), hold, standing);
                }
                Err(refused) => {
                    tracing::warn!(
                        "plugin {node:?}: the swap to its reloaded plugin was refused: {refused}"
                    );
                }
            }
        }
    }
}
