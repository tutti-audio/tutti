//! The plugin as a value: [`Session::load_plugin`] returns a [`LoadPlugin`]
//! that, when the app runs it on the main thread, opens the plugin and returns
//! the [`PluginNode`] that owns it; [`Session::add_plugin`] puts the node in the
//! graph, and [`Session::swap_plugin`] replaces the plugin at a node with one.
//!
//! The id is resolved and bound against the catalog when the load is built; the
//! load then opens the plugin, the state going in before it is activated. A load
//! holds the session's identity and rate, its collector of main-thread work and
//! a weak handle to its executor, which it upgrades only for the launch of a
//! served plugin.
//!
//! One opened plugin is one [`PluginNode`], which a session takes once. Its
//! [`LoadedPlugin`] is the cloneable observer: what the load read of the plugin
//! and whether it is live, or [`Closed`](Presence::Closed) once no plugin is
//! behind it. An unadded plugin closes when its node is dropped, and an added
//! one with its node in the graph (an in-process plugin on the main thread,
//! whichever thread drops it; a served one reaps its server). The session keeps
//! no cache: two loads of one id are two plugins.

use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_graph::NodeKey;
use tutti_platform::Asset;
use tutti_plugin_db::{BindError, Catalog, Hosting, Loadable};
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{MainThreadMarker, PluginFailure, PluginFormat, PluginId, PluginRef};
use tutti_rt::LockExt;

use super::opened::ports_of;
use super::opened::{Context, Install, PluginCause, PluginInfo, PluginPorts, Shared, Standing};
use super::{AnyPlugin, HostPlugin, Origin, PluginLoad};
use crate::apply::SessionId;
use crate::loaded::{Stamp, caught};
use crate::peer::sealed;
use crate::{CrossfadeRefused, GiveUp, Key, LoadError, NoSuchNode, Peer, Presence};
use crate::{OtherPeer, PeerState, ReplaceRefused, Resource, ResourceId, SavedPeer, Session};

/// A plugin to open, built by [`Session::load_plugin`] and not yet run:
/// [`run`](Self::run) returns the [`PluginNode`].
///
/// Its identity is the [`PluginId`] and the [`Hosting`]; its initial state is
/// applied by the load. It is not `Send`: every plugin value runs on the main
/// thread, served or in process; dropping it unrun cancels it.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::LoadPlugin>();
/// ```
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub struct LoadPlugin {
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

    /// Opens the plugin on the calling thread and restores the initial state.
    /// A served plugin's server is launched here, which blocks for up to the
    /// load deadline (half a second to fifteen).
    ///
    /// The plugin module's [`Identity`](tutti_platform::Identity) is taken
    /// before the open, so a rewrite during it shows in
    /// [`LoadedPlugin::changed`].
    ///
    /// # Errors
    ///
    /// [`LoadError::Unresolved`] when the id names no plugin of the catalog, or
    /// several; [`LoadError::Module`] when its bundle holds no module for this
    /// target; [`LoadError::Plugin`] when the catalog or the format refuses the
    /// hosting or the plugin does not open; [`LoadError::Panicked`] when a
    /// plugin or the host panicked in the open.
    pub fn run(self, main: MainThreadMarker) -> Result<PluginNode, LoadError> {
        caught(|| self.open(main))
    }

    fn open(self, main: MainThreadMarker) -> Result<PluginNode, LoadError> {
        let loadable = match &self.resolved {
            Ok(loadable) => loadable.clone().hosting(self.hosting),
            Err(unresolved) => return Err(LoadError::Unresolved(unresolved.clone())),
        };
        let stamp = Stamp::take(Asset::from(module_of(loadable.plugin())?));
        let cx = self.cx;
        let request = PluginLoad {
            loadable: loadable.clone(),
            rate: cx.rate,
            config: AudioConfig::default(),
            state: self.state,
            work: cx.work.clone(),
            executor: cx.executor.clone(),
        };
        let config = request.config.clone();
        let open = request.run_on(main);
        let (shared, install) = Shared::new(&cx, stamp, open, loadable, config, main)?;
        Ok(PluginNode {
            handle: LoadedPlugin {
                shared: Arc::new(shared),
            },
            install: Some(Box::new(install)),
            session: cx.session,
        })
    }
}

/// The file a plugin's code is in: the module inside a bundle directory, else the
/// path itself. A rewrite of the plugin shows in this file's metadata, where the
/// bundle directory's does not move.
///
/// # Errors
///
/// [`LoadError::Module`] for a bundle that holds no module this target loads: a
/// plugin that opened has one, so there is no other file to stamp.
fn module_of(plugin: &PluginRef) -> Result<PathBuf, LoadError> {
    let kind = match plugin.format() {
        Ok(PluginFormat::Vst2) => ModuleKind::Vst2,
        Ok(PluginFormat::Vst3) => ModuleKind::Vst3,
        Ok(PluginFormat::Clap) => ModuleKind::Clap,
        Ok(PluginFormat::AudioUnit) => ModuleKind::Au,
        // A path of no known format is a plain file: its own module.
        _ => return Ok(plugin.path().to_path_buf()),
    };
    native_module_path(plugin.path(), kind).map_err(|error| LoadError::Module(Arc::new(error)))
}

/// What an app observes of an opened plugin: [`PluginNode::handle`]. A clone
/// shares the readings, and holds no plugin: the node owns it.
///
/// It carries what the load read of the plugin ([`info`](Peer::info)) and
/// whether the plugin is live ([`presence`](Peer::presence)): [`Closed`](Presence::Closed)
/// once the node was dropped unadded, removed from the graph, or swapped out.
///
/// ```
/// fn send_sync<T: Send + Sync>() {}
/// send_sync::<tutti_session::LoadedPlugin>();
/// ```
#[derive(Clone, Debug)]
pub struct LoadedPlugin {
    shared: Arc<Shared>,
}

impl LoadedPlugin {
    /// The plugin module the load opened: the file in a bundle, else the path.
    #[must_use]
    pub fn asset(&self) -> &Asset {
        self.shared.stamp.asset()
    }

    /// Whether the plugin's module is not what was opened: its
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
        let standing = &self.shared.standing;
        if standing.retired.load(Ordering::Acquire) {
            Presence::Closed
        } else if standing.lost() {
            Presence::Lost(standing.cause())
        } else {
            Presence::Live
        }
    }
}

/// An opened plugin, owned: [`LoadPlugin::run`]'s output, which
/// [`Session::add_plugin`] and [`Session::swap_plugin`] consume. A plugin is in
/// one node, so the node is not `Clone`.
///
/// It is `Send`: the main thread opened it, and a session on any thread takes it.
/// Dropped before it is added, it closes its plugin (an in-process plugin on the
/// main thread, whichever thread drops it).
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::PluginNode>();
/// ```
#[must_use = "a node does nothing until it is added"]
pub struct PluginNode {
    handle: LoadedPlugin,
    /// The plugin, until a session takes it. Boxed: an opened plugin is far
    /// larger than the errors that hand a node back.
    install: Option<Box<Install>>,
    /// The session that loaded it, the only one it may be added to.
    session: SessionId,
}

impl fmt::Debug for PluginNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginNode")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl PluginNode {
    /// The observer of this plugin: a clone of it outlives the node.
    pub fn handle(&self) -> LoadedPlugin {
        self.handle.clone()
    }

    /// The plugin and its handle, the node spent: the caller owns the plugin's
    /// retirement from here.
    fn into_parts(mut self) -> (Install, LoadedPlugin) {
        let install = self
            .install
            .take()
            .expect("a node holds its plugin until taken");
        (*install, self.handle.clone())
    }
}

impl Drop for PluginNode {
    /// Dropped with its plugin still in it: the plugin closes, and its handle
    /// keeps no server alive.
    fn drop(&mut self) {
        if self.install.is_some() {
            let standing = &self.handle.shared.standing;
            standing.retired.store(true, Ordering::Release);
            standing.release_handle();
        }
    }
}

/// Why [`Session::add_plugin`] added no node.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddPluginError {
    /// The plugin is lost: its server died before it had a node. It is dropped.
    #[error("the plugin is lost")]
    Lost,
    /// Another session loaded the plugin, at its own rate and with its own
    /// collector of main-thread work. The node is handed back.
    #[error("the plugin was loaded by another session")]
    OtherSession(PluginNode),
}

/// Why [`Session::swap_plugin`] swapped nothing. The node keeps the plugin it
/// has in each case, and the incoming node is handed back in each case but
/// [`Graph`](Self::Graph).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SwapPluginError {
    /// No plugin is hosted at the node.
    #[error("{0}")]
    NoSuchNode(NoSuchNode, PluginNode),
    /// The incoming plugin is lost.
    #[error("the plugin is lost")]
    Lost(PluginNode),
    /// Another session loaded the incoming plugin.
    #[error("the plugin was loaded by another session")]
    OtherSession(PluginNode),
    /// The incoming plugin has other ports than the node's, which would leave
    /// the node's edges on ports that are gone.
    #[error("the plugin has {got} where its node has {was}")]
    Ports {
        /// The incoming plugin's ports.
        got: Box<PluginPorts>,
        /// The node's.
        was: Box<PluginPorts>,
        /// The incoming node.
        node: PluginNode,
    },
    /// The graph is re-preparing: swap again after an update.
    #[error("the graph is re-preparing; swap again after an update")]
    Busy(PluginNode),
    /// The graph refused the swap; the incoming plugin is closed.
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
    /// The opened plugin has no node in the graph yet:
    /// [`add_plugin`](Self::add_plugin) puts it there, in this session only.
    pub fn load_plugin(&self, id: &PluginId, catalog: &Catalog) -> LoadPlugin {
        LoadPlugin {
            resolved: catalog.resolve(id).bind(),
            hosting: Hosting::Auto,
            state: None,
            cx: Context {
                rate: self.graph.prepared().sample_rate(),
                work: self.plugins.work.clone(),
                executor: self.plugins.executor.clone(),
                ids: self.pool.resource_ids.clone(),
                session: self.id,
                recovery: self.plugins.recovery.is_some(),
            },
            main: PhantomData,
        }
    }

    /// Puts the plugin of `node` in the graph, unwired. The node's record holds
    /// a clone of the plugin's handle until the record retires, and then the
    /// handle reads [`Closed`](Presence::Closed).
    ///
    /// The one way a loaded plugin enters the graph. It takes no main thread:
    /// the plugin was opened on it, and binding it to a graph node is not
    /// main-thread work, so a session on another thread adds it too.
    ///
    /// # Errors
    ///
    /// [`AddPluginError::Lost`] when the plugin is lost;
    /// [`AddPluginError::OtherSession`] when another session loaded it.
    pub fn add_plugin(&mut self, node: PluginNode) -> Result<Key<AnyPlugin>, AddPluginError> {
        if node.session != self.id {
            return Err(AddPluginError::OtherSession(node));
        }
        if node.handle.shared.standing.lost() {
            return Err(AddPluginError::Lost);
        }
        let (install, handle) = node.into_parts();
        let Install {
            plugin,
            mirror,
            origin,
        } = install;
        let (key, rebinder) = self.install_plugin(plugin, mirror, origin);
        self.link_node(key, NodeLink::of(handle), rebinder);
        Ok(key)
    }

    /// Replaces the plugin at `node` with `with`'s: a crossfade when the shapes
    /// fit the graph's check, else a plain swap, at the same key, so the node's
    /// wiring and lanes stay. The record at the key is carried over to the
    /// incoming plugin with nothing of the outgoing plugin's state, so a node
    /// that was recovering reads healthy again, and a host-inserted node has the
    /// loaded kind's death behaviour afterwards: it stays silent at its key
    /// rather than being removed.
    ///
    /// The outgoing plugin is parked in the graveyard and closes at the next
    /// update on the main thread; its handle reads [`Closed`](Presence::Closed).
    /// `main` proves this call is on the main thread, for the inline close of a
    /// refused in-process incoming plugin.
    ///
    /// # Errors
    ///
    /// The incoming node comes back in the error, and `node` keeps its plugin:
    /// [`SwapPluginError::NoSuchNode`] when no plugin is hosted at `node`;
    /// [`SwapPluginError::Lost`] and [`SwapPluginError::OtherSession`] for an
    /// incoming plugin that is lost or loaded by another session;
    /// [`SwapPluginError::Ports`] for other audio buses or MIDI output than the
    /// node's; [`SwapPluginError::Busy`] while the graph re-prepares: swap again
    /// after an update. [`SwapPluginError::Graph`] means the graph refused, and
    /// the incoming plugin is closed.
    pub fn swap_plugin(
        &mut self,
        node: Key<AnyPlugin>,
        with: PluginNode,
        _main: MainThreadMarker,
    ) -> Result<(), SwapPluginError> {
        if with.session != self.id {
            return Err(SwapPluginError::OtherSession(with));
        }
        let was = match self.graph.plugins.get(&node.erase()) {
            Some(hosted) => ports_of(&hosted.handle),
            None => {
                return Err(SwapPluginError::NoSuchNode(
                    NoSuchNode::new(node.erase()),
                    with,
                ));
            }
        };
        if with.handle.shared.standing.lost() {
            return Err(SwapPluginError::Lost(with));
        }
        // A replacement with other ports would leave the node's edges on ports
        // that are gone and refuse every later commit.
        if with.handle.shared.ports != was {
            let got = Box::new(with.handle.shared.ports.clone());
            return Err(SwapPluginError::Ports {
                got,
                was: Box::new(was),
                node: with,
            });
        }
        if self.graph.is_repreparing() {
            return Err(SwapPluginError::Busy(with));
        }
        let (install, handle) = with.into_parts();
        let Install { plugin, origin, .. } = install;
        match self.swap_installed(node.erase(), plugin, origin) {
            Ok(rebinder) => {
                self.link_node(node, NodeLink::of(handle), rebinder);
                Ok(())
            }
            Err(refused) => {
                let standing = &handle.shared.standing;
                standing.retired.store(true, Ordering::Release);
                standing.release_handle();
                Err(SwapPluginError::Graph(Arc::new(refused)))
            }
        }
    }
}

/// What a node's record keeps of the plugin it is over: the handle the plugin
/// was loaded as, and with it the plugin's standing. Dropped with the node's
/// record, which retires the handle.
pub(crate) struct NodeLink {
    plugin: LoadedPlugin,
}

impl NodeLink {
    fn of(plugin: LoadedPlugin) -> Self {
        Self { plugin }
    }

    fn standing(&self) -> &Standing {
        &self.plugin.shared.standing
    }

    /// Recovery gave up on the plugin.
    pub(crate) fn gave_up(&self, why: GiveUp) {
        *self.standing().gave_up.lock_recover() = Some(why);
    }

    /// The session saw the plugin's bridge dead, for `failure`.
    pub(crate) fn died(&self, failure: PluginFailure) {
        self.standing().dead.lock_recover().get_or_insert(failure);
    }
}

impl Drop for NodeLink {
    fn drop(&mut self) {
        self.standing().hosted.store(false, Ordering::Release);
        self.standing().retired.store(true, Ordering::Release);
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
        link.standing().hosted.store(true, Ordering::Release);
        link.standing().release_handle();
        if let Some(hosted) = self.graph.plugins.get_mut(&node.erase()) {
            hosted.link = Some(link);
            hosted.set_rebinder(rebinder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A bundle with no module this target loads is the load's error, not a
    /// stamp of its directory.** `changed` reads the module, so a directory that
    /// holds none has nothing to stamp.
    #[test]
    fn a_bundle_without_a_module_is_a_module_error() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let bundle = dir.path().join("Empty.clap");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        let error = module_of(&PluginRef::file(&bundle)).expect_err("no module inside");
        assert!(matches!(error, LoadError::Module(_)), "{error:?}");
    }

    /// **A plain file is its own module.**
    #[test]
    fn a_plain_file_is_its_own_module() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let file = dir.path().join("plain.clap");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(module_of(&PluginRef::file(&file)).unwrap(), file);
    }
}
