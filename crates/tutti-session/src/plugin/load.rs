//! The plugin as a value: [`Session::load_plugin`] returns a [`LoadPlugin`]
//! that, when the app runs it on the main thread, opens the plugin and returns a
//! [`LoadedPlugin`] handle; [`Session::add_plugin`] puts the plugin in the graph
//! as a node, and [`Session::swap_plugin`] replaces the plugin of a node.
//!
//! The id is resolved and bound against the catalog when the load is built; the
//! load then opens the plugin, the state going in before it is activated. A load
//! holds the session's rate, its collector of main-thread work and a weak
//! handle to its executor, which it upgrades only for the launch of a served
//! plugin.
//!
//! A [`LoadedPlugin`] is a plugin opened and not yet in a graph. A clone shares
//! it, a node takes the plugin out once and holds a clone of the handle, and
//! releasing is dropping every clone: the plugin closes with the last one (an
//! in-process plugin on the main thread, whichever thread drops it; a served
//! one reaps its server). The session keeps no cache: two loads of one id are
//! two plugins. Removing the node leaves the handle without a plugin.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_graph::NodeKey;
use tutti_platform::Asset;
use tutti_plugin_db::{BindError, Catalog, Hosting, Loadable};
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::{MainThreadMarker, PluginFailure, PluginId};
use tutti_rt::LockExt;

use super::entry::{Context, Install, PluginCause, PluginInfo, PluginPorts, Shared, Standing};
use super::entry::{plugin_failed, ports_of};
use super::{AnyPlugin, HostPlugin, Origin, PluginLoad};
use crate::loaded::{Stamp, caught};
use crate::peer::sealed;
use crate::{CrossfadeRefused, GiveUp, Hold, Key, LoadError, NoSuchNode, Peer, PoolKey, Presence};
use crate::{OtherPeer, PeerState, ReplaceRefused, Resource, ResourceId, SavedPeer, Session};

/// A plugin to open, built by [`Session::load_plugin`] and not yet run:
/// [`run`](Self::run) returns a [`LoadedPlugin`].
///
/// Its identity is the [`PluginId`] and the [`Hosting`]; its initial state is
/// applied by the load. It is not `Send`: every plugin value runs on the main
/// thread, served or in process; dropping it unrun cancels it.
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub struct LoadPlugin {
    id: PluginId,
    resolved: Result<Loadable, BindError>,
    hosting: Hosting,
    state: Option<Arc<PeerState>>,
    cx: Context,
    main: PhantomData<*const ()>,
}

impl LoadPlugin {
    /// The plugin hosted as `hosting` ([`Hosting::Auto`] until this says
    /// otherwise).
    pub const fn hosting(mut self, hosting: Hosting) -> Self {
        self.hosting = hosting;
        self
    }

    /// With `state` restored at the load, before the plugin is activated. A
    /// refusal by the plugin is logged, and it loads at its defaults.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another peer than the one the
    /// id resolved to. An id that resolved to none is not checked: the load
    /// fails.
    pub fn initial_state(mut self, state: PeerState) -> Result<Self, OtherPeer> {
        if let Ok(loadable) = &self.resolved {
            state.check_for(&SavedPeer::Plugin(loadable.plugin().clone()))?;
        }
        self.state = Some(Arc::new(state));
        Ok(self)
    }

    /// The id the load opens.
    #[must_use]
    pub const fn id(&self) -> &PluginId {
        &self.id
    }

    /// Opens the plugin on the calling thread and restores the initial state.
    /// A served plugin's server is launched here, which blocks for up to the
    /// load deadline (half a second to fifteen).
    ///
    /// The plugin file's [`Identity`](tutti_platform::Identity) is taken before
    /// the open, so a rewrite during it shows in [`LoadedPlugin::changed`].
    ///
    /// # Errors
    ///
    /// [`LoadError::Unresolved`] when the id names no plugin of the catalog, or
    /// several; [`LoadError::Plugin`] when the catalog or the format refuses the
    /// hosting or the plugin does not open; [`LoadError::Panicked`] when a
    /// plugin or the host panicked in the open.
    pub fn run(self, main: MainThreadMarker) -> Result<LoadedPlugin, LoadError> {
        caught(|| self.open(main))
    }

    fn open(self, main: MainThreadMarker) -> Result<LoadedPlugin, LoadError> {
        let loadable = match &self.resolved {
            Ok(loadable) => loadable.clone().hosting(self.hosting),
            Err(unresolved) => return Err(LoadError::Unresolved(unresolved.clone())),
        };
        tutti_plugin::catalog::placement(&loadable).map_err(plugin_failed)?;
        let stamp = Stamp::take(Asset::from(loadable.plugin().path()));
        let cx = self.cx;
        let request = PluginLoad {
            loadable,
            rate: cx.rate,
            config: AudioConfig::default(),
            state: self.state,
            work: cx.work.clone(),
            executor: cx.executor.clone(),
        };
        let (loadable, config, open) = request.run().finish(Some(main));
        let shared = Shared::new(&cx, stamp, open, loadable, config, Some(main))?;
        Ok(LoadedPlugin {
            shared: Arc::new(shared),
        })
    }
}

/// A plugin opened and not yet in a graph: [`LoadPlugin::run`]'s output.
///
/// A clone shares the plugin, and the handle holds no plugin once a node took
/// it ([`Session::add_plugin`]): the node has it, and the handle keeps what the
/// load read of it ([`info`](Peer::info)) and whether it is live. Releasing is
/// dropping every clone, a node's among them.
#[derive(Clone, Debug)]
pub struct LoadedPlugin {
    pub(super) shared: Arc<Shared>,
}

impl LoadedPlugin {
    /// The plugin file the load opened.
    #[must_use]
    pub fn asset(&self) -> &Asset {
        self.shared.stamp.asset()
    }

    /// Whether the plugin's file is not what was opened: its
    /// [`Identity`](tutti_platform::Identity) (the path with its length and
    /// modification time) now differs from the one taken before the load. This
    /// reads the file's metadata, and nothing else.
    ///
    /// To reload, run [`Session::load_plugin`] again and swap the new plugin in
    /// ([`Session::swap_plugin`]).
    #[must_use]
    pub fn changed(&self) -> bool {
        self.shared.stamp.changed()
    }

    /// The plugin as a node to add ([`Session::add_plugin`]): a clone of this
    /// handle, which the node holds for as long as it lives.
    pub fn node(&self) -> PluginNode {
        PluginNode {
            plugin: self.clone(),
        }
    }

    /// The plugin's ports as a node over it would have them.
    #[must_use]
    pub fn ports(&self) -> &PluginPorts {
        &self.shared.ports
    }
}

impl sealed::Sealed for LoadedPlugin {}

impl Resource for LoadedPlugin {
    fn id(&self) -> ResourceId {
        self.shared.id
    }
}

impl Peer for LoadedPlugin {
    type Info = PluginInfo;
    type Cause = PluginCause;

    fn info(&self) -> Arc<PluginInfo> {
        Arc::clone(&self.shared.info)
    }

    fn presence(&self) -> Presence<PluginCause> {
        if self.shared.standing.lost() {
            Presence::Lost(self.shared.standing.cause())
        } else {
            Presence::Live
        }
    }
}

/// A [`LoadedPlugin`] as a node to put in a graph: [`LoadedPlugin::node`]'s
/// value, taken by [`Session::add_plugin`] and [`Session::swap_plugin`].
#[derive(Clone, Debug)]
#[must_use = "a node does nothing until it is added"]
pub struct PluginNode {
    plugin: LoadedPlugin,
}

impl PluginNode {
    /// The handle this node was made from.
    #[must_use]
    pub const fn plugin(&self) -> &LoadedPlugin {
        &self.plugin
    }
}

/// Why [`Session::add_plugin`] added no node.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddPluginError {
    /// The plugin is in a node already, or was and the node is gone: a loaded
    /// plugin has one node, and a new one comes with a new load.
    #[error("the plugin is already in a node, or was")]
    Installed,
    /// The plugin is lost: its server died before it had a node.
    #[error("the plugin is lost")]
    Lost,
}

/// Why [`Session::swap_plugin`] swapped nothing. The node keeps the plugin it
/// has in each case.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SwapPluginError {
    /// No plugin is hosted at the node.
    #[error(transparent)]
    NoSuchNode(#[from] NoSuchNode),
    /// The incoming plugin is in a node already, or was.
    #[error("the plugin is already in a node, or was")]
    Installed,
    /// The incoming plugin is lost.
    #[error("the plugin is lost")]
    Lost,
    /// The incoming plugin has other ports than the node's, which would leave
    /// the node's edges on ports that are gone.
    #[error("the plugin has {got} where its node has {was}")]
    Ports {
        /// The incoming plugin's ports.
        got: PluginPorts,
        /// The node's.
        was: PluginPorts,
    },
    /// The graph is re-preparing: swap again after the next update.
    #[error("the graph is re-preparing; swap again after the next update")]
    Busy,
    /// The graph refused the swap.
    #[error("the node could not swap to the plugin: {0}")]
    Graph(Arc<CrossfadeRefused>),
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// A plugin to open, as a value the app runs on the main thread
    /// ([`LoadPlugin::run`]).
    ///
    /// `id` is resolved and bound against `catalog` now ([`Catalog::resolve`]),
    /// and the load keeps the answer: a plugin that is missing or in two files
    /// fails the load ([`LoadError::Unresolved`]). The load runs at the
    /// session's rate and under the catalog's blacklist. The app keeps its
    /// catalog. Two loads of one id are two plugins.
    ///
    /// The loaded plugin has no node: [`add_plugin`](Self::add_plugin) puts it in
    /// the graph.
    pub fn load_plugin(&self, id: &PluginId, catalog: &Catalog) -> LoadPlugin {
        LoadPlugin {
            id: id.clone(),
            resolved: catalog.resolve(id).bind(),
            hosting: Hosting::Auto,
            state: None,
            cx: Context {
                rate: self.graph.prepared().sample_rate(),
                work: self.plugins.work.clone(),
                executor: self.plugins.executor.clone(),
                ids: self.pool.resource_ids.clone(),
                recovery: self.plugins.recovery.is_some(),
            },
            main: PhantomData,
        }
    }

    /// Puts the plugin of `node` in the graph, unwired: the plugin's node,
    /// which holds a clone of its handle until the node is removed.
    ///
    /// The one way a loaded plugin enters the graph. An in-process plugin was
    /// opened on the main thread by the load, so this call needs no main
    /// thread. A loaded plugin has one node: after the node is removed this call
    /// is [`AddPluginError::Installed`], and a new plugin comes with a new load.
    ///
    /// # Errors
    ///
    /// [`AddPluginError::Installed`] when a node took the plugin already;
    /// [`AddPluginError::Lost`] when the plugin is lost.
    pub fn add_plugin(&mut self, node: PluginNode) -> Result<Key<AnyPlugin>, AddPluginError> {
        let plugin = node.plugin;
        if plugin.shared.standing.lost() {
            return Err(AddPluginError::Lost);
        }
        let Some(Install {
            plugin: opened,
            mirror,
            origin,
        }) = plugin.shared.take_install()
        else {
            return Err(AddPluginError::Installed);
        };
        let (key, rebinder) = self.install_plugin(opened, mirror, origin);
        self.link_node(key, NodeLink::of_handle(plugin), rebinder);
        Ok(key)
    }

    /// Replaces the plugin at `node` with `with`'s: a crossfade when the shapes
    /// fit the graph's check, else a plain swap, at the same key, so the node's
    /// wiring and lanes stay. The outgoing plugin retires (an in-process one
    /// closes on the main thread, which `main` proves this call is on).
    ///
    /// # Errors
    ///
    /// [`SwapPluginError::NoSuchNode`] when no plugin is hosted at `node`;
    /// [`SwapPluginError::Installed`] and [`SwapPluginError::Lost`] for an
    /// incoming plugin that is in a node or lost; [`SwapPluginError::Ports`] for
    /// other audio buses or MIDI output than the node's;
    /// [`SwapPluginError::Busy`] while the graph re-prepares;
    /// [`SwapPluginError::Graph`] when the graph refused. The node keeps its
    /// plugin, and the incoming one stays available for a later swap in every
    /// case but the last.
    pub fn swap_plugin(
        &mut self,
        node: Key<AnyPlugin>,
        with: PluginNode,
        _main: MainThreadMarker,
    ) -> Result<(), SwapPluginError> {
        let plugin = with.plugin;
        let was = match self.graph.plugins.get(&node.erase()) {
            Some(hosted) => ports_of(&hosted.handle),
            None => return Err(NoSuchNode::new(node.erase()).into()),
        };
        if !plugin.shared.has_install() {
            return Err(SwapPluginError::Installed);
        }
        if plugin.shared.standing.lost() {
            return Err(SwapPluginError::Lost);
        }
        // A replacement with other ports would leave the node's edges on ports
        // that are gone and refuse every later commit.
        if plugin.shared.ports != was {
            let got = plugin.shared.ports.clone();
            return Err(SwapPluginError::Ports { got, was });
        }
        if self.graph.is_repreparing() {
            return Err(SwapPluginError::Busy);
        }
        let Some(Install {
            plugin: opened,
            origin,
            ..
        }) = plugin.shared.take_install()
        else {
            return Err(SwapPluginError::Installed);
        };
        let rebinder = self
            .swap_installed(node.erase(), opened, origin)
            .map_err(|refused| SwapPluginError::Graph(Arc::new(refused)))?;
        self.link_node(node, NodeLink::of_handle(plugin), rebinder);
        Ok(())
    }
}

/// What a node's record keeps of the plugin it is over: its standing, and the
/// claim that keeps the plugin's owner alive while the node lives. Dropped with
/// the node's record.
pub(crate) struct NodeLink {
    standing: Arc<Standing>,
    keeps: Keeps,
}

/// What keeps a plugin's owner alive while its node does.
enum Keeps {
    /// The pool entry the plugin loaded through: its hold.
    Entry { key: PoolKey, hold: Hold },
    /// The handle the plugin was loaded as: the node's own clone.
    Handle { _plugin: LoadedPlugin },
}

impl NodeLink {
    fn of_handle(plugin: LoadedPlugin) -> Self {
        Self {
            standing: Arc::clone(&plugin.shared.standing),
            keeps: Keeps::Handle { _plugin: plugin },
        }
    }

    pub(super) fn of_entry(key: PoolKey, hold: Hold, standing: Arc<Standing>) -> Self {
        Self {
            standing,
            keeps: Keeps::Entry { key, hold },
        }
    }

    /// The pool entry the node is over, if it is over one.
    pub(super) fn entry_key(&self) -> Option<&PoolKey> {
        match &self.keeps {
            Keeps::Entry { key, .. } => Some(key),
            Keeps::Handle { .. } => None,
        }
    }

    /// The hold of the pool entry the node is over.
    pub(super) fn hold(&self) -> Option<Hold> {
        match &self.keeps {
            Keeps::Entry { hold, .. } => Some(hold.clone()),
            Keeps::Handle { .. } => None,
        }
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

impl Drop for NodeLink {
    fn drop(&mut self) {
        self.standing.hosted.store(false, Ordering::Release);
    }
}

impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// Swaps the plugin at `node` for `plugin`, opened with `origin`, as
    /// [`replace`](crate::Graph::replace) swaps any node: crossfading when the
    /// shapes fit the graph's check, else a plain swap at the same key. The
    /// graph is not re-preparing: the caller checked, so it never hands the
    /// plugin back as busy.
    pub(super) fn swap_installed(
        &mut self,
        node: NodeKey,
        plugin: HostPlugin,
        origin: Origin,
    ) -> Result<tutti_plugin::catalog::PluginRebinder, CrossfadeRefused> {
        let (fade, law) = (tutti_graph::DECLICK, tutti_graph::FadeLaw::default());
        let meter = self.transport.settings().project_meter().clone();
        match plugin.into_client() {
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
        }
    }

    /// Records that `node` is over the plugin `link` stands for: the plugin is
    /// the node's now, so the owner's handle to it is let go.
    pub(super) fn link_node(
        &mut self,
        node: Key<AnyPlugin>,
        link: NodeLink,
        rebinder: tutti_plugin::catalog::PluginRebinder,
    ) {
        link.standing.hosted.store(true, Ordering::Release);
        link.standing.release_handle();
        if let Some(hosted) = self.graph.plugins.get_mut(&node.erase()) {
            hosted.link = Some(link);
            hosted.set_rebinder(rebinder);
        }
    }
}
