//! Hosted plugins in the session (feature `plugin`): the load that opens one
//! ([`Session::load_plugin`]), the apply that puts it in the graph
//! ([`Session::apply`]), the swap that replaces the plugin at a node
//! ([`Session::swap_plugin`]), the view of the record the session keeps for each
//! one ([`Plugin`]), and the work of [`Phase::Plugins`](crate::Phase::Plugins) and
//! [`Phase::RenderMode`](crate::Phase::RenderMode).
//!
//! # The load
//!
//! A plugin is a value the app runs ([`LoadPlugin`]): an in-process plugin on the
//! main thread, an out-of-process one on any thread. It returns a [`LoadedPlugin`], which has a refcounted [`PluginInstance`]; [`Session::apply`] puts
//! its node in the graph through the one install, and the node's record holds a
//! clone of the instance until the record retires.
//!
//! # The record
//!
//! Each plugin the graph holds (`Graph::add_plugin`,
//! [`Session::apply`]) has a record,
//! keyed by its [`NodeKey`] and lent as a [`Plugin`] ([`Session::plugin_view`]): the [`PluginRef`] it is, its handle, its
//! controls (an out-of-process plugin's latency and meter cells), its
//! liveness, whether it was told the render mode, its last good state
//! ([`state`](Plugin::state)), and its parameter mirror
//! ([`params`](Plugin::params)). A replacement by another plugin (`Graph::replace_plugin`,
//! `Session::swap_plugin`) carries the record over to the incoming plugin, with
//! nothing of the outgoing plugin's state (its last good state, an in-flight
//! snapshot task, its mirror); replacing it with a native node
//! (`Graph::replace`, `crossfade`) or removing the node drops it.
//!
//! # The main thread
//!
//! An in-process plugin's handle (`PluginHandle`, VST2 today) works from any
//! thread, which is what the snapshot and parameter-read tasks below need: a
//! call off the main thread is posted to the session's
//! [`MainWork`](tutti_plugin_types::MainWork) and the caller waits, for at most two
//! seconds, for the main thread's next tick to run it
//! ([`Session::update`], called on the process's main thread, drains it). A host that does not
//! call it there
//! has those calls answer "unavailable" at the deadline (`StateError::Unavailable`, `None`,
//! `false`).
//!
//! # Plugin tasks
//!
//! The loops below (a reload, a parameter read, a snapshot) each run as a task on the
//! session's executor. A reload and a snapshot run a blocking round trip on its blocking pool
//! (32 threads, shared with other blocking work: a task may wait in the pool's queue); a
//! parameter read is a future the plugin's replies wake, and holds no thread while it
//! waits. [`Session::plugin_tasks`] counts the
//! ones queued or running. A task holds clones of its plugin's handle, so
//! dropping or [closing](Session::close) a session waits for them first: each is bounded by
//! its round trip's IPC deadline (the `Timeouts` load for a reload, param for a read, state
//! for a snapshot), a close on the main thread runs the work they hand it while it waits,
//! and past the bound the session warns and goes on without them. A close from the main
//! thread therefore leaves no in-process plugin's close stranded unless a task outlives its
//! deadline.
//!
//! What retires an in-process record (a removal, a crossfade to another plugin
//! or to a native node, a reload's dead plugin) parks it in the graveyard
//! instead of dropping it, since the drop of a plugin's last owner tears down
//! its library and instance. `Session::update` on the main thread empties it;
//! [`Session::retired_plugins`] counts what waits, and `Phase::Plugins` warns
//! once when more than a handful do. An out-of-process plugin's record drops where it
//! is retired, on the control thread; only an editor it left open waits for
//! the main thread, to be closed, through the registry of open editors
//! (`Editors`).
//!
//! # `Phase::Plugins`
//!
//! Per plugin, in node order:
//!
//! - **Health.** `tutti_plugin_host::Liveness` observes the handle's status at
//!   `PhaseCx::now`: dead after at least `Liveness::DEATHS` consecutive
//!   unhealthy observations spanning at least the window. A dead loaded
//!   plugin's node stays at its key, silent in its place, and its handle reads
//!   lost; a node of a plugin the host inserted itself (`Graph::add_plugin`)
//!   is removed ([`Session::remove`]). Either is reported once,
//!   [`PluginChange::Died`].
//! - **Reports.** What the plugin reported since the last update (its own edit
//!   of a parameter, a rescan, a new tail, a bus change) is queued where it
//!   was heard and becomes [`PluginEvent`]s here, in the order the plugin made
//!   them.
//! - **Latency.** The figure its node declares now
//!   (`PluginControls::declared_latency`) is handed to the graph when it
//!   moved (`Graph::refresh_node_latency`):
//!   [`PluginChange::LatencyChanged`].
//! - **Parameters.** The values the plugin reported (or the host set) go into
//!   its mirror, and (`modulation`) into the knob of the lane on that
//!   parameter when it has one. A mirror rebuilt by a crossfade
//!   to another plugin, or by a rescan the plugin announced, is read by a task
//!   on the session's executor: one round trip
//!   per parameter, landed through the plugin inbox in a later [`Session::update`]. Until then the
//!   mirror is not ready, and a lane on the plugin waits for it.
//! - **Snapshots.** A healthy plugin's state is captured every
//!   `snapshot_interval` (8 s by default, set on the `SessionBuilder`) of
//!   `now`, by a task on the same executor (a blocking round trip), landed through
//!   the plugin inbox in a later update. One is in flight per plugin at most: the
//!   task holds the plugin's snapshot token until its end lands, whether it ran,
//!   panicked or was dropped.
//!
//! # Hang
//!
//! With or without recovery, `PluginHandle::block_counts` is observed each
//! phase against the session's [`HangPolicy`](crate::HangPolicy); a verdict
//! ends the server (`PluginHandle::terminate`), so a hung plugin dies like a
//! crashed one: [`PluginChange::Died`] with recovery off, a reload with it on.
//!
//! # Recovery
//!
//! With a [`RecoveryPolicy`](crate::RecoveryPolicy) (the default; `SessionBuilder::plugin_recovery`),
//! an out-of-process plugin that dies, or hangs (stops answering blocks), is
//! reloaded at its own node instead of removed. The policy is the pure
//! [`Recovery`]; this module carries out what it decides:
//!
//! - **Death.** The node stays in the graph, silent, with its wiring and
//!   every lane fed to it; [`PluginChange::Reloading`].
//! - **Load.** When the backoff passes, the existing `PluginLoad` (with
//!   the last good state as its state) runs as a task on the executor.
//!   Its end lands through the plugin inbox in a later `Session::update`, on whatever thread runs it
//!   (no main-thread proof: an out-of-process replacement opens off the main thread,
//!   and one that came back unopened for the main thread is a failed attempt),
//!   swaps the loaded plugin in at the same node (`Graph::replace_plugin`)
//!   and keeps the record: the last good state, its age, the failure
//!   history. A task the executor drops is [`RecoveryFailure::ReloadDropped`],
//!   one that panics [`RecoveryFailure::ReloadPanicked`]: a failed attempt.
//! - **Reported.** [`PluginChange::Recovered`] says whether the state
//!   came back and how old it was; a spent budget is
//!   [`PluginChange::GaveUp`], and the node is silenced (a loaded plugin's) or
//!   removed (a host-inserted one).
//!
//! An in-process plugin (VST2) is not recovered: a crash in it takes the
//! process, and a hang in it blocks the audio thread itself, which nothing
//! here observes (`block_counts` is zeros for it).
//!
//! # `Phase::RenderMode` (with `export`)
//!
//! Offline while an export token lives, Realtime otherwise; each
//! plugin is told per `tutti_plugin_host::needs_notify`: on a change of
//! mode, or if it was never told.

use std::future::{Future as _, IntoFuture as _};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use std::time::Duration;

use tutti_platform::Instant;
use tutti_plugin_types::{MainThreadMarker, StateUse};
use tutti_rt::Mutex;

use tutti_graph::NodeKey;
use tutti_plugin::catalog::Plugin as HostPlugin;
use tutti_plugin::catalog::ResolvedHosting;
use tutti_plugin_db::Loadable;
use tutti_plugin_host::{Liveness, LivenessVerdict};
use tutti_plugin_host::{
    NodeControls, PluginControls, PluginHandle, PluginSignal, PluginStatus, Subscription,
};
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::PluginRef;
use tutti_plugin_types::StateError;
use tutti_plugin_types::{
    IpcError, ParamEdit, PluginFailure, PluginInvalidation, PluginRefresh, PluginTail, RenderMode,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

mod awaiting;
#[cfg(test)]
mod fake_hosting;
mod inbox;
mod load;
mod lost;
mod op;
mod opened;
mod rebind;
mod scan;
pub use rebind::PortsRefusal;
mod preset;
mod state;
pub(crate) use lost::Lost;

use load::NodeLink;
pub use load::{
    AddPluginError, LoadInProcess, LoadOutOfProcess, LoadPlugin, LoadedPlugin, LoadingPlugin,
    PluginInstance, SwapPluginError,
};
pub use op::{AwaitingOp, InProcessOp, Op, OutOfProcessOp, PluginOp};
pub use opened::{PluginCause, PluginInfo, PluginPorts};
pub use preset::{
    LoadPreset, LoadPresetInProcess, LoadPresetOp, LoadPresetOutOfProcess, LoadedPreset,
    PresetStartError,
};
pub use state::{
    PluginApplyError, RestoreState, RestoreStateInProcess, RestoreStateOp,
    RestoreStateOutOfProcess, RestoredState, SaveState, SaveStateInProcess, SaveStateOp,
    SaveStateOutOfProcess, Saved, StateStartError,
};

use crate::plugin_diagnostics::Figures;
use crate::session::parts::Loop;
use crate::{
    Diagnostics, GiveUp, HangDetector, Key, PeerState, PluginBuses, PluginParams, PortCounts,
    Recovery, RecoveryAction, RecoveryEvent, RecoveryFailure, ReloadCause, ReplaceRefused, Session,
    SessionEvent,
};
pub(crate) use inbox::Inbox;
use inbox::{Arrival, contained};

/// How often a healthy plugin's state is captured, by default: 8 s.
/// [`SessionBuilder::snapshot_interval`](crate::SessionBuilder::snapshot_interval)
/// sets another.
pub(crate) const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(8);

/// A plugin load: what a [`LoadPlugin`]'s `run` runs, and what a recovery reload
/// runs again on a task. [`run`](Self::run) and
/// [`run_out_of_process`](Self::run_out_of_process) block (a `plugin-server`
/// launch, half a second to fifteen).
#[must_use = "a load does nothing until it is run"]
pub(crate) struct PluginLoad {
    /// The plugin, under the blacklist of the catalog that resolved it, asked
    /// by [`run`](Self::run) as the load opens.
    loadable: Loadable,
    rate: SampleRate,
    config: AudioConfig,
    state: Option<Arc<Saved>>,
    /// The session's collector of main-thread work, which the plugin's handles hold.
    work: tutti_plugin_types::MainWorkHandle,
    /// The session's executor, which an out-of-process plugin's exit watch runs on. The
    /// load upgrades it as it opens an out-of-process plugin and holds it only while it
    /// does, so a queued load never keeps the executor alive.
    executor: tutti_platform::WeakExecutor,
}

impl std::fmt::Debug for PluginLoad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginLoad")
            .field("plugin", self.loadable.plugin())
            .field("rate", &self.rate)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PluginLoad {
    /// Opens an in-process plugin on the main thread `main` proves, restores the
    /// request's state, and reads its parameter values, after the state: those
    /// are what a modulation lane's knob starts at. A library load and an
    /// instance's init, with no subprocess to wait for.
    ///
    /// A plugin the catalog blacklisted is refused here, before anything is
    /// opened.
    fn run_in_process(self, main: MainThreadMarker) -> Open {
        match self.loadable.check() {
            Err(refusal) => Open::refused(refusal),
            Ok(()) => self.open(Opening::OnMain(main)),
        }
    }

    /// Opens an out-of-process plugin: launches its `plugin-server`, restores the
    /// request's state, and reads its parameter values, as
    /// [`run_in_process`](Self::run_in_process) does. Any thread; it blocks
    /// (half a second to fifteen).
    fn run_out_of_process(self) -> Open {
        match self.loadable.check() {
            Err(refusal) => Open::refused(refusal),
            Ok(()) => self.open(Opening::OutOfProcess),
        }
    }

    /// [`run_in_process`](Self::run_in_process) for a load that runs where no main thread is
    /// proved (a recovery reload's task): an out-of-process plugin opens, and one that
    /// resolves in process is not opened ([`Opened::InProcess`]), since it is
    /// confined to the main thread. Any thread; it blocks.
    pub(crate) fn run(self) -> PluginLoaded {
        let opened = match self
            .loadable
            .check()
            .and_then(|()| tutti_plugin::catalog::placement(&self.loadable))
        {
            Err(refusal) => Opened::Done(Box::new(Open::refused(refusal))),
            Ok(ResolvedHosting::InProcess) => Opened::InProcess,
            Ok(ResolvedHosting::OutOfProcess) => {
                Opened::Done(Box::new(self.open(Opening::OutOfProcess)))
            }
        };
        PluginLoaded { opened }
    }

    /// The session's executor while it lives: a load that runs once the
    /// session is dropping is refused.
    ///
    /// The open is the spawn: an out-of-process plugin's exit watch is spawned inside it, so the
    /// strong handle lives only for the launch, which the load deadline bounds
    /// (`Timeouts::load`), and drops when the open returns. A session's drop waits for a
    /// reload task inside the open (see the module docs), so the executor outlives the
    /// launch; if the last other handle goes first the open still ends, and the server's
    /// `ProcessGuard` reaps the child when the plugin or the load that owns it drops.
    fn executor(&self) -> Result<tutti_platform::Executor, IpcError> {
        self.executor.upgrade().ok_or(IpcError::ShutDown)
    }

    /// [`run`](Self::run)'s open, on this thread.
    fn open(self, at: Opening) -> Open {
        // The state goes in **before the plugin is activated** (`open_with_state`):
        // a plugin may take its mode from the state at activation. A refusal
        // does not abort the load: a plugin at its defaults beats no plugin. But
        // it is not silent.
        let state = self.state.as_deref().map(|saved| saved.state().bytes());
        let opened = match (at, state) {
            (Opening::OutOfProcess, None) => self.executor().and_then(|executor| {
                HostPlugin::open_served(
                    &self.config,
                    &executor,
                    &self.work,
                    &self.loadable,
                    self.rate,
                )
                .map(|plugin| (plugin, None))
            }),
            (Opening::OutOfProcess, Some(state)) => self.executor().and_then(|executor| {
                HostPlugin::open_served_restoring(
                    &self.config,
                    &executor,
                    &self.work,
                    &self.loadable,
                    self.rate,
                    state,
                )
                .map(|(plugin, restore)| (plugin, Some(restore)))
            }),
            (Opening::OnMain(main), None) => {
                HostPlugin::open_in_process(main, &self.work, &self.loadable, self.rate)
                    .map(|plugin| (plugin, None))
            }
            (Opening::OnMain(main), Some(state)) => HostPlugin::open_in_process_restoring(
                main,
                &self.work,
                &self.loadable,
                self.rate,
                state,
            )
            .map(|(plugin, restore)| (plugin, Some(restore))),
        };
        let (plugin, restore) = match opened {
            Ok((plugin, restore)) => (Ok(plugin), restore),
            // The server died, or stopped answering, inside the step that
            // restores the state it was staged (`StateRestore`; a failure
            // before that step is a load failure and blames nothing): the state
            // is what the load is held against (the reload's landing withholds it).
            Err(tutti_plugin_types::IpcError::StateRestore { cause }) if self.state.is_some() => {
                let refusal = restore_ended(&cause);
                (
                    Err(tutti_plugin_types::IpcError::StateRestore { cause }),
                    Some(Err(refusal)),
                )
            }
            Err(e) => (Err(e), None),
        };
        // The writes the state did not hold, once the plugin took it: replayed
        // once the plugin is a record's, through its ledger.
        let replay = match (&plugin, &restore, &self.state) {
            (Ok(_), Some(Ok(())), Some(saved)) => saved.pending().to_vec(),
            _ => Vec::new(),
        };
        if let (Ok(plugin), Some(Err(e))) = (&plugin, &restore) {
            tracing::warn!(
                "{}: saved state was not restored ({e}); the plugin is loaded at its \
                 defaults",
                plugin.handle().name()
            );
        }
        let mirror = plugin.as_ref().ok().map(|plugin| {
            let (mut values, read) = tutti_plugin_host::PluginParamValues::start(plugin.handle());
            // An in-process plugin on its main thread answers at once, so its read lands
            // here, after the state. An out-of-process plugin's answers come from its
            // bridge's threads: its read goes to the hosted record, to be handed out as
            // a task. So does a read the plugin did not answer (its list unavailable),
            // which leaves the mirror not ready.
            let unread = match at {
                Opening::OnMain(_) => {
                    let mut reading = std::pin::pin!(read.into_future());
                    let cx = &mut Context::from_waker(Waker::noop());
                    match reading.as_mut().poll(cx) {
                        Poll::Ready(Ok(snapshot)) => {
                            values.land(snapshot);
                            None
                        }
                        Poll::Ready(Err(read)) => Some(*read),
                        Poll::Pending => Some(tutti_plugin_host::ParamRead::of(plugin.handle())),
                    }
                }
                Opening::OutOfProcess => Some(read),
            };
            Mirror { values, unread }
        });
        Open {
            plugin,
            restore,
            mirror,
            replay,
        }
    }
}

/// A new plugin's parameter mirror, and the read that still has to fill it
/// when the load-time read was not answered.
struct Mirror {
    values: tutti_plugin_host::PluginParamValues,
    /// Becomes the hosted record's `param_read`, to be run as a task.
    unread: Option<tutti_plugin_host::ParamRead>,
}

/// What the `cause` of a bridge failure inside a state restore says of the
/// state: the server did not answer within its window, or the plugin is gone.
/// Every other cause (a disconnect, a protocol violation) is `PluginCrashed`:
/// a bridge failure mid-launch ends the server, so the plugin is gone.
fn restore_ended(cause: &tutti_plugin_types::IpcError) -> StateError {
    match *cause {
        tutti_plugin_types::IpcError::Timeout { after, .. } => StateError::Stalled { after },
        _ => StateError::PluginCrashed,
    }
}

/// An opened plugin (or why it did not open) and its parameter mirror.
struct Open {
    plugin: Result<HostPlugin, IpcError>,
    /// The state restore, if one was asked for, and the refusal when it was refused.
    restore: Option<Result<(), StateError>>,
    mirror: Option<Mirror>,
    /// The writes the restored state did not hold, to replay through the
    /// record's ledger once the plugin is one's.
    replay: Vec<(tutti_units::ParamId, tutti_units::Normalized)>,
}

impl Open {
    /// A load that did not open: `why`.
    fn refused(why: IpcError) -> Self {
        Self {
            plugin: Err(why),
            restore: None,
            mirror: None,
            replay: Vec::new(),
        }
    }
}

/// What a parameter read gave: the values, or the read itself back to be awaited
/// again (the plugin's list was unavailable).
pub(crate) type ParamValues =
    Result<tutti_plugin_host::ParamSnapshot, Box<tutti_plugin_host::ParamRead>>;

/// Where a load is opened: on the calling thread, or on the main thread an
/// in-process plugin is confined to.
#[derive(Clone, Copy)]
enum Opening {
    OutOfProcess,
    OnMain(MainThreadMarker),
}

/// What a `PluginLoad`'s [`run`](PluginLoad::run) did.
enum Opened {
    /// Opened (or failed to) on the task's thread. Boxed: a plugin is far
    /// larger than unopened load.
    Done(Box<Open>),
    /// Resolves in process, which only the main thread opens: nothing was opened.
    InProcess,
}

/// What became of the state a reload was to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateRestore {
    /// The plugin took the snapshot, captured `age` before the failure was
    /// handled. Anything it changed since is lost.
    Restored {
        /// How old the snapshot was when the replacement loaded.
        age: Duration,
    },
    /// The plugin refused the snapshot and loads at its defaults.
    Refused {
        /// How old the snapshot was.
        age: Duration,
        /// Why the snapshot was refused.
        error: StateError,
    },
    /// No snapshot had been captured yet: the replacement loads at its
    /// defaults.
    NoSnapshot,
    /// The snapshot was left out: the plugin died within
    /// [`RecoveryPolicy::state_probation`](crate::RecoveryPolicy::state_probation) of loading it last time (or while
    /// loading it), so it is taken for the cause. The replacement loads at
    /// its defaults, and the snapshot is gone from the record.
    Withheld {
        /// How old the snapshot was.
        age: Duration,
    },
}

/// A finished `PluginLoad`: the plugin, or why it did not load. `Send`.
#[must_use = "a loaded plugin goes into the graph"]
pub(crate) struct PluginLoaded {
    opened: Opened,
}

impl std::fmt::Debug for PluginLoaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginLoaded").finish_non_exhaustive()
    }
}

/// What the session believes about a plugin's liveness: the debounce
/// between the engine's latched `PluginStatus` and a node removed. The public
/// reading is [`PluginHealth`] ([`Plugin::health`]).
#[derive(Debug, Clone, Default)]
pub(crate) enum PluginLiveness {
    /// Answering normally.
    #[default]
    Healthy,
    /// Recently unhealthy, not yet written off (`tutti_plugin_host::Liveness`).
    Failing,
    /// Dead or hung, and being replaced at its node (recovery on): the node
    /// stays, silent, until [`PluginChange::Recovered`] or
    /// [`PluginChange::GaveUp`].
    Recovering {
        /// Why it needs a replacement.
        cause: ReloadCause,
    },
    /// Written off (recovery off, or its budget spent): a loaded plugin's node
    /// stays at its key, silent; a host-inserted plugin's node is being
    /// removed.
    Dead {
        /// The engine's latched reason.
        cause: PluginFailure,
    },
}

/// How a hosted plugin is doing ([`Plugin::health`]):
/// one reading of what the engine latched, the session's debounce and its
/// recovery.
///
/// The verdicts that remove a host-inserted plugin's node (written off with
/// recovery off, or recovery's budget spent) are not here: the node and its
/// view are gone in the update that reaches them, and the events say so
/// ([`PluginChange::Died`], [`PluginChange::GaveUp`]). A loaded plugin's node
/// stays at its key, silent, and reads [`Died`](Self::Died) for good.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PluginHealth {
    /// Answering normally.
    Running,
    /// Alive, but its blocks go unanswered: the bridge is abandoning them and
    /// has not yet declared a hang ([`HangPolicy`](crate::HangPolicy)), with
    /// recovery on or off.
    Stalled,
    /// The engine reports it dead, and the session has not yet written it off
    /// (it waits out a debounce first). It then becomes
    /// [`Reloading`](Self::Reloading), or, with recovery off, a loaded
    /// plugin's node stays at its key, silent, and a host-inserted one's is
    /// removed.
    Died {
        /// The engine's latched reason.
        cause: PluginFailure,
    },
    /// Dead or hung, and a replacement is on its way to the same node: the node
    /// stays, silent.
    Reloading {
        /// Failures within the recovery budget's window so far, this one
        /// included.
        attempt: u32,
        /// Why it needs a replacement.
        cause: ReloadCause,
    },
}

/// What happened to a hosted plugin, as the session's
/// [`SessionEvent::Plugin`] reports it: the node, and what changed.
///
/// One stream carries both what a plugin reports (its own edit of a
/// parameter, a new tail) and every transition of its life (death, reload,
/// recovery), so a host reads one place. There is no callback slot to install
/// or clear; a host that needs a push from the thread that hears the plugin
/// subscribes to its handle (`PluginHandle::subscribe`, the low layer).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PluginEvent {
    /// The plugin's node. After a [`PluginChange::Died`] with recovery off, or
    /// a [`PluginChange::GaveUp`], it is no longer in the graph.
    pub node: NodeKey,
    /// What happened.
    pub change: PluginChange,
}

/// Whether the node of a plugin that ended renders silence in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Silence {
    /// Not yet: the plugin is running, or the graph was re-preparing and is
    /// asked again at the next phase.
    Pending,
    /// The graph took the silent stand-in.
    Silenced,
    /// The graph refused it ([`PluginChange::NodeNotSilenced`]) and is not asked
    /// again.
    Refused,
}

/// What changed in a hosted plugin ([`PluginEvent::change`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PluginChange {
    /// The plugin took hold of a parameter (its editor, or its own gesture):
    /// the start of an edit that ends with [`GestureEnded`](Self::GestureEnded).
    GestureBegan {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
    },
    /// The plugin moved one of its own parameters (its editor, a preset load,
    /// its own automation), inside a gesture or not. Every one is reported, in
    /// the order the plugin made them.
    ParamChanged {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
        /// Its normalized value.
        value: Normalized,
        /// Whether a host that records automation should record this edit
        /// (`false` for a CLAP value the plugin flagged `DONT_RECORD`).
        recorded: bool,
    },
    /// The plugin let go of a parameter.
    GestureEnded {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
    },
    /// The plugin's parameter values changed wholesale (a preset load, an
    /// internal write-back): re-read them.
    ParamsRescanned,
    /// The plugin's parameter list or its titles, units or flags changed:
    /// re-read the descriptors.
    ParamTitlesChanged,
    /// The plugin re-declared its controller table, or was rebuilt in place:
    /// [`Plugin::assignments`] reads the new one.
    MidiMapChanged,
    /// Reading the plugin's parameters panicked, once: the read is not made
    /// again, and the parameters stay unread
    /// ([`PluginParams::is_ready`](crate::PluginParams::is_ready) is `false`)
    /// until the plugin's own rescan ([`ParamsRescanned`](Self::ParamsRescanned))
    /// starts a read afresh.
    ParamsUnread,
    /// The plugin's declared latency moved, and the graph took it: the next
    /// commit compensates it.
    LatencyChanged {
        /// Its latency now.
        latency: Samples,
    },
    /// The plugin reported a new tail. The node's own cell is already
    /// updated; a bounce already in flight keeps the figure it started with.
    TailChanged {
        /// The new tail, which may be bounded, unbounded or unreported.
        tail: PluginTail,
    },
    /// The plugin's bus layout changed. An in-process plugin's node is
    /// re-inserted at its key with the new ports by the session
    /// ([`SessionEvent::Unwired`] names the wiring a narrower shape dropped);
    /// another plugin's node keeps its ports, and the app re-reads and rewires.
    /// The re-inserted plugin renders silence from the swap until its
    /// Processor reaches the new node: the old node returns it at its next block
    /// and the next update's pump lends it on. A node that is
    /// [`Bypassed`](crate::NodeMode::Bypassed) is not processed, but it still
    /// answers the recall once per block, so the change completes while the
    /// bypass lasts.
    IoChanged,
    /// The plugin was rebuilt in place and did not take the state it was saved
    /// with: it is at its defaults.
    StateNotRestored {
        /// Why the state was not restored.
        reason: String,
    },
    /// A parameter write the plugin refused because it no longer declares the
    /// parameter (a rescan dropped it after the session read its list). The
    /// write is given up: sending it again would repeat the refusal. Reported
    /// once per write.
    ParamUndeclared {
        /// The parameter, by the id the write named.
        id: ParamId,
    },
    /// A parameter write the plugin refused because it declares the parameter
    /// read-only. The write is given up: sending it again would repeat the
    /// refusal. Reported once per write.
    ParamReadOnly {
        /// The parameter, by the id the write named.
        id: ParamId,
    },
    /// The plugin was prepared off the main thread, where a render mode change
    /// cannot run: it keeps the mode it runs in, and the next prepare on the
    /// main thread applies it. Not a refusal by the plugin.
    RenderModeNotApplied {
        /// The mode the prepare carried.
        mode: RenderMode,
    },
    /// The silent stand-in for a plugin that ended could not be put at its node:
    /// the graph refused ([`ReplaceRefused`]: a poisoned graph, or a node that is
    /// gone). The node stays as it was and is not asked again. For a poisoned
    /// graph the app removes or replaces it; a node that is gone needs nothing.
    /// Reported once, after the plugin's
    /// [`Died`](Self::Died) or [`GaveUp`](Self::GaveUp).
    NodeNotSilenced {
        /// Why the graph refused.
        why: ReplaceRefused,
    },
    /// The plugin's new ports were not made a node, and the node stays as it
    /// is: they are wider than a block serves, or the graph refused the swap
    /// ([`PortsRefusal`]). Reported once for the ports: they count as handled.
    PortsRefused {
        /// Why the ports were refused.
        why: PortsRefusal,
    },
    /// A hosted plugin was declared dead
    /// (`tutti_plugin_host::Liveness`) with recovery off: a loaded plugin's
    /// node stays at its key, silent, and a host-inserted plugin's node was
    /// removed. Reported once. With recovery on the death is
    /// [`Reloading`](Self::Reloading), and the node's end
    /// [`GaveUp`](Self::GaveUp).
    Died {
        /// The engine's latched reason.
        cause: PluginFailure,
    },
    /// A hosted plugin died or hung and recovery is on
    /// ([`RecoveryPolicy`](crate::RecoveryPolicy)): its node stays in the
    /// graph, silent, and a replacement loads after `delay`. Reported once per
    /// attempt.
    Reloading {
        /// Why it needs a replacement: the engine's latched cause, or a failed
        /// attempt's.
        cause: ReloadCause,
        /// Failures within the budget's window so far, this one included.
        attempt: u32,
        /// The wait before the load is handed out.
        delay: Duration,
    },
    /// A replacement is in the graph at the same node, with the same wiring.
    Recovered {
        /// Whether its state came back, and from how long before.
        state: StateRestore,
        /// The node's latency before the failure. PDC follows at the next
        /// commit.
        latency_before: Samples,
        /// The node's latency now.
        latency_after: Samples,
        /// Attempts it took.
        attempts: u32,
    },
    /// Recovery stopped: the failures spent the budget. A loaded plugin's node
    /// stays at its key, silent; a host-inserted plugin's node is removed.
    GaveUp {
        /// Why.
        why: GiveUp,
        /// The last state worth keeping, for a host that offers the user a
        /// manual reload: the state a reload withheld, else the last good one.
        state: Option<Box<Saved>>,
    },
}

/// `change` at `node`, as the session's event.
pub(crate) fn event(node: NodeKey, change: PluginChange) -> SessionEvent {
    SessionEvent::Plugin(PluginEvent { node, change })
}

/// What a plugin reported, waiting for the next `Phase::Plugins`.
pub(crate) type Signals = Arc<Mutex<Vec<PluginSignal>>>;

/// Queues `notice` as if the plugin had reported it.
pub(crate) fn queue(notices: &Signals, notice: PluginSignal) {
    notices.lock().push(notice);
}

/// A queue for what `handle` reports and the subscription that fills it.
fn hear(handle: &PluginHandle) -> (Signals, Subscription) {
    let notices = Signals::default();
    let queue = Arc::clone(&notices);
    let hearing = handle.subscribe(move |notice| queue.lock().push(notice));
    (notices, hearing)
}

/// What `notice` is as an event, if it is one: a latency the plugin reports
/// is reported once, by the update that hands it to the graph; a rate refusal,
/// a crash and a reload are the plugin's health, read as a status. A reload
/// that left the plugin at its defaults is [`PluginChange::StateNotRestored`],
/// and a render mode a prepare off the main thread could not apply has no
/// status to read: both are events.
fn change_of(notice: PluginSignal) -> Option<PluginChange> {
    Some(match notice {
        PluginSignal::Param(ParamEdit::Begin(id)) => PluginChange::GestureBegan { id },
        PluginSignal::Param(ParamEdit::End(id)) => PluginChange::GestureEnded { id },
        PluginSignal::Param(ParamEdit::Change { id, value, record }) => {
            PluginChange::ParamChanged {
                id,
                value,
                recorded: record,
            }
        }
        PluginSignal::Refresh(PluginRefresh::ParamValues) => PluginChange::ParamsRescanned,
        PluginSignal::Refresh(PluginRefresh::ParamTitles) => PluginChange::ParamTitlesChanged,
        PluginSignal::Invalidate(PluginInvalidation::Tail { tail }) => {
            PluginChange::TailChanged { tail }
        }
        PluginSignal::Invalidate(PluginInvalidation::Io) => PluginChange::IoChanged,
        PluginSignal::Invalidate(PluginInvalidation::MidiMap) => PluginChange::MidiMapChanged,
        PluginSignal::Invalidate(PluginInvalidation::StateNotRestored { reason }) => {
            PluginChange::StateNotRestored { reason }
        }
        PluginSignal::Invalidate(PluginInvalidation::RenderModeNotApplied { mode }) => {
            PluginChange::RenderModeNotApplied { mode }
        }
        PluginSignal::Invalidate(_) | _ => return None,
    })
}

/// Where a hosted plugin runs, which says where its record may drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// In this process: the record owns the plugin's library and instance,
    /// whose drop is a main-thread call.
    InProcess,
    /// In a plugin server: the record owns an IPC client and its bridge,
    /// which drop on any thread but the audio thread.
    Served,
}

/// A plugin handle a swap replaced.
pub(crate) struct Outgoing {
    handle: PluginHandle,
    placement: Placement,
}

impl Outgoing {
    /// The handle, split by where its plugin ran. An out-of-process plugin's handle
    /// drops here, on the calling (control) thread.
    pub(crate) fn retire(self) -> Retired {
        let Self { handle, placement } = self;
        Retired::of(placement, handle)
    }
}

/// What a plugin record or handle hands to the session once it is retired.
#[must_use = "a retired record goes to the graveyard or drops, through `settle`"]
pub(crate) enum Retired {
    /// An in-process plugin's: its drop waits for the main thread.
    InProcess(InProcessRecord),
    /// An out-of-process plugin's, already dropped.
    Served,
}

impl Retired {
    /// `held` split by `placement`: an in-process owner is kept for the main
    /// thread, an out-of-process one drops here.
    fn of(placement: Placement, held: impl std::any::Any + Send) -> Self {
        match placement {
            Placement::InProcess => Self::InProcess(InProcessRecord {
                _held: Box::new(held),
            }),
            Placement::Served => Self::Served,
        }
    }

    /// Parks an in-process record in `yard`; an out-of-process one has dropped, and
    /// nothing of it waits. The editors of the plugin at `node` end either
    /// way: the plugin they were made for no longer serves it.
    pub(crate) fn settle(self, node: NodeKey, yard: &mut Graveyard) {
        yard.editors.end(node);
        match self {
            Self::InProcess(record) => yard.retired.push(record),
            Self::Served => {}
        }
    }
}

/// What an in-process plugin's retirement holds until the main thread drops
/// it: the only thing the [`Graveyard`] takes.
pub(crate) struct InProcessRecord {
    /// The owner whose drop waits for the main thread.
    _held: Box<dyn std::any::Any + Send>,
}

/// What a removal or a replacement took out of the session for an in-process
/// plugin, held until the main thread drops it (`Session::update` there).
///
/// A plugin record owns the plugin's control handle, and dropping the last
/// owner of an in-process plugin tears its library and instance down, a
/// main-thread call. `Session` is `Send` and has no main thread of its own,
/// so whatever retires such a record parks it here instead of dropping it,
/// and the view that holds the proof empties the yard. An out-of-process plugin's
/// record is not parked: it drops where it is retired, on a control thread.
/// A host that never reaches the OS's main thread (a headless one) never
/// empties the yard: [`len`](Self::len) is what `Phase::Plugins` warns on
/// past [`Graveyard::WARN_AT`].
#[derive(Default)]
pub(crate) struct Graveyard {
    /// Held to be dropped: a removed in-process plugin's record, or the handle
    /// a crossfade replaced.
    retired: Vec<InProcessRecord>,
    /// The editors that are alive: those of a retired plugin are closed by
    /// [`exhume`](Self::exhume) before the drop, the open ones are serviced.
    pub(crate) editors: crate::editor::Editors,
    warned: bool,
}

impl Graveyard {
    /// How many retired plugins a session holds before it says so.
    pub(crate) const WARN_AT: usize = 16;

    pub(crate) fn len(&self) -> usize {
        self.retired.len()
    }

    /// Drops everything in the yard, on the main thread. Returns how many.
    pub(crate) fn exhume(&mut self, main: MainThreadMarker) -> usize {
        self.editors.close_ended(main);
        let n = self.retired.len();
        self.retired.clear();
        self.warned = false;
        n
    }

    /// The idle tick of every open editor, on the main thread.
    pub(crate) fn service(&mut self, main: MainThreadMarker) {
        self.editors.service(main);
    }

    /// Warns once when the yard has grown past [`WARN_AT`](Self::WARN_AT).
    pub(crate) fn warn_if_piling(&mut self) {
        let waiting = self.retired.len() + self.editors.waiting();
        if waiting > Self::WARN_AT && !self.warned {
            self.warned = true;
            tracing::warn!(
                "{waiting} retired plugins and editors are waiting for the main thread: nothing \
                 has updated the session on the main thread (a host with no main thread never \
                 frees them)"
            );
        }
    }
}

/// What the session keeps for one hosted plugin. See the module docs.
pub(crate) struct HostedPlugin {
    /// Where the plugin runs: whether its record may drop off the main thread.
    placement: Placement,
    /// The plugin this is: from the load's ref, with the id the loaded plugin
    /// reports.
    plugin: PluginRef,
    pub(crate) handle: PluginHandle,
    pub(crate) controls: Option<PluginControls>,
    /// The state and preset requests of an out-of-process plugin:
    /// what makes its work values awaited.
    answers: Option<tutti_plugin::PluginAnswers>,
    /// What the node counts and holds (its meter cell and delivery counters).
    /// The project meter is installed only for a plugin the session added.
    pub(crate) node: NodeControls,
    /// What the host says of the plugin's track, which the main thread's
    /// `Phase::Plugins` delivers.
    track: Arc<crate::value::TrackInfoSlot>,
    /// The app's MIDI map, which the main thread's `Phase::Plugins` delivers.
    midi_map: Arc<crate::value::MidiMapSlot>,
    /// The controller table the plugin declares: read at the load, and again
    /// when the plugin re-declares it ([`PluginChange::MidiMapChanged`]).
    assignments: Vec<tutti_plugin_types::MidiAssignment>,
    /// What the plugins this record held before the current one lost, as
    /// [`Plugin::losses`] counts: banked when a plugin or its node is replaced,
    /// so the record's count only grows.
    banked: tutti_units::Losses,
    /// What the nodes before the held one did not render, or the plugin did
    /// not take ([`Diagnostics::block_losses`]): banked with `banked`.
    banked_blocks: tutti_plugin_host::BlockLosses,
    /// Round trips of the plugin's session tasks (a parameter read, a state
    /// snapshot) that panicked: [`Diagnostics::panicked_round_trips`]. Added to
    /// from the pool's threads.
    pub(crate) panicked_round_trips: tutti_rt::Tally<tutti_units::Calls>,
    /// Periodic state snapshots the plugin failed:
    /// [`Diagnostics::failed_snapshots`].
    pub(crate) failed_snapshots: tutti_rt::TallyWriter<tutti_units::Calls>,
    /// What the plugins before the held one had refused off their thread:
    /// banked when a plugin is replaced, so
    /// [`Diagnostics::refused_off_thread_calls`] only grows.
    banked_off_thread: tutti_units::Calls,
    liveness: Liveness,
    hang: HangDetector,
    status: PluginLiveness,
    /// Told a render mode since it was added (or crossfaded in).
    pub(crate) told: bool,
    /// The plugin's own bypass against the node's mode.
    pub(crate) bypass: crate::bypass::BypassSync,
    snapshot: Option<Saved>,
    /// When the snapshot's capture was asked for.
    snapshot_from: Option<Instant>,
    /// A snapshot left out of a reload ([`StateRestore::Withheld`]), kept for
    /// the host ([`StateView::withheld`]); later
    /// snapshots go to `snapshot` and never replace it.
    withheld: Option<Saved>,
    /// Until when a plugin that has just restored a state is suspected of
    /// dying of it.
    probation_until: Option<Instant>,
    /// How a plugin that died is brought back; `None`: not reloaded (hosted in
    /// process).
    reloadable: Option<Reloadable>,
    reload: Reload,
    /// Held by the snapshot task in flight, if one is.
    snapshot_token: Arc<()>,
    /// When the last snapshot was asked for, or (until the first) when the
    /// plugin was first observed.
    snapshot_at: Option<Instant>,
    /// What the plugin reported since the last `Phase::Plugins`, heard on
    /// whichever thread the plugin reports from.
    pub(crate) notices: Signals,
    /// The handle's subscription that fills `notices`; replaced with the handle.
    hearing: Subscription,
    /// What the plugin declares and the value of each parameter
    /// ([`Plugin::params`]).
    pub(crate) mirror: tutti_plugin_host::PluginParamValues,
    /// What a gesture asks before it sends: is this still the plugin, and does
    /// it still declare the parameter. Ended when the record retires or the
    /// plugin is replaced.
    pub(crate) gate: Gate,
    /// The lane knobs of the parameters a modulation lane drives: where a set
    /// of one goes.
    pub(crate) driven: crate::plugin_params::Driven,
    /// The writes the plugin did not take after the session read its
    /// declaration (a dropped parameter, a lost plugin, one that did not
    /// answer), which the value table counts and
    /// [`Plugin::losses`] reports as refused; and the ones owed again or
    /// refused by name ([`WriteLedger`](crate::value::WriteLedger)).
    writes: Arc<crate::value::WriteLedger>,
    /// The read that fills the mirror (a crossfade rebuilt it, the load's read
    /// was not answered, or the plugin announced a rescan), until
    /// `Phase::Plugins` starts it as a task.
    param_read: Option<tutti_plugin_host::ParamRead>,
    /// What the outgoing mirror declared, when a replace put the same plugin
    /// ([`PluginRef::is_same_plugin`]) at the node: the value table keeps these
    /// declarations and values until the new read lands, which replaces them.
    carried: Option<Vec<crate::value::CarriedParam>>,
    /// Held by that task: a later read mints a new one, and the earlier read
    /// lands nowhere.
    mirror_token: Arc<()>,
    /// Whether the node of a plugin that is gone renders silence in the plugin's
    /// place ([`lost`]).
    pub(crate) silence: Silence,
    /// The handle this plugin was loaded as, `None` for a plugin the host inserted
    /// itself. Dropped when the record retires.
    pub(crate) link: Option<NodeLink>,
    /// What re-inserts this plugin's node when it changes its ports. `None` for
    /// a plugin that reported no ports.
    pub(crate) rebind: Option<RebindState>,
}

/// What a hosted plugin's record keeps to re-insert its node when the plugin
/// changes its ports: the builder of the new node and the ports the node at the
/// key declares, built where those ports are known.
#[derive(Debug)]
pub(crate) enum RebindState {
    /// The node declares `laid_out`; nothing is moving.
    Idle {
        rebinder: tutti_plugin::catalog::PluginRebinder,
        laid_out: tutti_plugin_types::Layout,
    },
    /// A node over `laid_out` is staged in the graph and no commit that
    /// carries it has been sent: the node it replaces still holds the
    /// plugin's Processor and renders it.
    Staged(Staged),
    /// A node over `laid_out` was swapped in, its commit was sent and the
    /// plugin's Processor has not reached it yet: no second rebuild starts
    /// before it does.
    Settling {
        rebinder: tutti_plugin::catalog::PluginRebinder,
        laid_out: tutti_plugin_types::Layout,
    },
}

/// The node a rebuild staged and what moves the plugin to it once a commit has
/// carried it to the executor.
#[derive(Debug)]
pub(crate) struct Staged {
    pub(crate) rebinder: tutti_plugin::catalog::PluginRebinder,
    /// The ports the staged node declares.
    pub(crate) laid_out: tutti_plugin_types::Layout,
    /// Moves the Processor to the staged node.
    pub(crate) commit: tutti_plugin_host::Commit,
    /// The staged node's controls, which the record follows from the move.
    pub(crate) controls: Option<NodeControls>,
    /// The graph's commits sent when the node was staged: a later count means
    /// a commit carried it.
    pub(crate) commits: u64,
}

impl RebindState {
    /// The state of a node over the ports the plugin has now, or `None` for a
    /// plugin that has reported none (a lost one).
    pub(crate) fn new(rebinder: tutti_plugin::catalog::PluginRebinder) -> Option<Self> {
        let laid_out = rebinder.layout()?;
        Some(Self::Idle { rebinder, laid_out })
    }

    /// Whether a rebuilt node's Processor has not arrived.
    #[cfg(feature = "testing")]
    pub(crate) fn is_settling(&self) -> bool {
        matches!(self, Self::Staged(_) | Self::Settling { .. })
    }

    pub(crate) fn laid_out(&self) -> &tutti_plugin_types::Layout {
        match self {
            Self::Idle { laid_out, .. } | Self::Settling { laid_out, .. } => laid_out,
            Self::Staged(staged) => &staged.laid_out,
        }
    }

    /// The same builder, idle over the ports `laid_out`.
    pub(crate) fn idle(self, laid_out: tutti_plugin_types::Layout) -> Self {
        let rebinder = match self {
            Self::Idle { rebinder, .. } | Self::Settling { rebinder, .. } => rebinder,
            Self::Staged(staged) => staged.rebinder,
        };
        Self::Idle { rebinder, laid_out }
    }
}

/// The part of a plugin's record a [`ParamGesture`](crate::ParamGesture) keeps:
/// whether the plugin it began on is still the one at the node, and which
/// parameters it declares now. Shared (`Arc`) because a gesture spans frames
/// and cannot borrow the session; the session ends it when the record retires or
/// the plugin is replaced, and refreshes the ids when a read lands.
#[derive(Debug, Clone, Default)]
pub(crate) struct Gate(Arc<Mutex<GateState>>);

#[derive(Debug, Default)]
struct GateState {
    ended: bool,
    declared: std::collections::HashSet<ParamId>,
}

impl Gate {
    /// The plugin is gone: removed or replaced.
    pub(crate) fn end(&self) {
        self.0.lock().ended = true;
    }

    /// Whether the plugin is gone: removed or replaced.
    pub(crate) fn is_ended(&self) -> bool {
        self.0.lock().ended
    }

    /// Whether `other` is this same plugin instance's gate: a replaced plugin
    /// has a gate of its own.
    pub(crate) fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The ids the mirror declares now (a read landed).
    pub(crate) fn declare(&self, mirror: &tutti_plugin_host::PluginParamValues) {
        self.0.lock().declared = mirror.descriptors().iter().map(|p| p.id()).collect();
    }

    /// Whether the plugin is still there and declares `id`.
    pub(crate) fn holds(&self, id: ParamId) -> bool {
        let state = self.0.lock();
        !state.ended && state.declared.contains(&id)
    }
}

/// How a plugin was loaded, besides which plugin it is: what a reload needs.
#[derive(Debug, Clone)]
struct Origin {
    config: AudioConfig,
    /// The load as it was asked: its hosting and the catalog blacklist it ran
    /// under, which a reload asks again.
    loadable: Loadable,
}

/// What an out-of-process plugin's way back holds: the load it came from and,
/// with recovery on, the recovery's budget. A recovery without an origin is not
/// writable.
struct Reloadable {
    origin: Origin,
    recovery: Option<Recovery>,
}

/// A recovering plugin's reload, between its death and its replacement.
#[derive(Default)]
struct Reload {
    /// When to hand the load out.
    due: Option<Instant>,
    /// Held by the task in flight; a new one per task, so a late landing of an
    /// earlier task is recognised.
    token: Arc<()>,
    in_flight: bool,
    /// Raised by the task when it begins to run; a task queued behind a full pool has not
    /// raised it, and its load deadline has not begun.
    started: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the recovery's clock was restarted at the instant `started` was seen.
    clock_started: bool,
    /// The node's latency before the failure.
    latency_before: Samples,
    /// The age of a snapshot left out of the reload (see
    /// [`StateRestore::Withheld`]).
    withheld_age: Option<Duration>,
    /// A loaded replacement waiting for the graph to stop re-preparing.
    ready: Option<Ready>,
}

struct Ready {
    client: Box<tutti_plugin::PluginClient>,
    restore: StateRestore,
    mirror: Option<Mirror>,
}

impl std::fmt::Debug for HostedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("HostedPlugin");
        d.field("plugin", &self.plugin)
            .field("handle", &self.handle)
            .field("controls", &self.controls)
            .field("node", &self.node)
            .field("liveness", &self.liveness)
            .field("status", &self.status)
            .field("told", &self.told)
            .field("snapshot_token", &self.snapshot_token)
            .field("snapshot_at", &self.snapshot_at)
            .field("hearing", &self.hearing)
            .field("recovery", &self.recovery())
            .field("reload_due", &self.reload.due);
        d.field("mirror", &self.mirror)
            .field("param_read", &self.param_read)
            .field("mirror_token", &self.mirror_token);
        d.finish_non_exhaustive()
    }
}

impl HostedPlugin {
    /// The record of a plugin hosted in this process, whose node counts with
    /// `node`.
    pub(crate) fn new(handle: PluginHandle, plugin: PluginRef, node: NodeControls) -> Self {
        Self::placed(Placement::InProcess, handle, plugin, node)
    }

    /// The record of an out-of-process plugin, with its `controls`.
    pub(crate) fn served(
        handle: PluginHandle,
        plugin: PluginRef,
        controls: PluginControls,
        answers: tutti_plugin::PluginAnswers,
    ) -> Self {
        let mut record = Self::placed(Placement::Served, handle, plugin, (*controls).clone());
        record.controls = Some(controls);
        record.answers = Some(answers);
        record
    }

    fn placed(
        placement: Placement,
        handle: PluginHandle,
        plugin: PluginRef,
        node: NodeControls,
    ) -> Self {
        let (notices, hearing) = hear(&handle);
        let (mirror, read) = tutti_plugin_host::PluginParamValues::start(&handle);
        let assignments = handle.loaded().assignments.clone();
        Self {
            placement,
            plugin,
            handle,
            controls: None,
            answers: None,
            node,
            track: crate::value::TrackInfoSlot::new(),
            midi_map: crate::value::MidiMapSlot::new(),
            assignments,
            banked: tutti_units::Losses::NONE,
            banked_blocks: tutti_plugin_host::BlockLosses::default(),
            panicked_round_trips: tutti_rt::tally(),
            failed_snapshots: tutti_rt::tally().writer(),
            banked_off_thread: tutti_units::Calls::ZERO,
            liveness: Liveness::new(),
            hang: HangDetector::default(),
            status: PluginLiveness::Healthy,
            told: false,
            bypass: crate::bypass::BypassSync::default(),
            snapshot: None,
            snapshot_from: None,
            withheld: None,
            probation_until: None,
            reloadable: None,
            reload: Reload::default(),
            snapshot_token: Arc::new(()),
            snapshot_at: None,
            notices,
            hearing,
            mirror,
            gate: Gate::default(),
            driven: crate::plugin_params::Driven::default(),
            writes: Arc::default(),
            param_read: Some(read),
            carried: None,
            mirror_token: Arc::new(()),
            silence: Silence::Pending,
            link: None,
            rebind: None,
        }
    }

    /// Whether the plugin answers normally: its own bypass is compared only then.
    pub(crate) fn is_healthy(&self) -> bool {
        matches!(self.status, PluginLiveness::Healthy)
    }

    /// The incoming plugin of a crossfade at the same node: its handle and
    /// controls, a fresh debounce, and not yet told the render mode. Nothing
    /// of the outgoing plugin's state carries over but what it lost
    /// ([`Plugin::losses`] stays cumulative): its last good state is
    /// cleared (its bytes would restore the wrong plugin), a fresh snapshot token
    /// makes a task of the outgoing plugin still in flight land nowhere, the
    /// snapshot clock restarts, and its own-bypass state (the episode, the waits
    /// and the count) starts over.
    ///
    /// (`modulation`) A record that kept a parameter mirror gets one for
    /// the incoming plugin, from its handle (where its reports now arrive),
    /// not yet ready: its values are read by a task (one round trip per
    /// parameter, so not on the calling thread), which the next
    /// `Phase::Plugins` starts on the session's executor.
    /// The outgoing mirror's report callback was on the outgoing handle,
    /// which reports nothing here once it is gone.
    ///
    /// Returns the outgoing handle, to retire through [`Outgoing::retire`]: an
    /// in-process one waits in the graveyard, and an open editor of either
    /// kind is closed on the main thread.
    #[must_use = "the outgoing handle is retired through `Outgoing::retire`"]
    pub(crate) fn replace(
        &mut self,
        handle: PluginHandle,
        controls: PluginControls,
        plugin: PluginRef,
        answers: Option<tutti_plugin::PluginAnswers>,
    ) -> Outgoing {
        self.carry_declaration(&plugin);
        self.plugin = plugin;
        self.answers = answers;
        let outgoing = self.adopt(handle, Placement::Served);
        self.set_node((*controls).clone());
        self.controls = Some(controls);
        outgoing
    }

    /// [`replace`](Self::replace) for a plugin hosted in process, which has the
    /// node's controls and no plugin controls.
    #[must_use = "the outgoing handle is retired through `Outgoing::retire`"]
    pub(crate) fn replace_hosted(
        &mut self,
        handle: PluginHandle,
        node: NodeControls,
        plugin: PluginRef,
    ) -> Outgoing {
        self.carry_declaration(&plugin);
        self.plugin = plugin;
        self.answers = None;
        let outgoing = self.adopt(handle, Placement::InProcess);
        // A plugin in process is not reloaded: nothing of an out-of-process
        // outgoing plugin's way back carries over.
        self.reloadable = None;
        self.set_node(node);
        self.controls = None;
        outgoing
    }

    /// The recovery's state, when the plugin is reloaded and recovery is on.
    fn recovery(&self) -> Option<&Recovery> {
        self.reloadable.as_ref()?.recovery.as_ref()
    }

    fn recovery_mut(&mut self) -> Option<&mut Recovery> {
        self.reloadable.as_mut()?.recovery.as_mut()
    }

    /// Takes the recovery out, to be put back with
    /// [`restore_recovery`](Self::restore_recovery).
    fn take_recovery(&mut self) -> Option<Recovery> {
        self.reloadable.as_mut()?.recovery.take()
    }

    fn restore_recovery(&mut self, recovery: Option<Recovery>) {
        if let Some(reloadable) = self.reloadable.as_mut() {
            reloadable.recovery = recovery;
        }
    }

    /// This record, retired: an in-process plugin's waits for the main
    /// thread, an out-of-process plugin's drops here.
    pub(crate) fn retire(mut self) -> Retired {
        self.gate.end();
        // The node over a loaded plugin is gone: the link to its handle goes.
        self.link = None;
        Retired::of(self.placement, self)
    }

    /// The node's controls, `node` in place of the ones held: what the held
    /// ones counted is banked first.
    pub(crate) fn set_node(&mut self, node: NodeControls) {
        self.banked += self.node_losses();
        self.banked_blocks += self.node.block_losses();
        self.node = node;
    }

    /// What the node lost: MIDI past a block's capacity, in a block the plugin
    /// did not run, or from a SysEx message that lost a packet, and the MIDI
    /// the plugin sent that the node had no room to put out (all
    /// [`lost`](tutti_units::Losses::lost)), the parameter points it did not
    /// deliver as they were, the chords and scales past the capacity of a
    /// block's sequencer context ([refused](tutti_units::Losses::refused)), and,
    /// for an out-of-process plugin, the commands its bridge's queue
    /// [refused](tutti_units::Losses::refused) and the responses it
    /// [lost](tutti_units::Losses::lost).
    fn node_losses(&self) -> tutti_units::Losses {
        use tutti_units::Losses;
        let points = self.node.param_delivery();
        let bridge = self.controls.as_ref().map_or(Losses::NONE, |c| {
            let bridge = c.bridge_losses();
            Losses::refused(bridge.commands_refused) + Losses::lost(bridge.responses_lost)
        });
        bridge
            + Losses::lost(self.node.dropped_midi_in())
            + Losses::lost(self.node.midi_out_dropped())
            + Losses::refused(self.node.harmony_dropped())
            + Losses::lost(points.grid_points_dropped)
            + Losses::evicted(points.points_merged)
            + Losses::refused(points.points_dropped)
    }

    /// What the node did not render, or the plugin did not take, over the
    /// record's life: the held node's, and the nodes before it.
    pub(crate) fn block_losses(&self) -> tutti_plugin_host::BlockLosses {
        self.banked_blocks + self.node.block_losses()
    }

    /// The callbacks the plugin made from a thread its format forbids, which
    /// the host refused, over the record's life: the held plugin's and the
    /// ones before it.
    pub(crate) fn refused_off_thread_calls(&self) -> tutti_units::Calls {
        self.banked_off_thread + self.handle.refused_off_thread_calls()
    }

    /// What the plugin lost, over the record's life: what the held handle and
    /// node counted, and what the ones before them did.
    pub(crate) fn losses(&self) -> tutti_units::Losses {
        self.banked
            + self.node_losses()
            + tutti_units::Losses::lost(self.handle.dropped_parameter_events())
            + tutti_units::Losses::refused(self.writes.lost())
    }

    /// Keeps the declarations and values the mirror holds when `incoming` is
    /// the plugin at the node again (a recovery, a rebuild), so its parameters
    /// stay targets until the new read lands. Another plugin keeps none.
    fn carry_declaration(&mut self, incoming: &PluginRef) {
        if !self.plugin.is_same_plugin(incoming) {
            self.carried = None;
            return;
        }
        let held = self.mirror.is_ready().then(|| {
            self.mirror
                .descriptors()
                .iter()
                .enumerate()
                .map(|(at, info)| crate::value::CarriedParam::new(info, self.mirror.value_at(at)))
                .collect()
        });
        self.carried = held.or_else(|| self.carried.take());
    }

    /// [`replace`](Self::replace)'s handle half: everything but the
    /// controls.
    fn adopt(&mut self, handle: PluginHandle, placement: Placement) -> Outgoing {
        self.banked += tutti_units::Losses::lost(self.handle.dropped_parameter_events());
        self.banked_off_thread += self.handle.refused_off_thread_calls();
        let (mirror, read) = tutti_plugin_host::PluginParamValues::start(&handle);
        self.mirror = mirror;
        self.param_read = Some(read);
        self.mirror_token = Arc::new(());
        // The incoming plugin declares a table of its own.
        self.assignments = handle.loaded().assignments.clone();
        // A gesture on the outgoing plugin ends here: it is not this one.
        self.gate.end();
        self.gate = Gate::default();
        // What the outgoing plugin reported and nobody drained is its own.
        (self.notices, self.hearing) = hear(&handle);
        // The incoming plugin has a rebinder of its own, set by the caller.
        self.rebind = None;
        let outgoing = Outgoing {
            handle: std::mem::replace(&mut self.handle, handle),
            placement: std::mem::replace(&mut self.placement, placement),
        };
        self.liveness = Liveness::new();
        self.hang = HangDetector::default();
        self.status = PluginLiveness::Healthy;
        self.told = false;
        // The incoming plugin has been told nothing of its track.
        self.track.resend();
        self.midi_map.resend();
        // The writes owed or untold were the outgoing plugin's.
        self.writes.end_epoch();
        self.snapshot = None;
        self.snapshot_from = None;
        self.withheld = None;
        self.probation_until = None;
        if let Some(reloadable) = self.reloadable.as_mut() {
            reloadable.recovery = reloadable
                .recovery
                .as_ref()
                .map(|r| Recovery::new(r.policy()));
        }
        self.reload = Reload::default();
        // The incoming plugin's bypass is its own: nothing of the outgoing one's
        // episode, waits or count carries over.
        self.bypass = crate::bypass::BypassSync::default();
        self.silence = Silence::Pending;
        self.snapshot_token = Arc::new(());
        self.snapshot_at = None;
        outgoing
    }

    /// Takes the snapshot for the cause of a failure: the next load goes
    /// without it, and it is not offered again.
    fn withhold_snapshot(&mut self, now: Instant) {
        if let Some(state) = self.snapshot.take() {
            self.withheld = Some(state);
            self.reload.withheld_age = Some(
                self.snapshot_from
                    .map_or(Duration::ZERO, |t| now.saturating_duration_since(t)),
            );
        }
        self.snapshot_from = None;
        self.probation_until = None;
    }

    /// Records a scheduled reload and tells the host. The node stays.
    fn begin_reload(
        &mut self,
        cause: ReloadCause,
        now: Instant,
        delay: Duration,
        attempt: u32,
        node: NodeKey,
        out: &mut Vec<SessionEvent>,
    ) {
        self.status = PluginLiveness::Recovering {
            cause: cause.clone(),
        };
        self.reload.due = Some(now + delay);
        self.reload.in_flight = false;
        self.reload.started = Arc::default();
        self.reload.clock_started = false;
        // A task of an earlier attempt lands nowhere.
        self.reload.token = Arc::new(());
        out.push(event(
            node,
            PluginChange::Reloading {
                cause,
                attempt,
                delay,
            },
        ));
    }

    /// Parameter `id`'s value, as a modulation lane's knob starts at it:
    /// `None` while not read, `Some(None)` for one it does not list.
    pub(crate) fn param_value(&self, id: tutti_units::ParamId) -> Option<Option<f32>> {
        self.mirror.get(id).map(|v| v.map(|v| v.get() as f32))
    }

    /// Sets the builder of the node that replaces this plugin's when it
    /// changes its ports, which `Phase::Plugins` ticks and re-inserts. `None`
    /// for a plugin that reported no ports.
    pub(crate) fn set_rebinder(&mut self, rebinder: tutti_plugin::catalog::PluginRebinder) {
        self.rebind = RebindState::new(rebinder);
    }

    /// Queues the announcement that every parameter value changed, as the
    /// plugin's own would arrive: a load the host made moves values the plugin
    /// may not report, and the next `Phase::Plugins` reads them again.
    pub(crate) fn refresh_values(&self) {
        let mut waiting = self.notices.lock();
        // One read serves every request made before the next phase.
        if !waiting
            .iter()
            .any(|signal| matches!(signal, PluginSignal::Refresh(PluginRefresh::ParamValues)))
        {
            waiting.push(PluginSignal::Refresh(PluginRefresh::ParamValues));
        }
    }

    /// Whether the plugin reacts to the host's automation mode.
    pub(crate) fn takes_automation_mode(&self) -> bool {
        self.handle
            .loaded()
            .features
            .contains(tutti_plugin_types::Features::AUTOMATION_STATE)
    }

    /// Registers the plugin's parameters and automation mode with the value
    /// table at `node`, as the mirror and the handle hold them now.
    pub(crate) fn provide_values(&self, node: NodeKey, table: &crate::value::ValueTable) {
        let entry = crate::value::PluginEntry::new(
            self.handle.clone(),
            self.driven.clone(),
            &self.mirror,
            self.carried.as_deref(),
            Arc::clone(&self.writes),
        );
        let mode = self
            .takes_automation_mode()
            .then(|| crate::value::AutomationModeSlot::cell(self.handle.clone()));
        table.provide_plugin(node, entry, mode, self.track.clone(), self.midi_map.clone());
    }

    /// The IPC timeouts the plugin was loaded with, which bound its loops' round trips; the
    /// defaults for a plugin with no origin (one the host inserted in process).
    fn timeouts(&self) -> tutti_plugin_types::Timeouts {
        self.reloadable
            .as_ref()
            .map_or_else(Default::default, |r| r.origin.config.timeouts)
    }

    /// Asks for the parameters to be read again (the plugin announced a
    /// rescan): a fresh read, handed to the host at the next
    /// `Phase::Plugins`, and a fresh token, so a read still in flight lands
    /// nowhere.
    fn reread_params(&mut self) {
        self.param_read = Some(tutti_plugin_host::ParamRead::of(&self.handle));
        self.mirror_token = Arc::new(());
    }
}

/// The type of a hosted plugin's [`Key`]: what [`Session::apply`]
/// and [`Graph::add_plugin`](crate::Graph::add_plugin) return, so a plugin's
/// ports and parameters are methods of its key as a native node's are.
///
/// A marker only: no value of it exists. It names the kind of node, any
/// plugin format, in or out of process, as [`Key::erase`] names none; the
/// node below the session is `tutti_plugin_host::PluginNode`, and the view of
/// the plugin is [`Plugin`].
/// `key.param(id)` is a plugin parameter's
/// `ParamRef` (see `Key::param`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnyPlugin {}

impl Key<AnyPlugin> {
    /// Parameter `id` of the plugin, as the target `session.set` writes and a
    /// modulation route or an automation lane targets: the same one
    /// [`PluginParam::target`](crate::PluginParam::target) gives, and it converts into the
    /// [`ParamRef`](crate::ParamRef) a native node's param does, so a route
    /// names a plugin's parameter and a native one alike.
    ///
    /// A plain value, like a native param's: it borrows no session and checks
    /// nothing. An id the plugin does not declare is refused where the target
    /// is written ([`SetRefusal::Plugin`](crate::SetRefusal::Plugin)) or a route
    /// is resolved (`Mods`'s [`Problem`](crate::Problem));
    /// [`Plugin::params`] resolves one now.
    #[must_use]
    pub fn param(self, id: ParamId) -> crate::PluginParamTarget {
        crate::PluginParamTarget::new(self.erase(), id)
    }
}

/// A hosted plugin, lent by [`Session::plugin_view`]: its identity, and the
/// surfaces the session holds for it.
///
/// A view borrows the session, so it cannot outlive the plugin it names: a
/// node that is removed or replaced while one is held is a borrow error, and
/// what is kept across updates is the node's key, looked up again.
#[derive(Debug, Clone, Copy)]
pub struct Plugin<'a> {
    node: NodeKey,
    hosted: &'a HostedPlugin,
    figures: Figures,
}

impl<'a> Plugin<'a> {
    pub(crate) fn new(node: NodeKey, hosted: &'a HostedPlugin, figures: Figures) -> Self {
        Self {
            node,
            hosted,
            figures,
        }
    }

    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// The plugin's name, as it declares it.
    pub fn name(&self) -> &'a str {
        self.hosted.handle.name()
    }

    /// What the plugin declares about itself.
    pub fn descriptor(&self) -> &'a tutti_plugin_types::PluginDescriptor {
        self.hosted.handle.descriptor()
    }

    /// The plugin this is: its file, its format and its id.
    pub fn plugin(&self) -> &'a PluginRef {
        &self.hosted.plugin
    }

    /// What the plugin did not get or the host did not hear, over the life of
    /// this node's plugin: cumulative, and carried across a crash reload, a
    /// swap to another plugin and a rebuild of the node at the same key, so it
    /// only grows. In events, as the MIDI inlets count theirs
    /// ([`Losses`](tutti_units::Losses)):
    ///
    /// - [`lost`](tutti_units::Losses::lost): MIDI events on the node's event input
    ///   that never reached the plugin (in a
    ///   block the plugin did not run, or the rest of a SysEx message that
    ///   lost a packet; events past the node's declared event-input bound are
    ///   cut by the graph before the plugin runs, and are reported as
    ///   [`SessionEvent::EventsDropped`]
    ///   with `port: None`, not here); parameter points of a sent grid that a full queue
    ///   dropped; and parameter edits the plugin made that the host did not
    ///   hear (a lost edit costs a stale readout, not audio, and the session
    ///   re-reads the parameters after one).
    /// - [`evicted`](tutti_units::Losses::evicted): parameter points that replaced
    ///   the last one of a full queue.
    /// - [`refused`](tutti_units::Losses::refused): parameter points for a
    ///   parameter the node has no slot for, writes of a parameter the plugin
    ///   dropped after the session read its declaration (a rescan), and chords
    ///   and scales past the capacity of a block's sequencer context.
    ///
    /// The MIDI the plugin sent that the node had no room to put out is in
    /// [`lost`](tutti_units::Losses::lost) too.
    ///
    /// [`late`](tutti_units::Losses::late) stays zero. Audio the plugin's pipeline did
    /// not answer in time is not an event loss: it is counted in chunks, by
    /// [`Diagnostics::chunks`]; the blocks the node did not render and
    /// the entries the plugin refused are counted in blocks, by
    /// [`Diagnostics::block_losses`].
    #[must_use]
    pub fn losses(&self) -> tutti_units::Losses {
        self.hosted.losses()
    }

    /// The presets the plugin lists, read now on the main thread: the list of
    /// the plugin instance at this node, which [`Session::load_preset`] takes
    /// back. A plugin that lists none, or is gone, has an empty list.
    pub fn presets(&self, main: MainThreadMarker) -> crate::PluginPresets {
        let handle = &self.hosted.handle;
        let presets = crate::PluginPresets::new(handle.preset_support(), handle.instance());
        if presets.support().can_list() {
            presets.fill(handle.presets(main));
        }
        presets
    }

    /// The plugin's saved states: the last good one and the withheld one.
    /// A save now and a restore are [`Session::save_state`] and
    /// [`Session::restore_state`].
    pub fn state(&self) -> StateView<'a> {
        StateView {
            hosted: self.hosted,
        }
    }

    /// How the plugin is doing: running, stalled, dead and about to be
    /// written off, or being reloaded.
    pub fn health(&self) -> PluginHealth {
        let hosted = self.hosted;
        match &hosted.status {
            PluginLiveness::Recovering { cause } => PluginHealth::Reloading {
                attempt: hosted.recovery().map_or(1, Recovery::failures),
                cause: cause.clone(),
            },
            PluginLiveness::Dead { cause } => PluginHealth::Died {
                cause: cause.clone(),
            },
            PluginLiveness::Healthy | PluginLiveness::Failing => match hosted.handle.status() {
                PluginStatus::Dead { cause } => PluginHealth::Died { cause },
                _ if hosted.hang.is_stalled() => PluginHealth::Stalled,
                _ => PluginHealth::Running,
            },
        }
    }

    /// The plugin's diagnostics: its server, its dropouts and losses, and the
    /// latency and tail the graph holds for it.
    pub fn diagnostics(&self) -> Diagnostics<'a> {
        Diagnostics::new(self.hosted, self.figures)
    }

    /// The plugin's buses as the graph's bus handles, to wire with
    /// `Graph::connect`.
    pub fn buses(&self) -> PluginBuses<'a> {
        PluginBuses::new(self.node, self.hosted)
    }

    /// How many times the plugin's own bypass was found other than its node's
    /// mode after a write, over this plugin instance's life (a replacement or a
    /// reload starts the count again): a write that was lost, a write the
    /// plugin refused while it held another bypass, or a bypass its editor, a
    /// restored state or its automation moved since. One count per episode, not
    /// per retry, and a refusal is latched until the node's mode changes; `0`
    /// for a plugin with no bypass of its own, which the executor bypasses.
    #[must_use]
    pub fn bypass_writes_unapplied(&self) -> u64 {
        self.hosted.bypass.unapplied()
    }

    /// The plugin's control handle, the low layer the views are built on
    /// (feature `testing`: a host uses the views).
    #[cfg(feature = "testing")]
    pub fn handle(&self) -> &'a PluginHandle {
        &self.hosted.handle
    }

    /// The plugin's parameters: what it declares, read at load and again when
    /// it announces a rescan, and the value of each, kept current from what the
    /// plugin reports and the host sets.
    pub fn params(&self) -> PluginParams<'a> {
        PluginParams::new(self.node, self.hosted)
    }

    /// The target of what the host tells the plugin about the track it sits on.
    /// A format with no place for it takes it and drops it.
    #[must_use]
    pub fn track_info(&self) -> crate::PluginTrackInfoTarget {
        crate::PluginTrackInfoTarget::new(self.node)
    }

    /// The controller table the plugin declares (VST3 `IMidiMapping`, AU's
    /// parameter MIDI mappings): empty for a plugin with none. Read at the
    /// load and again whenever the plugin re-declares it
    /// ([`PluginChange::MidiMapChanged`]). The app's own entries are
    /// [`midi_map`](Self::midi_map), and
    /// [`MidiMap::effective`](crate::MidiMap::effective) reads the two together.
    #[must_use]
    pub fn assignments(&self) -> &'a [tutti_plugin_types::MidiAssignment] {
        &self.hosted.assignments
    }

    /// The target of the app's controller map of the plugin: which MIDI
    /// messages drive which parameters, beside the table the plugin declares
    /// ([`assignments`](Self::assignments)).
    #[must_use]
    pub fn midi_map(&self) -> crate::PluginMidiMapTarget {
        crate::PluginMidiMapTarget::new(self.node)
    }

    /// The target of the automation mode the host tells the plugin, or `None`
    /// for a plugin whose format has no such channel
    /// ([`Features::AUTOMATION_STATE`](tutti_plugin_types::Features) clear).
    #[must_use]
    pub fn automation_mode(&self) -> Option<crate::PluginAutomationModeTarget> {
        self.hosted
            .takes_automation_mode()
            .then(|| crate::PluginAutomationModeTarget::new(self.node))
    }
}

/// A hosted plugin's states ([`Plugin::state`]): what the session holds of
/// them. A save now and a restore are [`Session::save_state`] and
/// [`Session::restore_state`].
#[derive(Debug, Clone, Copy)]
pub struct StateView<'a> {
    hosted: &'a HostedPlugin,
}

impl<'a> StateView<'a> {
    /// The last state captured while the plugin was healthy: the
    /// crash-recovery copy a reload restores, up to a snapshot interval
    /// stale. A project is saved with [`Session::save_state`], not from this.
    pub fn last_good(&self) -> Option<&'a Saved> {
        self.hosted.snapshot.as_ref()
    }

    /// The state a reload left out because the plugin died loading it, or
    /// soon after ([`StateRestore::Withheld`]): the user's last state, which a
    /// host may still offer to load by hand. Kept until the plugin is
    /// withheld from again or replaced by the host; the states captured since
    /// (of a plugin at its defaults) do not replace it.
    pub fn withheld(&self) -> Option<&'a Saved> {
        self.hosted.withheld.as_ref()
    }
}

/// `Phase::RenderMode`'s record: the mode last announced.
#[derive(Debug)]
pub(crate) struct RenderModeState {
    pub(crate) announced: tutti_plugin_types::RenderMode,
}

impl Default for RenderModeState {
    fn default() -> Self {
        Self {
            announced: tutti_plugin_types::RenderMode::Realtime,
        }
    }
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// Lands what the plugin loops sent since the last phase, in arrival order.
    fn land_arrivals(&mut self, out: &mut Vec<SessionEvent>) {
        for arrival in self.plugins.inbox.drain() {
            match arrival {
                Arrival::Reload {
                    node,
                    token,
                    snapshot_age,
                    loaded,
                } => self.land_reload(node, &token, snapshot_age, *loaded, out),
                Arrival::ReloadEnded { node, token, cause } => {
                    self.reload_ended(node, &token, cause, out);
                }
                Arrival::Params {
                    node,
                    token,
                    values,
                } => {
                    self.land_params(node, &token, values);
                }
                Arrival::ParamsLost { node, token } => {
                    // The read panicked: it is not made again (a plugin that panics at
                    // every question would be read for ever). The mirror stays unread,
                    // and the plugin's own rescan starts a read afresh.
                    if let Some(hosted) = self.graph.plugins.get_mut(&node)
                        && Arc::ptr_eq(&token, &hosted.mirror_token)
                    {
                        out.push(event(node, PluginChange::ParamsUnread));
                    }
                }
                Arrival::Snapshot {
                    node,
                    token,
                    asked,
                    state,
                } => {
                    self.land_snapshot(node, &token, asked, state);
                }
            }
        }
    }

    /// Puts an opened plugin in the graph, unwired, and hosts it: the one
    /// place a plugin enters the session, through
    /// [`apply`](Self::apply).
    /// Its meter is the metronome's, an out-of-process plugin keeps how it
    /// was loaded for a reload (`origin`) and the session's recovery, and a
    /// parameter read that finished at the load is its mirror.
    fn install_plugin(
        &mut self,
        plugin: HostPlugin,
        mirror: Option<Mirror>,
        origin: Origin,
    ) -> (Key<AnyPlugin>, tutti_plugin::catalog::PluginRebinder) {
        let handle = plugin.handle().clone();
        let loaded_ref = plugin.plugin().clone();
        let meter = self.transport.settings().project_meter().clone();
        let (key, rebinder) = match plugin.into_client() {
            Ok(client) => {
                client.controls().set_meter(meter);
                let (key, rebinder) = self.graph.add_plugin_with_rebinder(client);
                if let Some(hosted) = self.graph.plugins.get_mut(&key.erase()) {
                    hosted.reloadable = Some(Reloadable {
                        origin,
                        recovery: self.plugins.recovery.map(Recovery::new),
                    });
                }
                (key, rebinder)
            }
            Err(plugin) => {
                let node_controls = plugin.node_controls();
                node_controls.set_meter(meter);
                let (bound, rebinder) = (*plugin).bind_with_rebinder(self.graph.tempo.clone());
                let (node, _) = self
                    .graph
                    .place(NodeKey::fresh(), crate::graph::Split::of(bound));
                let hosted = HostedPlugin::new(handle, loaded_ref, node_controls);
                self.graph.plugins.insert(node.erase(), hosted);
                self.graph.write_own_bypass(node.erase());
                (Key::new(node.erase()), rebinder)
            }
        };
        if let Some(hosted) = self.graph.plugins.get_mut(&key.erase())
            && let Some(Mirror { values, unread }) = mirror
        {
            hosted.mirror = values;
            hosted.param_read = unread;
            hosted.gate.declare(&hosted.mirror);
        }
        self.graph.provide_plugin(key.erase());
        (key, rebinder)
    }

    /// The view of the plugin hosted at `node`, a borrow of its record: its
    /// params, state, diagnostics, buses and health. It reads the
    /// node's plugin; the instance it was loaded as ([`PluginInstance`]) is the
    /// plugin's, whether or not a node has it.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`](crate::NoSuchNode) when no plugin is hosted at `node`.
    pub fn plugin_view(&self, node: impl Into<NodeKey>) -> Result<Plugin<'_>, crate::NoSuchNode> {
        let node = node.into();
        self.graph
            .plugins
            .get(&node)
            .map(|hosted| Plugin::new(node, hosted, self.graph.plugin_figures(node)))
            .ok_or_else(|| crate::NoSuchNode::new(node))
    }

    /// How many retired plugins wait for the main thread to drop them: what a
    /// removal or a replacement parked and no [`Session::update`] on the main thread
    /// has freed yet. Nonzero for good means no one calls it; the session
    /// warns once past a few.
    pub fn retired_plugins(&self) -> usize {
        self.graph.graveyard.len()
    }

    /// How many plugin tasks (a snapshot, a parameter read, a reload) are queued or
    /// running on the session's executor. They share its blocking pool with the session's
    /// other blocking work, so a task may wait in the pool's queue; a session's close or
    /// drop waits for them, each bounded by its round trip's IPC deadline.
    pub fn plugin_tasks(&self) -> usize {
        self.plugins.tasks.in_flight()
    }

    /// How many plugin tasks the executor refused to start since the session began (it had
    /// shut down, or the OS refused a thread). Each loop asks again at its next interval.
    pub fn plugin_task_refusals(&self) -> u64 {
        self.plugins.tasks.refused()
    }

    /// Every hosted plugin, in node order.
    pub fn plugins(&self) -> impl Iterator<Item = Plugin<'_>> {
        self.graph
            .plugins
            .iter()
            .map(|(&node, hosted)| Plugin::new(node, hosted, self.graph.plugin_figures(node)))
    }

    /// Lands a finished snapshot: the plugin's last snapshot becomes it. A
    /// refused one is logged and keeps the previous snapshot (a good
    /// recovery point beats none). Returns whether it was stored. Frees the
    /// plugin for its next snapshot (the landing's own `token` clone drops).
    ///
    /// A snapshot of a plugin no longer at its node (removed, or crossfaded
    /// to another plugin since the task was asked for) lands nowhere: it is
    /// the outgoing plugin's state, not a recovery point for the node.
    pub(crate) fn land_snapshot(
        &mut self,
        node: NodeKey,
        token: &Arc<()>,
        asked: Instant,
        state: Result<tutti_plugin_types::PluginState, StateError>,
    ) -> bool {
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(token, &hosted.snapshot_token) {
            tracing::debug!("plugin {node:?}: a snapshot of its previous plugin was discarded");
            return false;
        }
        match state {
            Ok(state) => {
                hosted.snapshot = Some(Saved::with_pending(
                    PeerState::of_plugin(hosted.plugin.clone(), state.bytes),
                    state.pending,
                ));
                hosted.snapshot_from = Some(asked);
                true
            }
            Err(e) => {
                tracing::warn!("plugin {node:?}: periodic state snapshot failed: {e}");
                hosted.failed_snapshots.add(tutti_units::Calls::new(1));
                false
            }
        }
    }

    /// Lands a finished parameter read: the plugin's mirror holds its
    /// values and is ready. Returns whether it landed. A read of a plugin no
    /// longer at its node (removed, or crossfaded again since) lands
    /// nowhere. A read the plugin did not answer (its list unavailable) does
    /// not land either: the mirror stays not ready, the read goes back to the
    /// plugin's record, and the next `Phase::Plugins` starts it again.
    pub(crate) fn land_params(
        &mut self,
        node: NodeKey,
        token: &Arc<()>,
        values: ParamValues,
    ) -> bool {
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(token, &hosted.mirror_token) {
            tracing::debug!(
                "plugin {node:?}: a parameter read of its previous plugin was discarded"
            );
            return false;
        }
        match values {
            Ok(values) => {
                hosted.mirror.land(values);
                hosted.carried = None;
                hosted.driven.report_knobs(&hosted.mirror);
                hosted.gate.declare(&hosted.mirror);
                self.graph.provide_plugin(node);
                true
            }
            Err(read) => {
                tracing::debug!(
                    "plugin {node:?}: its parameter list was unavailable; the read is kept to run again"
                );
                hosted.param_read = Some(*read);
                false
            }
        }
    }

    /// Lands a finished reload, at the time of the last `Phase::Plugins`: the
    /// replacement takes the dead plugin's node, with its wiring
    /// (`Graph::replace_plugin`), its snapshot and failure history. What
    /// happened is pushed to `out` as the events `Phase::Plugins` would have
    /// reported.
    ///
    /// A reload of a node no longer being recovered (removed, replaced by
    /// another plugin, or an earlier attempt's) lands nowhere; the loaded
    /// plugin is dropped, which ends its server. A failed load is the next
    /// attempt's backoff, or [`PluginChange::GaveUp`] and the node's removal.
    ///
    /// Takes no main-thread proof: only an in-process plugin is opened on the
    /// main thread, and an out-of-process plugin's replacement must not be one, so a
    /// load that came back unopened is a failed attempt
    /// ([`RecoveryFailure::ReplacementInProcess`]) without opening it.
    pub(crate) fn land_reload(
        &mut self,
        node: NodeKey,
        token: &Arc<()>,
        snapshot_age: Option<Duration>,
        loaded: PluginLoaded,
        out: &mut Vec<SessionEvent>,
    ) {
        let now = self.plugins.now;
        let mut gave_up = Vec::new();
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        if !Arc::ptr_eq(token, &hosted.reload.token)
            || !matches!(hosted.status, PluginLiveness::Recovering { .. })
        {
            tracing::debug!("plugin {node:?}: a reload that no longer applies was discarded");
            return;
        }
        hosted.reload.in_flight = false;
        let opened = match loaded.opened {
            Opened::Done(open) => *open,
            Opened::InProcess => {
                self.fail_reload(
                    node,
                    RecoveryFailure::ReplacementInProcess,
                    now,
                    out,
                    &mut gave_up,
                );
                self.finish_gave_up(gave_up, out);
                return;
            }
        };
        let Open {
            plugin,
            restore,
            mirror,
            replay: _,
        } = opened;
        let client = match plugin.map(HostPlugin::into_client) {
            Ok(Ok(client)) => client,
            Ok(Err(_in_process)) => {
                self.fail_reload(
                    node,
                    RecoveryFailure::ReplacementInProcess,
                    now,
                    out,
                    &mut gave_up,
                );
                self.finish_gave_up(gave_up, out);
                return;
            }
            Err(e) => {
                // A load that died restoring the snapshot is held against the
                // snapshot, as a replacement that dies once installed is
                // (`install_reload`): the next attempt goes without it.
                if matches!(restore, Some(Err(_)))
                    && let Some(hosted) = self.graph.plugins.get_mut(&node)
                {
                    hosted.withhold_snapshot(now);
                }
                self.fail_reload(
                    node,
                    RecoveryFailure::ReloadFailed {
                        source: Arc::new(e),
                    },
                    now,
                    out,
                    &mut gave_up,
                );
                self.finish_gave_up(gave_up, out);
                return;
            }
        };
        let restore = match (restore, snapshot_age) {
            (Some(Ok(())), Some(age)) => StateRestore::Restored { age },
            (Some(Err(error)), Some(age)) => StateRestore::Refused { age, error },
            _ => StateRestore::NoSnapshot,
        };
        let ready = Ready {
            client,
            restore,
            mirror,
        };
        self.install_reload(node, ready, now, out);
    }

    /// Puts a loaded replacement into the graph at `node`, or keeps it until
    /// the graph stops re-preparing.
    fn install_reload(
        &mut self,
        node: NodeKey,
        ready: Ready,
        now: Instant,
        out: &mut Vec<SessionEvent>,
    ) {
        let Ready {
            client,
            restore,
            mirror,
        } = ready;
        // Dead already (a state that kills it, say): not recovered.
        if client.is_crashed() {
            if !matches!(
                restore,
                StateRestore::NoSnapshot | StateRestore::Withheld { .. }
            ) && let Some(hosted) = self.graph.plugins.get_mut(&node)
            {
                hosted.withhold_snapshot(now);
            }
            let cause = RecoveryFailure::ReplacementDied {
                cause: client
                    .handle()
                    .status()
                    .cause()
                    .cloned()
                    .unwrap_or(PluginFailure::Unreported),
            };
            let mut gave_up = Vec::new();
            self.fail_reload(node, cause, now, out, &mut gave_up);
            self.finish_gave_up(gave_up, out);
            return;
        }
        let meter = self.transport.settings().project_meter().clone();
        client.controls().set_meter(meter);
        // The node's edges stay as they are, so a replacement with other
        // ports would leave the topology invalid and refuse every later
        // commit: it is a failed attempt instead, and dropping it ends its
        // server.
        let got = client.declared_shape();
        if let Some(was) = self.graph.shape(node) {
            let counts = |s: &tutti_graph::Shape| PortCounts {
                audio_in: usize::from(s.audio_in().count()),
                audio_out: usize::from(s.audio_out().count()),
                event_in: usize::from(s.event_in),
                event_out: usize::from(s.event_out),
            };
            if counts(&was) != counts(&got) {
                let cause = RecoveryFailure::ReplacementPorts {
                    got: counts(&got),
                    was: counts(&was),
                };
                let mut gave_up = Vec::new();
                self.fail_reload(node, cause, now, out, &mut gave_up);
                self.finish_gave_up(gave_up, out);
                return;
            }
        }
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        // What the replacement carries over from the record, which a swap
        // resets (`HostedPlugin::replace`).
        let snapshot = hosted.snapshot.take();
        let snapshot_from = hosted.snapshot_from;
        let withheld_bytes = hosted.withheld.take();
        let recovery = hosted.take_recovery();
        let latency_before = hosted.reload.latency_before;
        let withheld = hosted.reload.withheld_age;
        let attempts = recovery.as_ref().map_or(1, Recovery::failures);
        match self.graph.replace_plugin_with_rebinder(
            node,
            client,
            None,
            crate::graph::OnShape::Cut,
        ) {
            Ok(rebinder) => {
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.set_rebinder(rebinder);
                }
            }
            Err(crate::graph::SwapRefused::Busy(client)) => {
                // Between a re-prepare's two commits: try again next phase.
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.snapshot = snapshot;
                    hosted.withheld = withheld_bytes;
                    hosted.restore_recovery(recovery);
                    hosted.reload.ready = Some(Ready {
                        client,
                        restore,
                        mirror,
                    });
                }
                return;
            }
            Err(crate::graph::SwapRefused::Failed(refused)) => {
                tracing::error!("plugin {node:?}: the replacement was refused: {refused}");
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.snapshot = snapshot;
                    hosted.withheld = withheld_bytes;
                    hosted.restore_recovery(recovery);
                }
                let mut gave_up = Vec::new();
                let cause = RecoveryFailure::ReplacementRefused { source: refused };
                self.fail_reload(node, cause, now, out, &mut gave_up);
                self.finish_gave_up(gave_up, out);
                return;
            }
        }
        let latency_after = self.graph.node_latency(node).unwrap_or_default();
        let table = self.graph.value_table();
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        // The writes the restored snapshot did not hold, through the new
        // plugin's ledger: the swap ended the old one's epoch, so nothing owed by
        // the dead instance is sent over them.
        if matches!(restore, StateRestore::Restored { .. })
            && let Some(saved) = &snapshot
        {
            hosted.writes.send_each(&hosted.handle, saved.pending());
        }
        hosted.snapshot = snapshot;
        hosted.withheld = withheld_bytes;
        hosted.snapshot_from = snapshot_from;
        hosted.restore_recovery(recovery);
        if let Some(recovery) = hosted.recovery_mut() {
            recovery.step(RecoveryEvent::Loaded, now);
        }
        // The replacement's own read, after the restore, is the mirror: the
        // swap built an unread one, and nothing of the old plugin's values
        // carries over (they are the values read at load, which the restored
        // state has replaced; writing them back would undo the restore and
        // bypass the lanes that own those parameters).
        //
        // A modulation lane on a parameter needs no seeding here: the swap
        // moved the node's generation, so the apply rebuilds the lane (a
        // new node, its knob starting at this mirror's value) and the old
        // lane, with whatever GUI value its knob held, goes with the old unit.
        // The new lane's first value is the restored one.
        if let Some(Mirror { values, unread }) = mirror {
            hosted.mirror = values;
            hosted.param_read = unread;
            hosted.mirror_token = Arc::new(());
            hosted.gate.declare(&hosted.mirror);
            hosted.provide_values(node, &table);
        }
        let restore = match (restore, withheld) {
            (StateRestore::NoSnapshot, Some(age)) => StateRestore::Withheld { age },
            (restore, _) => restore,
        };
        hosted.probation_until = match (&restore, hosted.recovery()) {
            (StateRestore::Restored { .. }, Some(r)) => Some(now + r.policy().state_probation),
            _ => None,
        };
        hosted.status = PluginLiveness::Healthy;
        hosted.reload = Reload::default();
        out.push(event(
            node,
            PluginChange::Recovered {
                state: restore,
                latency_before,
                latency_after,
                attempts,
            },
        ));
    }

    /// One reload attempt failed (or its task was dropped): the next backoff,
    /// or the end of trying.
    fn fail_reload(
        &mut self,
        node: NodeKey,
        cause: RecoveryFailure,
        now: Instant,
        out: &mut Vec<SessionEvent>,
        gave_up: &mut Vec<(NodeKey, GiveUp)>,
    ) {
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        hosted.reload.in_flight = false;
        let Some(recovery) = hosted.recovery_mut() else {
            return;
        };
        let action = recovery.step(
            RecoveryEvent::LoadFailed {
                cause: cause.clone(),
            },
            now,
        );
        self.apply_failure(node, action, ReloadCause::Attempt(cause), now, out, gave_up);
    }

    /// Carries out the answer to a failed attempt.
    fn apply_failure(
        &mut self,
        node: NodeKey,
        action: RecoveryAction,
        reason: ReloadCause,
        now: Instant,
        out: &mut Vec<SessionEvent>,
        gave_up: &mut Vec<(NodeKey, GiveUp)>,
    ) {
        match action {
            RecoveryAction::Reload { delay, attempt } => {
                tracing::warn!("plugin {node:?}: reload failed ({reason}); again in {delay:?}");
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.begin_reload(reason, now, delay, attempt, node, out);
                }
            }
            RecoveryAction::GiveUp(why) => {
                tracing::error!("plugin {node:?}: reload failed ({reason}); giving up: {why}");
                gave_up.push((node, why));
            }
            RecoveryAction::Wait => {}
        }
    }

    /// A plugin's node after the plugin ended for good (it died with recovery
    /// off, or recovery gave up). The node of a loaded plugin stays at its
    /// key, silent in the plugin's place, and its handle reads lost
    /// ([`PluginCause`]) until a swap ([`Session::swap_plugin`]) brings a plugin to
    /// it; any other plugin's node goes.
    fn end_node(&mut self, node: NodeKey, out: &mut Vec<SessionEvent>) {
        if self
            .graph
            .plugins
            .get(&node)
            .is_some_and(|hosted| hosted.link.is_some())
        {
            self.silence_node(node, out);
        } else {
            let _ = self.remove(node); // gone already is as good as removed
        }
    }

    /// Puts the silent stand-in at `node`, which releases the dead plugin on
    /// the control side. A graph that is re-preparing is asked again at the
    /// next phase. A graph that refuses is not asked again: the refusal is
    /// reported once ([`PluginChange::NodeNotSilenced`]) and the node is not
    /// [`Silenced`](Silence::Silenced).
    fn silence_node(&mut self, node: NodeKey, out: &mut Vec<SessionEvent>) {
        let silence = match self.graph.silence_plugin(node) {
            Ok(true) => Silence::Silenced,
            Ok(false) => return,
            Err(why) => {
                tracing::warn!("plugin {node:?}: its node could not be silenced: {why}");
                out.push(event(node, PluginChange::NodeNotSilenced { why }));
                Silence::Refused
            }
        };
        if let Some(hosted) = self.graph.plugins.get_mut(&node) {
            hosted.silence = silence;
        }
    }

    fn finish_gave_up(&mut self, gave_up: Vec<(NodeKey, GiveUp)>, out: &mut Vec<SessionEvent>) {
        for (node, why) in gave_up {
            // The last state worth offering a user: what was withheld, else
            // the last snapshot. The record goes with the node.
            let state = self
                .graph
                .plugins
                .get(&node)
                .and_then(|h| h.withheld.clone().or_else(|| h.snapshot.clone()));
            if let Some(link) = self.graph.plugins.get(&node).and_then(|h| h.link.as_ref()) {
                link.gave_up(why.clone());
            }
            out.push(event(
                node,
                PluginChange::GaveUp {
                    why,
                    state: state.map(Box::new),
                },
            ));
            self.end_node(node, out);
        }
    }

    /// One phase's work on a plugin being replaced: install a replacement
    /// that was waiting, count a dropped or overdue task as failed, or hand
    /// the load out when its backoff has passed.
    fn drive_reload(
        &mut self,
        node: NodeKey,
        now: Instant,
        out: &mut Vec<SessionEvent>,
        gave_up: &mut Vec<(NodeKey, GiveUp)>,
    ) {
        let rate = self.graph.prepared().sample_rate();
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        if let Some(ready) = hosted.reload.ready.take() {
            self.install_reload(node, ready, now, out);
            return;
        }
        if hosted.reload.in_flight {
            // A load queued behind a full pool has not begun: its deadline starts when its
            // task does.
            if !hosted
                .reload
                .started
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            if !std::mem::replace(&mut hosted.reload.clock_started, true)
                && let Some(recovery) = hosted.recovery_mut()
            {
                recovery.start_clock(now);
            }
        }
        let (tick, load_timeout) = hosted
            .recovery_mut()
            .map_or((RecoveryAction::Wait, Duration::ZERO), |r| {
                (r.step(RecoveryEvent::Tick, now), r.policy().load_timeout)
            });
        if !matches!(tick, RecoveryAction::Wait) {
            // The attempt ran out of time.
            let reason = ReloadCause::Attempt(RecoveryFailure::ReloadTimedOut {
                after: load_timeout,
            });
            self.apply_failure(node, tick, reason, now, out, gave_up);
            return;
        }
        let (Some(origin), Some(due)) = (
            hosted.reloadable.as_ref().map(|r| r.origin.clone()),
            hosted.reload.due,
        ) else {
            return;
        };
        if hosted.reload.in_flight || now < due {
            return;
        }
        hosted.reload.due = None;
        hosted.reload.in_flight = true;
        let timeouts = origin.config.timeouts;
        let load = PluginLoad {
            loadable: origin.loadable.retargeted(hosted.plugin.clone()),
            rate,
            config: origin.config,
            state: hosted.snapshot.clone().map(Arc::new),
            work: self.plugins.work.clone(),
            executor: self.plugins.executor.clone(),
        };
        let token = Arc::clone(&hosted.reload.token);
        let snapshot_age = hosted
            .snapshot_from
            .map(|t| now.saturating_duration_since(t));
        let started = Arc::clone(&hosted.reload.started);
        let post = self
            .plugins
            .inbox
            .post()
            .reload(node, token, snapshot_age, started);
        // blocks: a plugin load is an IPC round trip, bounded by the load deadlines. The task
        // holds the post and the load's weak executor, never an executor. A refused task is
        // dropped unrun, which ends the reload as dropped through its post.
        let _ = self.plugins.spawn(Loop::Reload, &timeouts, move || {
            post.run(move || load.run());
        });
    }

    /// The reload's task ended without a result (it was dropped, or it panicked): a
    /// failed attempt, unless the attempt no longer applies.
    pub(crate) fn reload_ended(
        &mut self,
        node: NodeKey,
        token: &Arc<()>,
        cause: RecoveryFailure,
        out: &mut Vec<SessionEvent>,
    ) {
        let now = self.plugins.now;
        let Some(hosted) = self.graph.plugins.get(&node) else {
            return;
        };
        if !Arc::ptr_eq(token, &hosted.reload.token)
            || !matches!(hosted.status, PluginLiveness::Recovering { .. })
        {
            return;
        }
        let mut gave_up = Vec::new();
        self.fail_reload(node, cause, now, out, &mut gave_up);
        self.finish_gave_up(gave_up, out);
    }

    /// The render mode last announced to the hosted plugins.
    pub fn render_mode(&self) -> tutti_plugin_types::RenderMode {
        self.plugins.render_mode.announced
    }

    /// `Phase::Plugins`. See the module docs.
    pub(crate) fn plugins_phase(&mut self, cx: crate::PhaseCx, out: &mut Vec<SessionEvent>) {
        self.graph.graveyard.warn_if_piling();
        self.graph.reconcile_own_bypass();
        let now = cx.now;
        self.plugins.now = now;
        // Before what is due is decided: a landed snapshot frees its plugin for the next.
        self.land_arrivals(out);
        let interval = self.plugins.snapshot_interval;
        let hang_policy = self.plugins.hang;
        let values = self.graph.value_table();
        let mut dead = Vec::new();
        let mut gave_up = Vec::new();
        // Nodes of plugins that are gone whose silent stand-in the graph has
        // not taken yet (it was re-preparing).
        let mut unsilenced = Vec::new();
        let nodes: Vec<NodeKey> = self.graph.plugins.keys().copied().collect();
        for node in nodes {
            let Some(hosted) = self.graph.plugins.get_mut(&node) else {
                continue;
            };
            if matches!(hosted.status, PluginLiveness::Dead { .. }) {
                if hosted.link.is_some() && hosted.silence == Silence::Pending {
                    unsilenced.push(node);
                }
                continue;
            }
            // What the plugin reported since the last update, in the order
            // it reported it.
            let heard = std::mem::take(&mut *hosted.notices.lock());
            // A rescan the plugin announced re-reads what it declares and the
            // value of each, as a task below.
            if heard.iter().any(|notice| {
                matches!(
                    notice,
                    PluginSignal::Refresh(PluginRefresh::ParamValues | PluginRefresh::ParamTitles)
                )
            }) {
                hosted.reread_params();
            }
            // A table the plugin re-declared is read now, where the main
            // thread is at hand; the handle answers the load's copy when it
            // has no route to the plugin.
            let redeclared = heard.iter().any(|notice| {
                matches!(
                    notice,
                    PluginSignal::Invalidate(PluginInvalidation::MidiMap)
                )
            });
            if let (true, Some(main)) = (redeclared, cx.main) {
                hosted.assignments = hosted.handle.wiring(main).assignments;
            }
            out.extend(
                heard
                    .into_iter()
                    .filter_map(change_of)
                    .map(|change| event(node, change)),
            );
            let cause = match hosted.handle.status() {
                PluginStatus::Dead { cause } => Some(cause),
                _ => None,
            };
            if let (Some(cause), Some(link)) = (&cause, hosted.link.as_ref()) {
                link.died(cause.clone());
            }
            if matches!(hosted.status, PluginLiveness::Recovering { .. }) {
                self.drive_reload(node, now, out, &mut gave_up);
                continue;
            }
            if let Some(main) = cx.main {
                hosted.track.deliver(&hosted.handle, main);
                hosted.midi_map.deliver(&hosted.handle, main);
            }
            // The writes the plugin could not answer for are sent again, and the
            // ones it refused by name are told.
            out.extend(
                hosted
                    .writes
                    .resend(&hosted.handle)
                    .into_iter()
                    .map(|refusal| {
                        event(
                            node,
                            match refusal {
                                crate::value::WriteRefusal::Undeclared(id) => {
                                    PluginChange::ParamUndeclared { id }
                                }
                                crate::value::WriteRefusal::ReadOnly(id) => {
                                    PluginChange::ParamReadOnly { id }
                                }
                            },
                        )
                    }),
            );
            match hosted.liveness.observe(cause.is_none(), now) {
                LivenessVerdict::Healthy => hosted.status = PluginLiveness::Healthy,
                LivenessVerdict::Failing { .. } => hosted.status = PluginLiveness::Failing,
                LivenessVerdict::Dead => {
                    // `Liveness` declares a death only on an unhealthy
                    // observation, and every unhealthy one carries the
                    // engine's latched cause.
                    let cause = cause.unwrap_or(PluginFailure::Unreported);
                    let latency = self.graph.node_latency(node).unwrap_or_default();
                    let hosted = self
                        .graph
                        .plugins
                        .get_mut(&node)
                        .expect("looked up above, and nothing between removes it");
                    if hosted.probation_until.is_some_and(|t| now < t) {
                        tracing::warn!(
                            "plugin {node:?} died soon after restoring its snapshot; the next \
                             load goes without it"
                        );
                        hosted.withhold_snapshot(now);
                    }
                    let action = hosted.recovery_mut().map(|r| {
                        r.step(
                            RecoveryEvent::Died {
                                cause: cause.clone(),
                            },
                            now,
                        )
                    });
                    match action {
                        Some(RecoveryAction::Reload { delay, attempt }) => {
                            tracing::error!(
                                "plugin '{}' declared dead ({cause}); reloading {node:?} in \
                                 {delay:?}",
                                hosted.handle.name()
                            );
                            hosted.reload.latency_before = latency;
                            hosted.begin_reload(
                                ReloadCause::Died(cause),
                                now,
                                delay,
                                attempt,
                                node,
                                out,
                            );
                            continue;
                        }
                        Some(RecoveryAction::GiveUp(why)) => {
                            tracing::error!(
                                "plugin '{}' declared dead ({cause}); giving up on {node:?}: {why}",
                                hosted.handle.name()
                            );
                            hosted.status = PluginLiveness::Dead { cause };
                            gave_up.push((node, why));
                            continue;
                        }
                        _ => {}
                    }
                    tracing::error!(
                        "plugin '{}' declared dead ({cause}); removing {node:?}",
                        hosted.handle.name()
                    );
                    hosted.status = PluginLiveness::Dead {
                        cause: cause.clone(),
                    };
                    dead.push((node, cause));
                    continue;
                }
            }
            // A plugin that is alive but never returns from `process()` is
            // ended here, and dies by the path above.
            if cause.is_none()
                && let counts = hosted.handle.block_counts()
                && let Some(reason) =
                    hosted
                        .hang
                        .observe(hang_policy, counts.abandoned, counts.replies, now)
            {
                tracing::error!(
                    "plugin '{}' ({node:?}): {reason}; ending its server",
                    hosted.handle.name()
                );
                hosted.handle.terminate(reason);
            }

            // The figure its node declares now, onto the graph when it moved.
            let declared = hosted
                .controls
                .as_ref()
                .map(PluginControls::declared_latency);
            // Likewise its tail claim (`refresh_node_tail` turns it into the
            // ceiling the node's shape declares, at the graph's rate): a
            // tail-changed callback re-declares it, and the next commit moves
            // the plan and the silence skip to it.
            let declared_tail = hosted.controls.as_ref().map(PluginControls::tail);
            let healthy = matches!(hosted.status, PluginLiveness::Healthy);
            let due = healthy && Arc::strong_count(&hosted.snapshot_token) == 1 && {
                let since = *hosted.snapshot_at.get_or_insert(now);
                now.saturating_duration_since(since) >= interval
            };
            let snapshot = due.then(|| {
                hosted.snapshot_at = Some(now);
                (
                    hosted.handle.clone(),
                    Arc::clone(&hosted.snapshot_token),
                    hosted.timeouts(),
                    hosted.panicked_round_trips.writer(),
                )
            });

            if let Some(read) = hosted.param_read.take() {
                let token = Arc::clone(&hosted.mirror_token);
                let post = self.plugins.inbox.post();
                let timeouts = hosted.timeouts();
                let panics = hosted.panicked_round_trips.writer();
                // The read is awaited on the executor: the plugin's replies wake it, and
                // no thread waits for them (each query is bounded by its own deadline).
                let started = self
                    .plugins
                    .spawn_future(Loop::ParamRead, &timeouts, async move {
                        let mut reading = Box::pin(read.into_future());
                        let values = std::future::poll_fn(|cx| {
                            match contained("parameter read", node, &panics, || {
                                reading.as_mut().poll(cx)
                            }) {
                                Some(polled) => polled.map(Some),
                                None => Poll::Ready(None),
                            }
                        })
                        .await;
                        match values {
                            Some(values) => post.params(node, token, values),
                            None => post.params_lost(node, token),
                        }
                    });
                if !started {
                    // The read went with the refused task: ask the plugin for a fresh one.
                    hosted.reread_params();
                }
            }
            // A value the plugin reported or the host set goes into the knob
            // of the lane driving that parameter, if one does.
            let driven = &hosted.driven;
            let mut applied = Vec::new();
            hosted.mirror.drain_edits(|id, value| {
                driven.write(id, value);
                applied.push((id, value));
            });
            values.note_plugin_values(node, &applied);

            if let Some(tail) = declared_tail {
                self.graph.refresh_node_tail(node, tail);
            }
            if let Some(declared) = declared
                && self.graph.refresh_node_latency(node, declared)
            {
                out.push(event(
                    node,
                    PluginChange::LatencyChanged {
                        latency: self.graph.node_latency(node).unwrap_or_default(),
                    },
                ));
            }
            if let Some((handle, token, timeouts, panics)) = snapshot {
                let asked = now;
                let post = self.plugins.inbox.post();
                // blocks: the state save is an IPC round trip, bounded by `state_wait`.
                // A refused task drops its token, which frees the plugin for the next ask.
                let _ = self.plugins.spawn(Loop::Snapshot, &timeouts, move || {
                    if let Some(state) = contained("state snapshot", node, &panics, move || {
                        handle.save_state(StateUse::Project)
                    }) {
                        post.snapshot(node, token, asked, state);
                    }
                });
            }
        }
        for node in unsilenced {
            self.silence_node(node, out);
        }
        for (node, cause) in dead {
            out.push(event(node, PluginChange::Died { cause }));
            self.end_node(node, out);
        }
        self.finish_gave_up(gave_up, out);
    }

    /// `Phase::RenderMode`: Offline while an export token lives, Realtime
    /// otherwise, told per `needs_notify`. Without `export` nothing
    /// is ever announced: there is no bounce to announce.
    pub(crate) fn render_mode_phase(&mut self) {
        #[cfg(feature = "export")]
        {
            let wanted = if self.bounce.in_flight() {
                tutti_plugin_types::RenderMode::Offline
            } else {
                tutti_plugin_types::RenderMode::Realtime
            };
            let changed = self.plugins.render_mode.announced != wanted;
            for hosted in self.graph.plugins.values_mut() {
                if !tutti_plugin_host::needs_notify(changed, hosted.told) {
                    continue;
                }
                // `false` means the plugin declined (a CLAP plugin without
                // `clap.render`): it renders the same either way, so there is
                // nothing to retry.
                let _ = hosted.handle.announce_render_mode(wanted);
                hosted.told = true;
            }
            self.plugins.render_mode.announced = wanted;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The record's own rules over a fake backend (no `plugin-server`):
    //! the snapshot cadence and its one-in-flight token, a declined save,
    //! and a failing plugin's skipped snapshot. A task ends in the plugin inbox, which a
    //! test waits on (no sleep) before the update that lands it. The death, the render mode
    //! and the export are pinned end to end over the CLAP test plugin, in
    //! `tests/plugin/plugin.rs`.

    use super::*;
    use crate::RecoveryPolicy;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tutti_platform::Executor;
    use tutti_plugin_types::{ParameterInfo, Timeouts};
    use tutti_units::Normalized;
    use tutti_units::ParamId;

    /// **A plugin load that is queued and never run keeps no executor alive**:
    /// the load reaches the executor through the session part's weak handle, so
    /// with every strong handle dropped the executor shuts down while the load
    /// is still in its queue.
    #[test]
    fn a_queued_plugin_load_does_not_keep_the_executor_alive() {
        let executor = Executor::new();
        let (part, _early) = crate::session::parts::PluginPart::new(
            &crate::session::parts::PluginOptions::default(),
            &executor,
        );
        let (_collector, work) = tutti_plugin_types::MainWork::new();
        let load = PluginLoad {
            loadable: Loadable::uncatalogued(PluginRef::file("queued.vst3")),
            rate: SampleRate::SR_48K,
            config: AudioConfig::default(),
            state: None,
            work,
            executor: part.executor.clone(),
        };
        // Queued, and nothing drives the executor: the load waits in its queue.
        let queued = executor.spawn(async move {
            let _load = load;
            std::future::pending::<()>().await;
        });
        let weak = executor.downgrade();

        drop(executor);

        assert!(
            weak.upgrade().is_none(),
            "a queued load kept the executor alive"
        );
        drop(queued);
    }

    /// **No plugin task holds an executor.** A task of each loop, held in its round trip,
    /// leaves the executor to shut down when its last strong handle drops: the part keeps a
    /// weak handle and `spawn` upgrades it for the call only.
    #[test]
    fn no_plugin_task_holds_an_executor() {
        for kind in [Loop::Reload, Loop::ParamRead, Loop::Snapshot] {
            let executor = Executor::new();
            let (part, _early) = crate::session::parts::PluginPart::new(
                &crate::session::parts::PluginOptions::default(),
                &executor,
            );
            let (release, hold) = std::sync::mpsc::channel::<()>();
            assert!(part.spawn(kind, &Timeouts::default(), move || {
                let _ = hold.recv_timeout(HANG_GUARD);
            }));
            assert_eq!(part.tasks.in_flight(), 1, "{kind:?}: queued or running");
            let weak = executor.downgrade();
            drop(executor);
            assert!(
                weak.upgrade().is_none(),
                "a {kind:?} task kept the executor alive"
            );
            // The executor's shutdown may have dropped the task unrun, which ended it too.
            let _ = release.send(());
            assert!(part.tasks.wait_idle(None), "{kind:?}: the task ended");
        }
    }

    /// **A loop's bound is the plugin's own configured timeout.** A close waits for a
    /// reload as long as its `load` timeout, a read its `param` and a snapshot its
    /// `state`, as the plugin was loaded with (`Origin::config`), the defaults only for a
    /// plugin with no origin: an app that raised a timeout is not given up on early.
    #[test]
    fn a_loops_bound_is_the_plugins_configured_timeout() {
        let mut timeouts = Timeouts::default();
        timeouts.load = Duration::from_secs(101);
        timeouts.param = Duration::from_secs(102);
        timeouts.state = Duration::from_secs(103);
        assert_eq!(Loop::Reload.deadline(&timeouts), timeouts.load);
        assert_eq!(Loop::ParamRead.deadline(&timeouts), timeouts.param);
        assert_eq!(Loop::Snapshot.deadline(&timeouts), timeouts.state);

        let mut rig = rig(None);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        assert_eq!(hosted.timeouts(), Timeouts::default(), "no origin");
        hosted.reloadable = Some(Reloadable {
            origin: Origin {
                config: AudioConfig::default().timeouts(timeouts),
                loadable: Loadable::uncatalogued(PluginRef::file("fake.clap")),
            },
            recovery: None,
        });
        assert_eq!(hosted.timeouts(), timeouts);
    }

    /// **A task still queued when a close begins never runs.** The pool is full, so the
    /// snapshot waits in its queue; a piece of main-thread work frees a pool thread during
    /// the close, after it was marked begun, and the freed thread takes the snapshot: it
    /// ends at its first step, unrun (no save was asked), and the plugin's close reaches
    /// the main thread with nothing stranded.
    #[test]
    fn a_task_queued_when_a_close_begins_never_runs() {
        let mut rig = in_process_rig(Some(vec![1]));
        let executor = rig.executor.upgrade().expect("the session's executor");
        let held: Vec<_> = (0..32)
            .map(|_| {
                let (release, hold) = std::sync::mpsc::channel::<()>();
                let task = executor
                    .unblock(move || {
                        let _ = hold.recv_timeout(HANG_GUARD);
                    })
                    .expect("a pool thread starts");
                (release, task)
            })
            .collect();
        drop(executor);
        let _ = rig.events_at(0);
        let _ = rig.events_at(1_000);
        assert_eq!(rig.session.plugin_tasks(), 1, "the snapshot is queued");

        // The close runs this on the main thread, once it has begun: it frees a thread and
        // waits for the queued snapshot to end, which only its first step can do quickly.
        let tasks = rig.session.plugins.tasks.clone();
        let (first, _) = &held[0];
        let first = first.clone();
        rig.session.plugins.work.defer(move |_| {
            let _ = first.send(());
            let guard = tutti_platform::Instant::now() + HANG_GUARD;
            while tasks.in_flight() > 0 && tutti_platform::Instant::now() < guard {
                std::thread::yield_now();
            }
        });
        let closed = Arc::clone(&rig.closed);
        let saves = Arc::clone(&rig.saves);
        let stranded = tutti_plugin_types::MainWork::stranded();
        rig.session.close(tutti_fixtures::main_thread());
        for (release, task) in held {
            let _ = release.send(());
            task.detach();
        }
        assert_eq!(saves.load(Ordering::SeqCst), 0, "the snapshot never ran");
        assert!(closed.load(Ordering::SeqCst), "the plugin's close ran");
        assert_eq!(tutti_plugin_types::MainWork::stranded(), stranded);
    }

    /// **A refused spawn is counted, starts nothing and leaves no task in flight.**
    #[test]
    fn a_refused_spawn_is_counted_and_leaves_nothing_in_flight() {
        let executor = Executor::new();
        let (part, _early) = crate::session::parts::PluginPart::new(
            &crate::session::parts::PluginOptions::default(),
            &executor,
        );
        drop(executor);
        for refused in 1..=3 {
            assert!(
                !part.spawn(Loop::Snapshot, &Timeouts::default(), || unreachable!(
                    "the task never runs"
                ))
            );
            assert_eq!(part.tasks.refused(), refused);
        }
        assert_eq!(part.tasks.in_flight(), 0);
    }

    /// **A close waits for an in-process plugin's snapshot, so its close reaches the main
    /// thread and nothing is stranded.** The snapshot task holds a clone of the plugin's
    /// handle and waits on the main thread (two pieces of work, the second handed over by
    /// the first): the close pumps the main thread's work while it waits, the task ends
    /// and drops its clone, and the record's drop is the last, handing the close over.
    #[test]
    fn a_close_waits_for_an_in_process_snapshot_so_nothing_is_stranded() {
        let mut rig = in_process_rig(Some(vec![1]));
        let _ = rig.events_at(0);
        let _ = rig.events_at(1_000);
        assert_eq!(rig.session.plugin_tasks(), 1, "the snapshot is in flight");
        let weak = rig.executor.clone();
        let closed = Arc::clone(&rig.closed);
        let stranded = tutti_plugin_types::MainWork::stranded();
        rig.session.close(tutti_fixtures::main_thread());
        assert!(closed.load(Ordering::SeqCst), "the plugin's close ran");
        assert_eq!(
            tutti_plugin_types::MainWork::stranded(),
            stranded,
            "nothing was stranded"
        );
        assert!(
            weak.upgrade().is_none(),
            "the executor ended with the session"
        );
    }

    /// **A reload queued behind a full blocking pool is not timed out before it starts.**
    /// The recovery's load deadline counts from the instant the task begins to run (it
    /// raises a flag), not from the spawn: 32 held closures fill the pool, the reload waits
    /// in its queue far past the deadline with no failure reported, and runs once a thread
    /// frees.
    #[cfg(feature = "clap")]
    #[test]
    fn a_reload_queued_behind_a_full_pool_does_not_time_out_before_it_starts() {
        let mut rig = rig(None);
        let executor = rig.executor.upgrade().expect("the session's executor");
        let held: Vec<_> = (0..32)
            .map(|_| {
                let (release, hold) = std::sync::mpsc::channel::<()>();
                let task = executor
                    .unblock(move || {
                        let _ = hold.recv_timeout(HANG_GUARD);
                    })
                    .expect("a pool thread starts");
                (release, task)
            })
            .collect();
        drop(executor);
        rig.recovering_in_process();
        let events = rig.events_at(0);
        assert!(events.is_empty());
        assert_eq!(rig.session.plugin_tasks(), 1, "the reload is queued");
        let ten_minutes = rig.events_at(600_000);
        assert!(
            ten_minutes.is_empty(),
            "a reload that has not started timed out: {ten_minutes:?}"
        );
        for (release, task) in held {
            release.send(()).expect("the closure waits");
            task.detach();
        }
        rig.settle();
        let landed = rig.land();
        assert!(
            matches!(
                &landed[..],
                [SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading {
                        cause: ReloadCause::Attempt(RecoveryFailure::ReplacementInProcess),
                        ..
                    },
                    ..
                })]
            ),
            "{landed:?}"
        );
    }

    /// **A parameter read the executor refused is asked for again.** The read went with the
    /// refused task, so the record makes a fresh one; the next phase, with the executor
    /// back, runs it and its values land.
    #[test]
    fn a_refused_parameter_read_is_asked_again() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (incoming, _) = fake_handle(None, vec![(param, 0.75)]);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming, Placement::InProcess);
        let live = rig.session.plugins.executor.clone();
        rig.session.plugins.executor = Executor::new().downgrade();
        let _ = rig.events_at(0);
        assert_eq!(rig.session.plugin_task_refusals(), 1);
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_some(),
            "the read is kept to run again"
        );
        rig.session.plugins.executor = live;
        let _ = rig.events_at(10);
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(param),
            Some(Some(0.75))
        );
    }

    /// **A parameter read whose plugin dropped its answer is asked for again.** The
    /// plugin's round trip panics when the test's end of its hold goes, which drops the
    /// answer's sender: the read resolves, as an unavailable list, to the read itself, and
    /// the landing makes the record read afresh (a new token), where the read would
    /// otherwise be lost for good.
    #[test]
    fn a_parameter_read_whose_answer_was_dropped_is_asked_again() {
        let mut rig = rig(None);
        let (incoming, release) = held_handle(vec![(ParamId::new(3), 0.5)]);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming, Placement::InProcess);
        let _ = rig.events_at(0);
        drop(release);
        rig.settle();
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_none(),
            "the read was consumed by the task"
        );
        let _ = rig.land();
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_some(),
            "the lost read is made again"
        );
    }

    /// **A parameter read whose poll panicked is given up, and reported.** The backend fails
    /// inside the call that queues its question, which runs in the read's poll on the
    /// executor: the panic is contained and the task sends that the read was lost. The
    /// session reports [`PluginChange::ParamsUnread`] once and makes no second read, so a
    /// plugin that panics at every question is not read for ever; the mirror stays unread.
    #[test]
    fn a_parameter_read_whose_poll_panicked_is_given_up_and_reported() {
        let mut rig = rig(None);
        let incoming = PluginHandle::from_backend(
            Arc::new(Fake {
                state: None,
                saves: Arc::default(),
                dead: Arc::default(),
                params: vec![(ParamId::new(3), 0.5)],
                unlisted: Arc::default(),
                hold: None,
                panics: true,
                main: None,
            }),
            None,
            tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
            Default::default(),
            Default::default(),
        );
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming, Placement::InProcess);
        let _ = rig.events_at(0);
        rig.settle();
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_none(),
            "the read was consumed by the task"
        );
        let is_unread = |e: &SessionEvent| {
            matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::ParamsUnread,
                    ..
                })
            )
        };
        let landed = rig.land();
        assert!(
            landed.iter().any(is_unread),
            "the give-up is reported: {landed:?}"
        );
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_none(),
            "and no second read is made"
        );
        let later = rig.events_at(20);
        rig.settle();
        let later_landed = rig.land();
        assert!(
            !later.iter().chain(&later_landed).any(is_unread),
            "reported once"
        );
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_none(),
            "still no second read after a later update"
        );
    }

    /// **With two reads in flight across a crossfade, only the new one lands.** The first
    /// plugin's read is held, the second crossfade's read is released and lands; the first
    /// then ends, and lands nowhere.
    #[test]
    fn with_two_reads_in_flight_only_the_new_one_lands() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (first, release_first) = held_handle(vec![(param, 0.25)]);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(first, Placement::InProcess);
        let _ = rig.events_at(0);
        let (second, release_second) = held_handle(vec![(param, 0.75)]);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(second, Placement::InProcess);
        let _ = rig.events_at(10);
        assert_eq!(rig.session.plugin_tasks(), 2, "both reads are in flight");

        // The reads list their parameters first and read each value after: a token each.
        for _ in 0..2 {
            let _ = release_second.send(());
        }
        // Wait for the second to end by landing until its values are in.
        let guard = tutti_platform::Instant::now() + HANG_GUARD;
        while rig.session.graph.plugins[&rig.node].param_value(param) != Some(Some(0.75)) {
            assert!(
                tutti_platform::Instant::now() < guard,
                "the new read never landed"
            );
            let _ = rig.land();
            std::thread::yield_now();
        }
        for _ in 0..2 {
            let _ = release_first.send(());
        }
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(param),
            Some(Some(0.75)),
            "the old plugin's read landed nowhere"
        );
    }

    /// The controls of a plugin's node, counting nothing yet.
    fn node_controls() -> NodeControls {
        (*PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K).0).clone()
    }

    /// A backend that answers `state` (or declines), counts its saves, and
    /// is dead while `dead` is set.
    struct Fake {
        state: Option<Vec<u8>>,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        /// The parameters it lists, and their values.
        params: Vec<(ParamId, f32)>,
        /// While set, its list is unavailable (`parameter_descriptors` is
        /// `None`), as an in-process plugin's is when its main thread does
        /// not answer.
        unlisted: Arc<AtomicBool>,
        /// While it holds one, a state save or a parameter list waits for a token on it:
        /// the round trip stays in flight until the test sends one.
        hold: Option<Arc<Mutex<std::sync::mpsc::Receiver<()>>>>,
        /// While set, asking for the parameter list panics (a backend that fails inside
        /// the call that queues the question).
        panics: bool,
        /// Models an in-process plugin: a state save waits for the main thread to run two
        /// pieces of its work, the second handed over by the first, and the plugin's close
        /// is handed to the collector, setting the flag when the main thread runs it.
        main: Option<(tutti_plugin_types::MainWorkHandle, Arc<AtomicBool>)>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            if let Some((work, closed)) = self.main.take() {
                work.defer(move |_| closed.store(true, Ordering::SeqCst));
            }
        }
    }

    /// The longest a test waits for an event it started: a guard against a hang, never a
    /// measure of speed.
    const HANG_GUARD: Duration = Duration::from_secs(60);

    impl Fake {
        /// What the plugin lists, with no round trip to wait for.
        fn listing(&self) -> Option<Vec<ParameterInfo>> {
            if self.unlisted.load(Ordering::SeqCst) {
                return None;
            }
            Some(
                self.params
                    .iter()
                    .map(|&(id, _)| ParameterInfo::new(id, "p"))
                    .collect(),
            )
        }

        /// Waits for the test to release the round trip, when it holds them.
        fn held(&self) {
            if let Some((work, _)) = &self.main {
                let (done, ran) = std::sync::mpsc::channel();
                let again = work.clone();
                work.defer(move |_| {
                    again.defer(move |_| {
                        let _ = done.send(());
                    })
                });
                ran.recv_timeout(HANG_GUARD)
                    .expect("the main thread ran the call");
            }
            if let Some(hold) = &self.hold {
                hold.lock()
                    .recv_timeout(HANG_GUARD)
                    .expect("the test released the round trip");
            }
        }
    }

    impl tutti_plugin_types::AnyThread for Fake {
        fn parameter_value(
            &self,
            id: ParamId,
        ) -> tutti_plugin_types::Answer<Option<tutti_plugin_types::PluginValue>> {
            tutti_plugin_types::Answer::ready(self.params.iter().find(|(a, _)| *a == id).map(
                |&(_, v)| tutti_plugin_types::PluginValue::new(Normalized::clamped(f64::from(v))),
            ))
        }
        /// A held list is a round trip in flight: a thread of its own waits for the test's
        /// token and then answers, so no executor thread waits on it. A dropped sender
        /// (the thread panicked when the test's end of the hold went) cancels the answer.
        fn parameter_descriptors(&self) -> tutti_plugin_types::Answer<Option<Vec<ParameterInfo>>> {
            assert!(!self.panics, "the backend failed while asked for its list");
            let Some(hold) = self.hold.clone() else {
                self.held();
                return tutti_plugin_types::Answer::ready(self.listing());
            };
            let listed = self.listing();
            let (tx, answer) = tutti_plugin_types::Answer::channel();
            std::thread::spawn(move || {
                hold.lock()
                    .recv_timeout(HANG_GUARD)
                    .expect("the test released the round trip");
                drop(tx.send(listed));
            });
            answer
        }
        fn set_parameter_value(
            &self,
            _id: ParamId,
            _value: Normalized,
        ) -> Result<(), tutti_plugin_types::ParamWriteError> {
            Ok(())
        }
        fn bypassed(&self, _site: tutti_plugin_types::OwnBypass) -> Option<bool> {
            None
        }
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(
            &self,
            _id: ParamId,
            _plain: f64,
        ) -> tutti_plugin_types::Answer<Option<Normalized>> {
            tutti_plugin_types::Answer::ready(None)
        }
        fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
            None
        }
        fn save_state(
            &self,
            _: tutti_plugin_types::StateUse,
        ) -> Result<tutti_plugin_types::PluginState, tutti_plugin_types::StateError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.held();
            self.state
                .clone()
                .map(tutti_plugin_types::PluginState::new)
                .ok_or(tutti_plugin_types::StateError::NoStateRoute)
        }
        fn set_render_mode(&self, _mode: tutti_plugin_types::RenderMode) -> bool {
            false
        }
        fn set_automation_mode(
            &self,
            _mode: tutti_plugin_types::AutomationMode,
        ) -> Result<(), tutti_plugin_types::AutomationModeError> {
            Err(tutti_plugin_types::AutomationModeError::PluginDead)
        }
    }

    impl tutti_plugin_types::Health for Fake {
        fn is_crashed(&self) -> bool {
            self.dead.load(Ordering::SeqCst)
        }
        fn crash_cause(&self) -> Option<PluginFailure> {
            self.is_crashed().then_some(PluginFailure::StreamClosed)
        }
        fn rate_refusal(&self) -> Option<tutti_plugin_types::RateRefusal> {
            None
        }
        fn block_counts(&self) -> tutti_plugin_types::PipelineCounts {
            tutti_plugin_types::PipelineCounts::default()
        }
        fn refused_edits(&self) -> u64 {
            0
        }

        fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
    }

    struct Rig {
        session: crate::DetachedSession,
        node: NodeKey,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        t0: Instant,
        /// The session's executor, which the test does not keep alive.
        executor: tutti_platform::WeakExecutor,
        /// Releases one held round trip of the plugin, when the rig holds them.
        release: Option<std::sync::mpsc::Sender<()>>,
        /// Set when an in-process rig's plugin is closed on the main thread.
        closed: Arc<AtomicBool>,
    }

    /// A detached session snapshotting every second, hosting a fake that
    /// answers `state` at a dc node.
    fn rig(state: Option<Vec<u8>>) -> Rig {
        build_rig(state, Mode::Plain)
    }

    /// [`rig`], whose plugin's state saves wait for [`Rig::release`].
    fn held_rig(state: Option<Vec<u8>>) -> Rig {
        build_rig(state, Mode::Held)
    }

    /// [`rig`], whose plugin models an in-process one (see `Fake::main`).
    fn in_process_rig(state: Option<Vec<u8>>) -> Rig {
        build_rig(state, Mode::InProcess)
    }

    #[derive(PartialEq)]
    enum Mode {
        Plain,
        Held,
        InProcess,
    }

    fn build_rig(state: Option<Vec<u8>>, mode: Mode) -> Rig {
        let mut session = Session::builder()
            .snapshot_interval(Duration::from_secs(1))
            .build_detached(SampleRate::SR_48K)
            .unwrap();
        let weak = session.plugins.executor.clone();
        let saves = Arc::new(AtomicUsize::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let (release, hold) = if mode == Mode::Held {
            let (tx, rx) = std::sync::mpsc::channel();
            (Some(tx), Some(Arc::new(Mutex::new(rx))))
        } else {
            (None, None)
        };
        let closed = Arc::new(AtomicBool::new(false));
        let main =
            (mode == Mode::InProcess).then(|| (session.plugins.work.clone(), Arc::clone(&closed)));
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                panics: false,
                state,
                saves: Arc::clone(&saves),
                dead: Arc::clone(&dead),
                params: Vec::new(),
                unlisted: Arc::default(),
                hold,
                main,
            }),
            None,
            tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
            Default::default(),
            Default::default(),
        );
        let node =
            session
                .graph()
                .add(tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(
                    0.0,
                )));
        super::fake_hosting::host(&mut session, node, handle, PluginRef::file("fake.clap"));
        let node = node.erase();
        // The load's parameter read is not what these tests count reads of.
        session
            .graph
            .plugins
            .get_mut(&node)
            .expect("hosted")
            .param_read = None;
        Rig {
            session,
            node,
            saves,
            dead,
            t0: Instant::now(),
            executor: weak,
            release,
            closed,
        }
    }

    /// A fake plugin's handle listing `params`, answering `state`, and the
    /// sink its reports go through (firing it is the plugin reporting).
    fn fake_handle(
        state: Option<Vec<u8>>,
        params: Vec<(ParamId, f32)>,
    ) -> (PluginHandle, tutti_plugin_host::ParameterChangeSink) {
        let sink = tutti_plugin_host::ParameterChangeSink::default();
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                panics: false,
                state,
                saves: Arc::default(),
                dead: Arc::default(),
                params,
                unlisted: Arc::default(),
                hold: None,
                main: None,
            }),
            None,
            tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    /// A fake plugin's handle listing `params`, whose parameter list waits for a token on the
    /// returned sender (dropping the sender makes the waiting round trip panic).
    fn held_handle(params: Vec<(ParamId, f32)>) -> (PluginHandle, std::sync::mpsc::Sender<()>) {
        let (release, hold) = std::sync::mpsc::channel();
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                panics: false,
                state: None,
                saves: Arc::default(),
                dead: Arc::default(),
                params,
                unlisted: Arc::default(),
                hold: Some(Arc::new(Mutex::new(hold))),
                main: None,
            }),
            None,
            tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
            Default::default(),
            Default::default(),
        );
        (handle, release)
    }

    impl Rig {
        /// `Phase::Plugins` at `t0 + ms`: every event it reported. What the loops'
        /// tasks sent lands first; what is due starts a task.
        fn events_at(&mut self, ms: u64) -> Vec<SessionEvent> {
            let mut out = Vec::new();
            let cx = crate::PhaseCx::at(self.t0 + Duration::from_millis(ms));
            self.session.run_phase(crate::Phase::Plugins, cx, &mut out);
            out
        }

        /// Waits until no task is in flight: what they sent waits in the inbox for the next
        /// landing.
        fn settle(&self) {
            super::fake_hosting::await_tasks(&self.session);
        }

        /// Lands what the tasks sent, without deciding anything is due: the events it
        /// reported.
        fn land(&mut self) -> Vec<SessionEvent> {
            let mut out = Vec::new();
            self.session.plugins.now = self.t0;
            self.session.land_arrivals(&mut out);
            out
        }

        /// Lets one held round trip of the plugin return.
        fn release(&self) {
            self.release
                .as_ref()
                .expect("a held rig")
                .send(())
                .expect("the plugin holds the other end");
        }

        /// The tokens on the node's snapshot: the record's, and one per ask in flight.
        fn snapshot_holders(&self) -> usize {
            Arc::strong_count(&self.session.graph.plugins[&self.node].snapshot_token)
        }

        /// The tokens on the node's mirror: the record's, and one per read in flight.
        fn mirror_holders(&self) -> usize {
            Arc::strong_count(&self.session.graph.plugins[&self.node].mirror_token)
        }

        /// The node's record as a plugin being recovered, its reload due now,
        /// whose load resolves in process (CLAP, `Hosting::InProcess`).
        #[cfg(feature = "clap")]
        fn recovering_in_process(&mut self) {
            let plugin = PluginRef::file("fake.clap");
            self.recovering(
                plugin.clone(),
                Loadable::uncatalogued(plugin).hosting(tutti_plugin_db::Hosting::InProcess),
            );
        }

        /// The node's record as `plugin` being recovered, its reload due now,
        /// from a load that was `loadable`.
        fn recovering(&mut self, plugin: PluginRef, loadable: Loadable) {
            let hosted = self.session.graph.plugins.get_mut(&self.node).unwrap();
            hosted.plugin = plugin;
            let mut recovery = Recovery::new(RecoveryPolicy::default());
            let _ = recovery.step(
                RecoveryEvent::Died {
                    cause: PluginFailure::StreamClosed,
                },
                self.t0,
            );
            hosted.reloadable = Some(Reloadable {
                origin: Origin {
                    config: AudioConfig::default(),
                    loadable,
                },
                recovery: Some(recovery),
            });
            hosted.status = PluginLiveness::Recovering {
                cause: ReloadCause::Died(PluginFailure::StreamClosed),
            };
            hosted.reload.due = Some(self.t0);
        }
    }

    /// **Snapshots are taken on the interval of `now`, not every update,
    /// and one at a time.** The first update starts the clock; updates
    /// inside the interval ask for nothing; the first at or past it asks
    /// once; while that task is in flight nothing more is asked, however late
    /// (the record and the task hold the snapshot token, and no second ask clones it);
    /// landing its end stores the bytes, frees the plugin and restarts the cadence at
    /// the next ask.
    #[test]
    fn snapshots_are_taken_on_the_interval_one_at_a_time() {
        let mut rig = held_rig(Some(vec![1, 2, 3]));
        let _ = rig.events_at(0);
        assert_eq!(
            rig.snapshot_holders(),
            1,
            "the first update starts the clock"
        );
        let _ = rig.events_at(500);
        assert_eq!(rig.snapshot_holders(), 1, "inside the interval");
        let _ = rig.events_at(1_000);
        assert_eq!(rig.snapshot_holders(), 2, "one task at the interval");
        let _ = rig.events_at(5_000);
        assert_eq!(
            rig.snapshot_holders(),
            2,
            "nothing more while the task is in flight"
        );
        assert!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .is_none(),
            "nothing landed yet"
        );

        // The first task ends: its landing stores the bytes, and the plugin, a long
        // interval past its last ask, is asked again in the same phase.
        rig.release();
        rig.settle();
        let _ = rig.events_at(5_000);
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(|saved| saved.state().bytes()),
            Some(&[1u8, 2, 3][..])
        );
        assert_eq!(rig.snapshot_holders(), 2, "a landed task frees it");

        // The second ends, and the third waits for the interval from the second's ask.
        rig.release();
        rig.settle();
        let _ = rig.events_at(5_500);
        assert_eq!(rig.saves.load(Ordering::SeqCst), 2, "one task is one save");
        assert_eq!(rig.snapshot_holders(), 1, "landed, inside the interval");
        let _ = rig.events_at(6_000);
        assert_eq!(rig.snapshot_holders(), 2, "asked again at the interval");
        rig.release();
    }

    /// **A declined save keeps the previous snapshot.** A refusal (the
    /// plugin declines, or the bridge dropped mid-call) replacing a good
    /// recovery point with nothing is worse than a stale one.
    #[test]
    fn a_declined_save_keeps_the_previous_snapshot() {
        let mut rig = rig(None);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.snapshot = Some(Saved::new(PeerState::of_plugin(
            hosted.plugin.clone(),
            vec![4, 2],
        )));
        let _ = rig.events_at(0);
        let _ = rig.events_at(1_000);
        rig.settle();
        assert!(rig.land().is_empty());
        assert_eq!(
            rig.saves.load(Ordering::SeqCst),
            1,
            "the plugin was asked and declined"
        );
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(|saved| saved.state().bytes()),
            Some(&[4u8, 2][..]),
            "a declined save must keep the last good snapshot"
        );
    }

    /// **The update frees a hosted plugin's project-meter cell cleared while a
    /// block held it**: once cleared the node's meter slot follows no project
    /// meter, so only the session's sweep of its plugins' meters reaches what the
    /// clear retired, on the next update with nothing published again.
    #[test]
    fn an_update_frees_a_plugin_meter_cleared_while_a_block_held_it() {
        let mut rig = rig(None);
        let (controls, _) =
            PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K);
        let project = rig.session.transport.settings().project_meter().clone();
        controls.set_meter(project);
        let meter = controls.meter();
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .expect("hosted")
            .node = (*controls).clone();
        assert!(meter.has_meter(), "the project's meter is followed");

        let mut block = meter.probe_reader();
        {
            let _held = block.lend();
            meter.clear_meter();
            let _ = rig.session.update();
            assert_eq!(meter.retired_len(), 1, "held across the clear");
        }
        let _ = rig.session.update();
        assert_eq!(meter.retired_len(), 0, "the update freed it");
    }

    /// **A latency past what PDC compensates is clamped, not refused**: a plugin
    /// that reports more than `MAX_NODE_LATENCY` (the reference plugin cannot: its
    /// delay line is short) reaches the graph as that figure at the next
    /// `Phase::Plugins`.
    #[test]
    fn a_reported_latency_past_what_pdc_compensates_is_clamped() {
        let mut rig = rig(None);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .expect("hosted")
            .controls = Some(
            PluginControls::new(
                Samples::new(10_000_000),
                PluginTail::default(),
                SampleRate::SR_48K,
            )
            .0,
        );
        let _ = rig.events_at(0);
        assert_eq!(
            rig.session.graph_ref().node_latency(rig.node).unwrap(),
            tutti_graph::MAX_NODE_LATENCY.to_samples_ceil(SampleRate::SR_48K)
        );
    }

    /// **A failing plugin is not asked for its state**: the call would
    /// block against a peer that is not answering, and return nothing.
    #[test]
    fn a_failing_plugin_is_not_snapshotted() {
        let mut rig = rig(Some(vec![1]));
        let _ = rig.events_at(0);
        rig.dead.store(true, Ordering::SeqCst);
        let _ = rig.events_at(1_000);
        assert_eq!(rig.snapshot_holders(), 1, "failing: no snapshot task");
        assert_eq!(rig.saves.load(Ordering::SeqCst), 0, "failing: no save");
        assert!(matches!(
            rig.session.plugin_view(rig.node).unwrap().health(),
            PluginHealth::Died { .. }
        ));
    }

    /// **A hosted plugin replaced by a native node is no longer hosted**
    /// (`Graph::replace`): its record goes, so a later death of the outgoing
    /// plugin removes nothing (the node is the native one's now) and reports
    /// nothing, and the record's handle no longer keeps the outgoing plugin
    /// alive.
    #[test]
    fn a_plugin_replaced_by_a_native_node_is_no_longer_hosted() {
        let mut rig = rig(None);
        let replacement = tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(1.0));
        let cut = tutti_graph::Fade::new(tutti_units::Samples::new(0));
        assert!(
            rig.session
                .graph()
                .replace(rig.node, crate::Replacement::new(replacement).fade(cut))
                .is_ok()
        );
        assert!(
            rig.session.plugin_view(rig.node).is_err(),
            "the record went"
        );
        // The outgoing plugin dies, past the debounce's count and window.
        rig.dead.store(true, Ordering::SeqCst);
        for ms in [0, 100, 200, 300, 400] {
            let died = rig.events_at(ms).into_iter().any(|e| {
                matches!(
                    e,
                    SessionEvent::Plugin(PluginEvent {
                        change: PluginChange::Died { .. },
                        ..
                    })
                )
            });
            assert!(!died, "a native node is reported dead");
        }
        assert!(
            rig.session.graph_ref().contains(rig.node),
            "the native replacement stays in the graph"
        );
    }

    /// **A swap ends the outgoing plugin's editor and closes it** when the main
    /// thread empties the graveyard: `adopt` installs the incoming plugin's
    /// handle, whose editor is closed, and the outgoing handle's editor is closed
    /// first, not left to the drop. The editor reads closed, and its calls
    /// answer `Ended` rather than act on the incoming plugin. An editor that was
    /// not open is not closed.
    #[test]
    fn a_swap_ends_and_closes_the_outgoing_editor() {
        use tutti_plugin_types::fake::{Call, FakePlugin};
        let main = tutti_fixtures::main_thread();
        for open in [true, false] {
            let (fake, control) = FakePlugin::new();
            fake.with_editor(true);
            let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(control);
            let outgoing = PluginHandle::from_backend(
                Arc::new(Fake {
                    panics: false,
                    state: None,
                    saves: Arc::default(),
                    dead: Arc::default(),
                    params: Vec::new(),
                    unlisted: Arc::default(),
                    hold: None,
                    main: None,
                }),
                Some(control),
                tutti_plugin_types::PluginDescriptor::new(
                    "",
                    tutti_plugin_types::PluginClass::Unknown,
                ),
                Default::default(),
                Default::default(),
            );
            let mut record =
                HostedPlugin::new(outgoing, PluginRef::file("fake.clap"), node_controls());
            let mut yard = Graveyard::default();
            let node = NodeKey::fresh();
            let mut editor = yard.editors.make(node, record.handle.clone(), main);
            if open {
                let _ = editor
                    .open(
                        &tutti_fixtures::StandInWindow,
                        tutti_plugin_types::ContentScale::IDENTITY,
                    )
                    .expect("the fake opens its editor");
            }
            let (incoming, _) = fake_handle(None, Vec::new());
            record
                .adopt(incoming, Placement::InProcess)
                .retire()
                .settle(node, &mut yard);
            assert!(
                !record.handle.is_editor_open(),
                "the incoming plugin has no editor open"
            );
            assert!(!editor.is_open(), "the editor reads closed after the swap");
            assert!(matches!(
                editor.resize(dpi::PhysicalSize::new(1, 1).into()),
                Err(crate::EditorCallError::Ended)
            ));
            assert!(
                !fake.calls().contains(&Call::CloseEditor),
                "not before the main thread"
            );
            yard.exhume(main);
            assert_eq!(
                fake.calls().contains(&Call::CloseEditor),
                open,
                "editor open: {open}"
            );
        }
    }

    /// **A crossfade to another plugin does not inherit the outgoing
    /// plugin's recovery state.** Its last snapshot is cleared, and a
    /// snapshot task of the outgoing plugin still in flight lands nowhere
    /// (its token is not the record's any more); the incoming plugin gets
    /// its own on the interval from the crossfade. Driven through
    /// `HostedPlugin::adopt`, `replace`'s handle half (a crossfade to
    /// another plugin needs a `PluginClient`, a real `plugin-server`).
    #[test]
    fn a_crossfade_to_another_plugin_drops_the_old_snapshot() {
        let mut rig = held_rig(Some(vec![1, 2, 3]));
        let _ = rig.events_at(0);
        let _ = rig.events_at(1_000);
        rig.release();
        rig.settle();
        let _ = rig.events_at(1_000);
        let _ = rig.events_at(2_000);
        assert_eq!(
            rig.snapshot_holders(),
            2,
            "the outgoing plugin's task is in flight"
        );

        let (incoming, _) = fake_handle(Some(vec![9]), Vec::new());
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming, Placement::InProcess);
        // The incoming plugin's parameter read is not what this counts.
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .param_read = None;
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(|saved| saved.state().bytes()),
            None,
            "the outgoing plugin's snapshot is not the incoming one's"
        );
        rig.release();
        rig.settle();
        assert!(rig.land().is_empty());
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(|saved| saved.state().bytes()),
            None,
            "the outgoing plugin's task must not land on the incoming plugin"
        );

        let _ = rig.events_at(2_000);
        assert_eq!(rig.snapshot_holders(), 1, "its clock starts here");
        let _ = rig.events_at(3_000);
        assert_eq!(rig.snapshot_holders(), 2, "its own task");
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(|saved| saved.state().bytes()),
            Some(&[9u8][..])
        );
    }

    /// **A snapshot of a plugin removed since it was asked for lands nowhere.** The task
    /// ends with its bytes after the node's record is gone: no event, nothing stored.
    #[test]
    fn a_snapshot_of_a_removed_plugin_lands_nowhere() {
        let mut rig = held_rig(Some(vec![1]));
        let _ = rig.events_at(0);
        let _ = rig.events_at(1_000);
        rig.session.remove(rig.node).expect("the node is removed");
        assert!(
            rig.session.plugin_view(rig.node).is_err(),
            "its record is gone"
        );
        rig.release();
        rig.settle();
        assert!(
            rig.land().is_empty(),
            "nothing is reported for a node that is gone"
        );
    }

    /// **A crossfade to another plugin rebuilds the parameter mirror from
    /// the incoming plugin, read off the control thread.** Right after the
    /// crossfade the mirror is not ready (a lane waits rather than start
    /// from the outgoing plugin's value); the next `Phase::Plugins` starts
    /// the read as a task on the session's executor, once; the task runs on another thread
    /// and its values land in a later update. A read asked for before a
    /// second crossfade lands nowhere. The read goes through the incoming
    /// handle, which `PluginParamValues::start` also gives the mirror's
    /// report callback; a fake's reports cannot be fired from outside
    /// `tutti-plugin`, so the callback is not asserted here.
    #[test]
    fn a_crossfade_to_another_plugin_rebuilds_the_mirror() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (outgoing, _) = fake_handle(None, vec![(param, 0.25)]);
        let (mut mirror, read) = tutti_plugin_host::PluginParamValues::start(&outgoing);
        mirror.land(
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                .expect("the read lands"),
        );
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.mirror = mirror;
        assert_eq!(
            hosted.param_value(ParamId::new(3)),
            Some(Some(0.25)),
            "precondition"
        );

        let (incoming, _) = fake_handle(None, vec![(param, 0.75)]);
        hosted.adopt(incoming, Placement::InProcess);
        assert_eq!(
            hosted.param_value(ParamId::new(3)),
            None,
            "not ready until the read lands: nothing was read on this thread"
        );

        let _ = rig.events_at(0);
        assert_eq!(
            rig.mirror_holders(),
            2,
            "the read is a task holding the token"
        );
        rig.settle();
        // This phase lands the read's end, and starts no second one.
        let _ = rig.events_at(10);
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            Some(Some(0.75)),
            "the mirror holds the incoming plugin's values"
        );
        assert_eq!(
            rig.mirror_holders(),
            1,
            "the landing freed the token, and no second read started"
        );

        // A read asked for, then a second crossfade: the first lands nowhere.
        let (second, _) = fake_handle(None, vec![(param, 0.5)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(second, Placement::InProcess);
        let _ = rig.events_at(20);
        let (third, _) = fake_handle(None, vec![(param, 0.125)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(third, Placement::InProcess);
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            None,
            "a read of the previous plugin must not land"
        );
        let _ = rig.events_at(30);
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            Some(Some(0.125)),
            "its own read landed"
        );
    }

    /// **A read whose parameter list is unavailable does not make the mirror
    /// ready, and is started again.** Its end lands as not landed (the
    /// mirror stays not ready), the next phase starts the read for the
    /// same plugin once more, and once the list answers the re-run read lands
    /// the values.
    #[test]
    fn a_read_whose_parameter_list_is_unavailable_is_submitted_again() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (outgoing, _) = fake_handle(None, vec![(param, 0.25)]);
        let (mut mirror, read) = tutti_plugin_host::PluginParamValues::start(&outgoing);
        mirror.land(
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                .expect("the read lands"),
        );
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.mirror = mirror;

        let unlisted = Arc::new(AtomicBool::new(true));
        let incoming = PluginHandle::from_backend(
            Arc::new(Fake {
                panics: false,
                state: None,
                saves: Arc::default(),
                dead: Arc::default(),
                params: vec![(param, 0.75)],
                unlisted: Arc::clone(&unlisted),
                hold: None,
                main: None,
            }),
            None,
            tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
            Default::default(),
            Default::default(),
        );
        hosted.adopt(incoming, Placement::InProcess);

        let _ = rig.events_at(0);
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            None,
            "the mirror is not ready"
        );
        assert!(
            rig.session.graph.plugins[&rig.node].param_read.is_some(),
            "the read is kept to run again"
        );

        unlisted.store(false, Ordering::SeqCst);
        let _ = rig.events_at(10);
        rig.settle();
        let _ = rig.land();
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            Some(Some(0.75)),
            "the re-run read filled the mirror"
        );
        let _ = rig.events_at(20);
        assert_eq!(rig.mirror_holders(), 1, "and it is done");
    }

    /// **A reload task that ends without a result is a failed attempt, named by
    /// how it ended.** A task dropped unrun is `ReloadDropped`, one that
    /// panicked `ReloadPanicked` with its message; either schedules the next
    /// attempt (`Reloading`). The end of an attempt that was already replaced
    /// (its token is not the record's any more) changes nothing.
    #[test]
    fn a_reload_task_that_ends_without_a_result_is_a_failed_attempt() {
        for panics in [false, true] {
            let mut rig = rig(None);
            let now = rig.t0;
            let node = rig.node;
            let hosted = rig.session.graph.plugins.get_mut(&node).unwrap();
            let mut recovery = Recovery::new(RecoveryPolicy::default());
            let _ = recovery.step(
                RecoveryEvent::Died {
                    cause: PluginFailure::StreamClosed,
                },
                now,
            );
            hosted.reloadable = Some(Reloadable {
                origin: Origin {
                    config: AudioConfig::default(),
                    loadable: Loadable::uncatalogued(PluginRef::file("fake.clap")),
                },
                recovery: Some(recovery),
            });
            hosted.status = PluginLiveness::Recovering {
                cause: ReloadCause::Died(PluginFailure::StreamClosed),
            };
            hosted.reload.in_flight = true;
            let token = Arc::clone(&hosted.reload.token);
            let post = rig.session.plugins.inbox.post();
            let end =
                |post: &inbox::Post| post.reload(node, Arc::clone(&token), None, Arc::default());
            if panics {
                end(&post).run(|| panic!("boom"));
            } else {
                drop(end(&post));
            }

            let out = rig.land();
            let [
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading { cause, .. },
                    ..
                }),
            ] = &out[..]
            else {
                panic!("the next attempt is scheduled: {out:?}");
            };
            match (panics, cause) {
                (false, ReloadCause::Attempt(RecoveryFailure::ReloadDropped)) => {}
                (true, ReloadCause::Attempt(RecoveryFailure::ReloadPanicked { message })) => {
                    assert_eq!(message, "boom")
                }
                other => panic!("{other:?}"),
            }

            // The same end again, with the attempt's old token: nothing.
            drop(end(&post));
            let again = rig.land();
            assert!(again.is_empty(), "a replaced attempt's end is stale");
        }
    }

    /// **Recovery lands once per attempt.** The reload's task ends with its load, which
    /// fails the attempt and schedules the next with a new token; a second end of the same
    /// attempt (a task dropped after it sent) lands nowhere, so one attempt is one
    /// `Reloading` and one failure in the policy's history.
    #[cfg(feature = "clap")]
    #[test]
    fn recovery_lands_once_per_attempt() {
        let mut rig = rig(None);
        rig.recovering_in_process();
        let attempt = Arc::clone(&rig.session.graph.plugins[&rig.node].reload.token);
        let _ = rig.events_at(0);
        rig.settle();
        drop(
            rig.session
                .plugins
                .inbox
                .post()
                .reload(rig.node, attempt, None, Arc::default()),
        );
        let events = rig.land();
        let [
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { attempt, .. },
                ..
            }),
        ] = &events[..]
        else {
            panic!("one attempt, one landing: {events:?}");
        };
        assert_eq!(*attempt, 2, "the death, and the one failed attempt");
    }

    /// **A reload of a plugin whose load resolves in process is refused as
    /// `ReplacementInProcess`, with no main thread and nothing opened.** The
    /// decision is `Loadable::placement` (a pure resolve, no module
    /// read): `PluginLoad::run` returns such a load unopened (`Opened::InProcess`),
    /// and the landing does not open it. No thread of this process claimed the
    /// main thread, so an open (which takes the token) could not have happened.
    #[cfg(feature = "clap")]
    #[test]
    fn a_reload_that_resolves_in_process_is_refused_unopened() {
        assert!(
            MainThreadMarker::new().is_none(),
            "this test thread is not the main thread"
        );
        let mut rig = rig(None);
        rig.recovering_in_process();
        let _ = rig.events_at(0);
        rig.settle();
        let events = rig.land();
        let [
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { cause, .. },
                ..
            }),
        ] = &events[..]
        else {
            panic!("the next attempt is scheduled: {events:?}");
        };
        assert!(
            matches!(
                cause,
                ReloadCause::Attempt(RecoveryFailure::ReplacementInProcess)
            ),
            "{cause:?}"
        );
        assert!(
            MainThreadMarker::new().is_none(),
            "still not the main thread"
        );
    }

    /// **A plugin blacklisted after its load is not reloaded**: the reload asks
    /// the catalog blacklist the original load ran under, so a plugin the
    /// catalog has since recorded as crashing the scanner fails the attempt as
    /// the blacklist's refusal, and nothing is opened.
    #[test]
    fn a_reload_of_a_plugin_blacklisted_since_its_load_is_refused() {
        use tutti_plugin_db::{Catalog, CatalogConfig, PluginDb};
        let file =
            std::env::temp_dir().join(format!("tutti-reload-bl-{}.clap", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let mut catalog = Catalog::new(PluginDb::new(), CatalogConfig::new("/nonexistent"), |_| {
            Ok(())
        });
        let plugin = PluginRef::file(&file);
        let loadable = Loadable::uncatalogued(plugin.clone()).under(&catalog);
        let mut rig = rig(None);
        rig.recovering(plugin, loadable);

        catalog.blacklist(&file, "crashed the scanner");
        let _ = rig.events_at(0);
        rig.settle();
        let events = rig.land();
        std::fs::remove_file(&file).ok();

        let [
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { cause, .. },
                ..
            }),
        ] = &events[..]
        else {
            panic!("the next attempt is scheduled: {events:?}");
        };
        let ReloadCause::Attempt(RecoveryFailure::ReloadFailed { source }) = cause else {
            panic!("{cause:?}");
        };
        assert!(
            matches!(&**source, IpcError::Blacklisted { reason, .. } if reason == "crashed the scanner"),
            "{source:?}"
        );
    }

    /// **A swap while the plugin is recovering ends the recovery: the old reload
    /// lands nowhere and the node reads healthy.** The reload is in flight when
    /// the incoming plugin takes the record (`HostedPlugin::adopt`, which a swap
    /// runs): its token is not the record's any more, so its end is discarded, and
    /// the incoming plugin's health starts clean.
    #[cfg(feature = "clap")]
    #[test]
    fn a_replacement_while_recovering_lands_the_old_reload_nowhere() {
        let mut rig = rig(None);
        rig.recovering_in_process();
        let _ = rig.events_at(0);
        assert!(
            matches!(
                rig.session.plugin_view(rig.node).unwrap().health(),
                PluginHealth::Reloading { .. }
            ),
            "the record is recovering"
        );
        let (incoming, _) = fake_handle(None, Vec::new());
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming, Placement::InProcess);
        assert!(matches!(
            rig.session.plugin_view(rig.node).unwrap().health(),
            PluginHealth::Running
        ));
        rig.settle();
        let events = rig.land();
        assert!(
            events.is_empty(),
            "the old reload landed nowhere: {events:?}"
        );
        assert!(matches!(
            rig.session.plugin_view(rig.node).unwrap().health(),
            PluginHealth::Running
        ));
    }

    /// **A reload task that outlives its node lands nowhere, silently.** The
    /// task ends after the node is removed: its end lands as no event, and no failure is
    /// recorded for a node that is gone.
    #[cfg(feature = "clap")]
    #[test]
    fn a_reload_task_that_outlives_its_node_lands_quietly() {
        let mut rig = rig(None);
        rig.recovering_in_process();
        let _ = rig.events_at(0);
        rig.session.remove(rig.node).expect("the node is removed");
        assert!(
            rig.session.plugin_view(rig.node).is_err(),
            "its record is gone"
        );
        rig.settle();
        let events = rig.land();
        assert!(events.is_empty(), "nothing is reported: {events:?}");
    }

    /// **A swap to an in-process plugin drops the served kind's way back.** The
    /// record of a served plugin holds the load it came from and its recovery's
    /// budget; the in-process plugin that replaces it is never reloaded, so
    /// neither is left to act on.
    #[test]
    fn a_replacement_in_process_holds_no_way_back() {
        let (outgoing, _) = fake_handle(None, Vec::new());
        let mut hosted = HostedPlugin::new(outgoing, PluginRef::file("fake.clap"), node_controls());
        hosted.reloadable = Some(Reloadable {
            origin: Origin {
                config: AudioConfig::default(),
                loadable: Loadable::uncatalogued(PluginRef::file("fake.clap")),
            },
            recovery: Some(Recovery::new(RecoveryPolicy::default())),
        });
        let (incoming, _) = fake_handle(None, Vec::new());
        let _ = hosted.replace_hosted(incoming, node_controls(), PluginRef::file("other.clap"));
        assert!(hosted.reloadable.is_none(), "no way back to act on");
    }

    /// **A plugin that replaces another starts its own bypass episode.** The
    /// outgoing plugin ignored the node's mode (an open episode, counted once);
    /// the incoming one's divergence is counted as its own, from zero.
    #[test]
    fn a_replacement_counts_its_own_bypass_episode() {
        use crate::bypass::tests::rig;
        use std::sync::atomic::Ordering::SeqCst;
        let (outgoing, plugin_a, mirror_a) = rig(false);
        plugin_a.accepts.store(false, SeqCst);
        let mut hosted = HostedPlugin::new(
            outgoing.clone(),
            PluginRef::file("fake.clap"),
            node_controls(),
        );
        hosted
            .bypass
            .reconcile(&outgoing, &mirror_a, tutti_graph::NodeMode::Bypassed);
        assert_eq!(
            hosted.bypass.unapplied(),
            1,
            "the outgoing plugin's episode"
        );

        let (incoming, plugin_b, _) = rig(false);
        plugin_b.accepts.store(false, SeqCst);
        let _ = hosted.adopt(incoming.clone(), Placement::InProcess);
        assert_eq!(hosted.bypass.unapplied(), 0, "nothing carries over");
        hosted
            .bypass
            .reconcile(&incoming, &hosted.mirror, tutti_graph::NodeMode::Bypassed);
        assert_eq!(hosted.bypass.unapplied(), 1, "the incoming plugin's own");
        assert_eq!(plugin_b.writes.load(SeqCst), 1);
    }

    /// **A plugin that enters refusing its bypass is written once and counted
    /// once.** The outgoing plugin's refusal is latched; the incoming one, which
    /// refuses too, starts unlatched: the node's mode is written to it and its
    /// refusal is its own episode.
    #[test]
    fn a_replacement_that_refuses_is_written_and_counted_once() {
        use crate::bypass::tests::rig;
        use std::sync::atomic::Ordering::SeqCst;
        use tutti_graph::NodeMode;
        let (outgoing, plugin_a, mirror_a) = rig(false);
        plugin_a.refuses.store(true, SeqCst);
        let mut hosted = HostedPlugin::new(
            outgoing.clone(),
            PluginRef::file("fake.clap"),
            node_controls(),
        );
        hosted
            .bypass
            .assert_mode(&outgoing, &mirror_a, NodeMode::Bypassed);
        assert_eq!(hosted.bypass.unapplied(), 1, "the outgoing plugin's");

        let (incoming, plugin_b, _) = rig(false);
        plugin_b.refuses.store(true, SeqCst);
        let _ = hosted.adopt(incoming.clone(), Placement::InProcess);
        assert_eq!(hosted.bypass.unapplied(), 0, "nothing carries over");
        hosted
            .bypass
            .assert_mode(&incoming, &hosted.mirror, NodeMode::Bypassed);
        for _ in 0..40 {
            hosted
                .bypass
                .reconcile(&incoming, &hosted.mirror, NodeMode::Bypassed);
        }
        assert_eq!(plugin_b.attempts.load(SeqCst), 1, "written once");
        assert_eq!(hosted.bypass.unapplied(), 1, "counted once");
    }
}
