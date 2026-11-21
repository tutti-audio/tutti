//! The plugin as a value: [`Session::load_plugin`] returns a [`LoadPlugin`], a
//! [`LoadInProcess`] the app runs on the main thread or a [`LoadOutOfProcess`] it
//! awaits on any thread, which opens the plugin and returns the [`LoadedPlugin`]
//! that owns it; [`Session::apply`] puts the loaded plugin in the graph, and
//! [`Session::swap_plugin`] replaces the plugin at a node with one.
//!
//! The id is resolved and bound against the catalog, and the hosting resolved
//! for the format, when the load is built; the load then opens the plugin, the
//! state going in before it is activated. A load holds the session's identity
//! and rate, its collector of main-thread work and a weak handle to its
//! executor, which it upgrades only for the launch of an out-of-process plugin.
//!
//! One opened plugin is one [`LoadedPlugin`], which a session takes once. Its
//! [`PluginInstance`] is the cloneable observer: what the load read of the plugin
//! and whether it is live, or [`Closed`](Presence::Closed) once no plugin is
//! behind it. A plugin not yet applied closes when its [`LoadedPlugin`] is
//! dropped, and an applied one with its node in the graph (an in-process plugin
//! on the main thread, whichever thread drops it; an out-of-process one reaps
//! its server). The session keeps no cache: two loads of one id are two plugins.

use std::fmt;
use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context as TaskContext, Poll};

use futures_channel::oneshot;
use tutti_graph::NodeKey;
use tutti_platform::Asset;
use tutti_plugin::PluginClient;
use tutti_plugin::catalog::ResolvedHosting;
use tutti_plugin_db::{Catalog, Hosting, Loadable};
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{IpcError, Preset, PresetError};
use tutti_plugin_types::{MainThreadMarker, PluginFailure, PluginFormat, PluginId, PluginRef};
use tutti_rt::LockExt;

use super::opened::{Context, Install, PluginCause, PluginInfo, PluginPorts, Shared, Standing};
use super::opened::{plugin_failed, ports_of};
use super::{AnyPlugin, HostPlugin, Open, Origin, PluginLoad};
use crate::apply::{Cut, Sealed as ApplySealed};
use crate::graph::{OnShape, SwapRefused};
use crate::loaded::{Stamp, caught};
use crate::peer::sealed;
use crate::{AppKey, Clock, GiveUp, Key, LoadError, NoSuchNode, Peer, Presence};
use crate::{OtherPeer, PeerState, ReplaceRefused, Resource, ResourceId, SavedPeer, Session};

/// A plugin to open, built by [`Session::load_plugin`] and not yet run. Where it
/// runs is decided when it is built, from the catalog's row and the format: a
/// plugin that runs in the app's process is a [`LoadInProcess`], which only the
/// main thread runs, and one that runs in a `plugin-server` process is a
/// [`LoadOutOfProcess`], which any thread awaits. This enum has no `run`.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::LoadInProcess>();
/// ```
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::LoadOutOfProcess>();
/// ```
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub enum LoadPlugin {
    /// The plugin runs in this process ([`Hosting::InProcess`]).
    InProcess(LoadInProcess),
    /// The plugin runs in a `plugin-server` process ([`Hosting::OutOfProcess`]).
    OutOfProcess(LoadOutOfProcess),
}

/// What both loads hold: the plugin the catalog resolved, the state it starts
/// from, and what the session lends the load.
#[derive(Debug)]
struct Request {
    id: PluginId,
    loadable: Loadable,
    state: Option<Arc<PeerState>>,
    cx: Context,
}

impl Request {
    fn initial_state(&mut self, state: PeerState) -> Result<(), OtherPeer> {
        state.check_for(&SavedPeer::Plugin(self.loadable.plugin().clone()))?;
        self.state = Some(Arc::new(state));
        Ok(())
    }

    /// Takes the module's stamp, opens with `open`, and wraps the result as a
    /// node of the session that built the load. `presets` reads the plugin's
    /// preset list the way its hosting allows.
    fn open(
        self,
        open: impl FnOnce(PluginLoad) -> Open,
        presets: impl FnOnce(&HostPlugin) -> Result<Vec<Preset>, PresetError>,
    ) -> Result<LoadedPlugin, LoadError> {
        let stamp = Stamp::take(Asset::from(module_of(self.loadable.plugin())?));
        let cx = self.cx;
        let request = PluginLoad {
            loadable: self.loadable.clone(),
            rate: cx.rate,
            config: AudioConfig::default(),
            state: self.state,
            work: cx.work.clone(),
            executor: cx.executor.clone(),
        };
        let config = request.config.clone();
        let opened = open(request);
        let (shared, install) =
            Shared::new(&cx, stamp, self.id, opened, self.loadable, config, presets)?;
        Ok(LoadedPlugin {
            instance: PluginInstance {
                shared: Arc::new(shared),
            },
            install: Some(Box::new(install)),
            cut: cx.cut,
        })
    }
}

/// A plugin that runs in this process, not yet opened. Its library loads and
/// its instance is made on the main thread: it is not `Send`, and dropping it
/// unrun cancels it.
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub struct LoadInProcess {
    request: Request,
    main: PhantomData<*const ()>,
}

impl LoadInProcess {
    /// With `state` restored at the load, before the plugin is activated. A
    /// refusal by the plugin is logged, and it loads at its defaults.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another peer than the one the
    /// id resolved to.
    pub fn initial_state(&mut self, state: PeerState) -> Result<(), OtherPeer> {
        self.request.initial_state(state)
    }

    /// Opens the plugin on the main thread `main` proves and restores the
    /// initial state.
    ///
    /// The plugin module's [`Identity`](tutti_platform::Identity) is taken
    /// before the open, so a rewrite during it shows in
    /// [`PluginInstance::changed`].
    ///
    /// # Errors
    ///
    /// [`LoadError::Module`] when its bundle holds no module for this target;
    /// [`LoadError::Plugin`] when the catalog blacklisted the plugin since the
    /// load was built or the plugin does not open; [`LoadError::Panicked`] when
    /// a plugin or the host panicked in the open.
    pub fn run(self, main: MainThreadMarker) -> Result<LoadedPlugin, LoadError> {
        caught(|| {
            self.request.open(
                |load| load.run_in_process(main),
                |plugin| plugin.handle().presets(main),
            )
        })
    }
}

/// A plugin that runs in a `plugin-server` process, not yet started. It is
/// `Send`, and it is a [`Future`] through [`IntoFuture`]: awaiting it launches
/// the server and loads the plugin as a task on the session's executor, and
/// yields the [`LoadedPlugin`]. Nothing of it needs the main thread, so any
/// thread awaits it and the session applies the result on its own. There is no
/// blocking form: the task wakes the awaiting future when the reply lands.
///
/// It holds no strong handle to the session's executor. Dropping it unawaited
/// starts nothing.
#[must_use = "work does nothing until it is awaited"]
#[derive(Debug)]
pub struct LoadOutOfProcess {
    request: Request,
}

impl LoadOutOfProcess {
    /// With `state` restored at the load, before the plugin is activated. A
    /// refusal by the plugin is logged, and it loads at its defaults.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another peer than the one the
    /// id resolved to.
    pub fn initial_state(&mut self, state: PeerState) -> Result<(), OtherPeer> {
        self.request.initial_state(state)
    }
}

impl IntoFuture for LoadOutOfProcess {
    type Output = Result<LoadedPlugin, LoadError>;
    type IntoFuture = LoadingPlugin;

    fn into_future(self) -> LoadingPlugin {
        LoadingPlugin {
            stage: Stage::Unstarted(self.request),
        }
    }
}

/// An out-of-process load being awaited ([`LoadOutOfProcess`]): resolves to
/// the [`LoadedPlugin`] once the plugin's server has launched and loaded it.
///
/// The first poll starts the launch as a task on the session's executor (the
/// session's weak handle, upgraded for the spawn only) and awaits the receiver
/// of a oneshot. **The launch task fills the sender**: it takes the plugin
/// module's [`Identity`](tutti_platform::Identity), opens the plugin, and sends
/// the node or the error when the open ends, which wakes the future. Nothing
/// else drives it and no caller waits on code the app does not run: a poll
/// never blocks, and `Session::update` is not needed. Once the plugin is open the
/// future awaits the read of its parameters, which the server's replies feed, so the
/// node arrives with its parameters read; a plugin that cannot list them in time
/// arrives with the read left to the session, which starts it again.
///
/// Dropped before it resolves, the future drops the receiver. A launch that has
/// not begun then sees the receiver gone and opens nothing; one in flight
/// finishes, finds `send` refused, and drops the node it gets back, which closes
/// the plugin and reaps its server: no server outlives the drop.
///
/// # Errors
///
/// The output is [`LoadError::Module`] when the bundle holds no module for this
/// target; [`LoadError::Plugin`] when the catalog blacklisted the plugin since
/// the load was built, the session is gone
/// ([`IpcError::ShutDown`]), or the plugin does not open;
/// [`IpcError::ShutDown`] too when the launch task ended without an answer
/// (the executor dropped it before it ran: the host is shutting down, and the
/// plugin did not fail), and [`IpcError::Io`] when the executor could not
/// start it; [`LoadError::Panicked`] when a plugin or the host panicked in
/// the open.
///
/// # Panics
///
/// When polled after it resolved.
///
/// [`IpcError::ShutDown`]: tutti_plugin_types::IpcError::ShutDown
/// [`IpcError::Io`]: tutti_plugin_types::IpcError::Io
#[must_use = "a load does nothing unless it is awaited"]
pub struct LoadingPlugin {
    stage: Stage,
}

type Reply = oneshot::Receiver<Result<LoadedPlugin, LoadError>>;

enum Stage {
    Unstarted(Request),
    Running(Reply),
    /// The plugin is open and its node made; its parameters are being read, which the
    /// plugin's replies fill.
    Reading(Option<LoadedPlugin>, Box<tutti_plugin_host::ReadingParams>),
    Finished,
}

impl fmt::Debug for LoadingPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadingPlugin").finish_non_exhaustive()
    }
}

impl LoadingPlugin {
    /// Starts the launch of `request` on the session's executor: the task owns
    /// the sender of the reply this returns the receiver of.
    fn launch(request: Request) -> Result<Reply, LoadError> {
        // Upgraded for the spawn only: the task holds no strong handle.
        let Some(executor) = request.cx.executor.upgrade() else {
            return Err(plugin_failed(IpcError::ShutDown));
        };
        let (reply, receiver) = oneshot::channel();
        let task = executor
            .unblock(move || {
                // A future dropped before the launch began has nobody to answer.
                if reply.is_canceled() {
                    return;
                }
                let node = caught(|| {
                    request.open(PluginLoad::run_out_of_process, |plugin| {
                        plugin
                            .client()
                            .and_then(PluginClient::presets)
                            .ok_or(PresetError::Lost)
                    })
                });
                // Refused when the future was dropped meanwhile: the node comes
                // back and closes here, which reaps its server.
                drop(reply.send(node));
            })
            .map_err(|refused| plugin_failed(IpcError::Io(std::io::Error::from(refused.kind()))))?;
        // The task runs on without the future: its end is the reply.
        task.detach();
        Ok(receiver)
    }
}

impl Future for LoadingPlugin {
    type Output = Result<LoadedPlugin, LoadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if matches!(this.stage, Stage::Unstarted(_)) {
            let Stage::Unstarted(request) = std::mem::replace(&mut this.stage, Stage::Finished)
            else {
                unreachable!("matched above");
            };
            match Self::launch(request) {
                Ok(receiver) => this.stage = Stage::Running(receiver),
                Err(refused) => return Poll::Ready(Err(refused)),
            }
        }
        if let Stage::Running(receiver) = &mut this.stage {
            let answered = std::task::ready!(Pin::new(receiver).poll(cx));
            // A sender dropped unanswered: the executor dropped the launch task
            // before it ran (the open is `caught`, so a panic answers).
            let mut node = match answered.unwrap_or_else(|_| Err(plugin_failed(IpcError::ShutDown)))
            {
                Ok(node) => node,
                Err(error) => {
                    this.stage = Stage::Finished;
                    return Poll::Ready(Err(error));
                }
            };
            // The plugin arrives with its parameters read, as an app's first look finds
            // them: the read is awaited here, not left for the session's updates.
            let Some(read) = node.take_param_read() else {
                this.stage = Stage::Finished;
                return Poll::Ready(Ok(node));
            };
            this.stage = Stage::Reading(Some(node), Box::new(read.into_future()));
        }
        let Stage::Reading(node, reading) = &mut this.stage else {
            panic!("a load future is polled after it resolved");
        };
        let values = std::task::ready!(Pin::new(reading).poll(cx));
        let mut node = node.take().expect("the node waits for its read");
        // A list the plugin did not give leaves the read with the node, for the session's
        // next update to start again.
        node.land_params(values);
        this.stage = Stage::Finished;
        Poll::Ready(Ok(node))
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

/// What an app observes of a loaded plugin: [`LoadedPlugin::instance`]. A clone
/// shares the readings, and holds no plugin: the [`LoadedPlugin`] or its node owns it.
///
/// It carries what the load read of the plugin ([`info`](Peer::info)) and
/// whether the plugin is live ([`presence`](Peer::presence)): [`Closed`](Presence::Closed)
/// once the [`LoadedPlugin`] was dropped unapplied, removed from the graph, or swapped out.
///
/// ```
/// fn send_sync<T: Send + Sync>() {}
/// send_sync::<tutti_session::PluginInstance>();
/// ```
#[derive(Clone, Debug)]
pub struct PluginInstance {
    shared: Arc<Shared>,
}

impl PluginInstance {
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
    /// To reload, compose the load as [`Session::swap_plugin`]'s reload
    /// recipe does.
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

impl sealed::Sealed for PluginInstance {}

impl Resource for PluginInstance {
    fn id(&self) -> ResourceId {
        self.shared.id
    }
}

impl Peer for PluginInstance {
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

/// A plugin loaded and not yet in the graph: a load's output, which
/// [`Session::apply`] and [`Session::swap_plugin`] take. A plugin is in one
/// node, so a `LoadedPlugin` is not `Clone`; applying it gives the node's
/// [`Key`], and a refused apply hands nothing back.
///
/// It is `Send`: a session on any thread takes it, whichever thread opened it.
/// Dropped before it is applied, it closes its plugin (an in-process plugin on
/// the main thread, whichever thread drops it).
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::LoadedPlugin>();
/// ```
#[must_use = "a loaded plugin does nothing until it is applied"]
pub struct LoadedPlugin {
    instance: PluginInstance,
    /// The plugin, until a session takes it. Boxed: an opened plugin is far
    /// larger than the errors that hand a node back.
    install: Option<Box<Install>>,
    /// The session that loaded it, the only one it may be applied to.
    cut: Cut,
}

impl fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl LoadedPlugin {
    /// The parameter read the open left for its caller to await, when it has one.
    fn take_param_read(&mut self) -> Option<tutti_plugin_host::ParamRead> {
        self.install.as_mut()?.mirror.as_mut()?.unread.take()
    }

    /// Lands what that read gave, or keeps the read when the plugin's list was
    /// unavailable.
    fn land_params(&mut self, values: super::ParamValues) {
        let Some(mirror) = self.install.as_mut().and_then(|i| i.mirror.as_mut()) else {
            return;
        };
        match values {
            Ok(snapshot) => mirror.values.land(snapshot),
            Err(read) => mirror.unread = Some(*read),
        }
    }

    /// The observer of this plugin: a clone of it outlives the load.
    pub fn instance(&self) -> PluginInstance {
        self.instance.clone()
    }

    /// The plugin and its instance, the load spent: the caller owns the plugin's
    /// retirement from here.
    fn into_parts(mut self) -> (Install, PluginInstance) {
        let install = self
            .install
            .take()
            .expect("a node holds its plugin until taken");
        (*install, self.instance.clone())
    }
}

impl Drop for LoadedPlugin {
    /// Dropped with its plugin still in it: the plugin closes, and its handle
    /// keeps no server alive.
    fn drop(&mut self) {
        if self.install.is_some() {
            let standing = &self.instance.shared.standing;
            standing.retired.store(true, Ordering::Release);
            standing.release_handle();
        }
    }
}

/// Why [`Session::apply`] of a [`LoadedPlugin`] added no node. The plugin is
/// dropped in each case, which closes it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AddPluginError {
    /// The plugin is lost: its server died before it had a node.
    #[error("the plugin is lost")]
    Lost,
    /// Another session loaded the plugin, at its own rate and with its own
    /// collector of main-thread work.
    #[error("the plugin was loaded by another session")]
    Stale,
}

/// Why [`Session::swap_plugin`] swapped nothing. The node keeps the plugin it
/// has in each case, and the incoming node is handed back in each case but
/// [`Graph`](Self::Graph).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SwapPluginError {
    /// No plugin is hosted at the node.
    #[error("{0}")]
    NoSuchNode(NoSuchNode, LoadedPlugin),
    /// The incoming plugin is lost.
    #[error("the plugin is lost")]
    Lost(LoadedPlugin),
    /// Another session loaded the incoming plugin.
    #[error("the plugin was loaded by another session")]
    Stale(LoadedPlugin),
    /// The incoming plugin has other ports than the node's, which would leave
    /// the node's edges on ports that are gone.
    #[error("the plugin has {got} where its node has {was}")]
    Ports {
        /// The incoming plugin's ports.
        got: Box<PluginPorts>,
        /// The node's.
        was: Box<PluginPorts>,
        /// The incoming node.
        node: LoadedPlugin,
    },
    /// The graph is re-preparing: swap again after an update.
    #[error("the graph is re-preparing; swap again after an update")]
    Busy(LoadedPlugin),
    /// The graph refused the swap; the incoming plugin is closed.
    #[error("the node could not swap to the plugin: {0}")]
    Graph(Arc<ReplaceRefused>),
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// A plugin to open, as a value the app runs: a [`LoadInProcess`] on the
    /// main thread or a [`LoadOutOfProcess`] on any thread, by where the
    /// plugin's format runs by default in this build.
    ///
    /// `id` is resolved and bound against `catalog` now ([`Catalog::resolve`]),
    /// and the load keeps the answer. The load runs at the session's rate and
    /// under the catalog's blacklist, which it asks again when it runs. The app
    /// keeps its catalog. Two loads of one id are two plugins.
    ///
    /// The opened plugin has no node in the graph yet:
    /// [`apply`](Self::apply) puts it there, in this session only.
    ///
    /// # Errors
    ///
    /// [`LoadError::Unresolved`] when `id` names no plugin of the catalog, or
    /// several; [`LoadError::Plugin`] when the plugin is blacklisted now or
    /// this build holds no host for its format
    /// ([`IpcError::NotInProcess`]).
    ///
    /// [`IpcError::NotInProcess`]: tutti_plugin_types::IpcError::NotInProcess
    pub fn load_plugin(&self, id: &PluginId, catalog: &Catalog) -> Result<LoadPlugin, LoadError> {
        self.load_hosted(id, catalog, Hosting::Auto)
    }

    /// [`load_plugin`](Self::load_plugin), run where `hosting` says rather than
    /// where the format runs by default.
    ///
    /// # Errors
    ///
    /// As [`load_plugin`](Self::load_plugin), and [`LoadError::Plugin`] when
    /// the format cannot be hosted as `hosting` ([`IpcError::NotOutOfProcess`]
    /// for VST2, [`IpcError::NotInProcess`] for a format this build holds no
    /// in-process host for).
    ///
    /// [`IpcError::NotOutOfProcess`]: tutti_plugin_types::IpcError::NotOutOfProcess
    /// [`IpcError::NotInProcess`]: tutti_plugin_types::IpcError::NotInProcess
    pub fn load_plugin_as(
        &self,
        id: &PluginId,
        catalog: &Catalog,
        hosting: ResolvedHosting,
    ) -> Result<LoadPlugin, LoadError> {
        self.load_hosted(
            id,
            catalog,
            match hosting {
                ResolvedHosting::InProcess => Hosting::InProcess,
                ResolvedHosting::OutOfProcess => Hosting::OutOfProcess,
            },
        )
    }

    /// The load of `id` as `hosting` asks.
    fn load_hosted(
        &self,
        id: &PluginId,
        catalog: &Catalog,
        hosting: Hosting,
    ) -> Result<LoadPlugin, LoadError> {
        let loadable = catalog
            .resolve(id)
            .bind()
            .map_err(LoadError::Unresolved)?
            .hosting(hosting);
        let placed = loadable
            .check()
            .and_then(|()| tutti_plugin::catalog::placement(&loadable))
            .map_err(plugin_failed)?;
        let request = Request {
            id: id.clone(),
            loadable,
            state: None,
            cx: Context {
                rate: self.graph.prepared().sample_rate(),
                work: self.plugins.work.clone(),
                executor: self.plugins.executor.clone(),
                ids: self.resource_ids.clone(),
                cut: Cut::of(self),
                recovery: self.plugins.recovery.is_some(),
            },
        };
        Ok(match placed {
            ResolvedHosting::InProcess => LoadPlugin::InProcess(LoadInProcess {
                request,
                main: PhantomData,
            }),
            ResolvedHosting::OutOfProcess => LoadPlugin::OutOfProcess(LoadOutOfProcess { request }),
        })
    }

    /// Replaces the plugin at `node` with `with`'s: a crossfade when the shapes
    /// fit the graph's check, else a plain swap, at the same key, so the node's
    /// wiring and lanes stay. The record at the key is carried over to the
    /// incoming plugin with nothing of the outgoing plugin's state, so a node
    /// that was recovering reads healthy again, its way back is the incoming
    /// plugin's kind's (an out-of-process plugin is reloaded when it dies, one
    /// hosted in process is not, whatever the outgoing plugin was), and a host-inserted node has the
    /// loaded kind's death behaviour afterwards: it stays silent at its key
    /// rather than being removed.
    ///
    /// The outgoing plugin is parked in the graveyard and closes at the next
    /// update on the main thread; its handle reads [`Closed`](Presence::Closed).
    /// `main` proves this call is on the main thread, for the inline close of a
    /// refused in-process incoming plugin.
    ///
    /// # Reloading
    ///
    /// A reload is the app's own composition: the load of the plugin's id as it
    /// was hosted, started from the state worth keeping, swapped in at its key.
    /// The inputs are read: [`PluginInfo::id`] and [`PluginInfo::hosting`] from
    /// the node's instance, and [`StateView::withheld`](crate::StateView::withheld)
    /// or [`last_good`](crate::StateView::last_good) from the session's view of
    /// it. A node that gave up is revived the same way.
    ///
    /// ```no_run
    /// # use tutti_plugin_db::Catalog;
    /// # use tutti_plugin_types::MainThreadMarker;
    /// # use tutti_session::{AnyPlugin, Key, LoadPlugin, PluginInstance, Peer, Session};
    /// # async fn go(
    /// #     session: &mut Session,
    /// #     catalog: &Catalog,
    /// #     node: Key<AnyPlugin>,
    /// #     instance: &PluginInstance,
    /// #     main: MainThreadMarker,
    /// # ) -> Result<(), Box<dyn std::error::Error>> {
    /// let info = instance.info();
    /// let state = {
    ///     let view = session.plugin_view(node)?;
    ///     view.state().withheld().or(view.state().last_good()).cloned()
    /// };
    /// let loaded = match session.load_plugin_as(info.id(), catalog, info.hosting())? {
    ///     LoadPlugin::InProcess(mut load) => {
    ///         if let Some(state) = state {
    ///             load.initial_state(state)?;
    ///         }
    ///         load.run(main)?
    ///     }
    ///     LoadPlugin::OutOfProcess(mut load) => {
    ///         if let Some(state) = state {
    ///             load.initial_state(state)?;
    ///         }
    ///         load.await?
    ///     }
    /// };
    /// session.swap_plugin(node, loaded, main)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// The incoming node comes back in the error, and `node` keeps its plugin:
    /// [`SwapPluginError::NoSuchNode`] when no plugin is hosted at `node`;
    /// [`SwapPluginError::Lost`] and [`SwapPluginError::Stale`] for an
    /// incoming plugin that is lost or loaded by another session;
    /// [`SwapPluginError::Ports`] for other audio buses or MIDI output than the
    /// node's; [`SwapPluginError::Busy`] while the graph re-prepares: swap again
    /// after an update. [`SwapPluginError::Graph`] means the graph refused, and
    /// the incoming plugin is closed.
    pub fn swap_plugin(
        &mut self,
        node: Key<AnyPlugin>,
        with: LoadedPlugin,
        _main: MainThreadMarker,
    ) -> Result<(), SwapPluginError> {
        if with.cut != Cut::of(self) {
            return Err(SwapPluginError::Stale(with));
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
        if with.instance.shared.standing.lost() {
            return Err(SwapPluginError::Lost(with));
        }
        // A replacement with other ports would leave the node's edges on ports
        // that are gone and refuse every later commit.
        if with.instance.shared.ports != was {
            let got = Box::new(with.instance.shared.ports.clone());
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

impl ApplySealed<f32> for LoadedPlugin {
    type Applied = Key<AnyPlugin>;
    type Error = AddPluginError;

    fn cut(&self) -> &Cut {
        &self.cut
    }

    fn stale() -> AddPluginError {
        AddPluginError::Stale
    }

    /// Puts the plugin in the graph, unwired. The node's record holds a clone
    /// of the plugin's instance until the record retires, and then the
    /// instance reads [`Closed`](Presence::Closed). It takes no main thread:
    /// the plugin was opened on it, and binding it to a graph node is not
    /// main-thread work, so a session on another thread applies it too.
    ///
    /// A refusal hands nothing back: the load drops, which closes its plugin.
    fn apply_to<K: AppKey, C: Clock<Float = f32>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<Key<AnyPlugin>, AddPluginError> {
        if self.instance.shared.standing.lost() {
            return Err(AddPluginError::Lost);
        }
        let (install, instance) = self.into_parts();
        let Install {
            plugin,
            mirror,
            origin,
        } = install;
        let (key, rebinder) = session.install_plugin(plugin, mirror, origin);
        session.link_node(key, NodeLink::of(instance), rebinder);
        Ok(key)
    }
}

/// What a node's record keeps of the plugin it is over: the instance the plugin
/// was loaded as, and with it the plugin's standing. Dropped with the node's
/// record, which retires the handle.
pub(crate) struct NodeLink {
    plugin: PluginInstance,
}

impl NodeLink {
    fn of(plugin: PluginInstance) -> Self {
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
    ) -> Result<tutti_plugin::catalog::PluginRebinder, ReplaceRefused> {
        let meter = self.transport.settings().project_meter().clone();
        let recovery = self.plugins.recovery.map(crate::Recovery::new);
        match plugin.into_client() {
            Ok(client) => {
                client.controls().set_meter(meter);
                let swapped =
                    self.graph
                        .replace_plugin_with_rebinder(node, client, None, OnShape::Cut);
                if swapped.is_ok()
                    && let Some(hosted) = self.graph.plugins.get_mut(&node)
                {
                    hosted.reloadable = Some(super::Reloadable { origin, recovery });
                }
                swapped.map_err(|refused| match refused {
                    SwapRefused::Failed(refused) => refused,
                    SwapRefused::Busy(_) => unreachable!("the graph is not re-preparing"),
                })
            }
            Err(plugin) => {
                plugin.node_controls().set_meter(meter);
                self.graph
                    .replace_hosted_plugin(node, plugin, None, OnShape::Cut)
                    .map_err(|refused| match refused {
                        SwapRefused::Failed(refused) => refused,
                        SwapRefused::Busy(_) => unreachable!("the graph is not re-preparing"),
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

    /// **A launch task dropped before it answers resolves as a shut-down
    /// host**, never a wait without end: the reply's sender is gone.
    #[test]
    fn a_dropped_sender_resolves_as_a_shut_down_host() {
        let (reply, receiver) = oneshot::channel();
        drop(reply);
        let load = LoadingPlugin {
            stage: Stage::Running(receiver),
        };
        let error = tutti_fixtures::block_on(load).expect_err("nobody answered");
        assert!(
            matches!(error, LoadError::Plugin(ref failed) if matches!(**failed, IpcError::ShutDown)),
            "{error:?}"
        );
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
