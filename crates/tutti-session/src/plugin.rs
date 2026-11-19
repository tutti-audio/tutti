//! Hosted plugins in the session (feature `plugin`; design 015 §4.2-§4.4):
//! the pool entry that loads one ([`Session::plugin_entry`]), the add
//! ([`Session::add_pooled_plugin`]),
//! the view of the record the session keeps for each one ([`Plugin`]), and the
//! work of [`Phase::Plugins`](crate::Phase::Plugins) and
//! [`Phase::RenderMode`](crate::Phase::RenderMode).
//!
//! # The pool
//!
//! A plugin may also be a pool entry ([`Session::plugin_entry`]): the pool
//! loads it, [`Session::add_pooled_plugin`] puts its node in the graph through
//! the one install, and the node's record holds the entry until the record
//! retires.
//!
//! # The record
//!
//! Each plugin the graph holds (`Graph::insert_plugin`,
//! [`Session::add_pooled_plugin`]) has a record,
//! keyed by its [`NodeKey`] and lent as a [`Plugin`] ([`Session::plugin_view`]): the [`PluginRef`] it is, its handle, its
//! controls (an out-of-process plugin's latency and meter cells), its
//! liveness, whether it was told the render mode, its last good state
//! ([`state`](Plugin::state)), and its parameter mirror
//! ([`params`](Plugin::params)). A crossfade to another plugin (`Graph::crossfade_plugin`,
//! `replace_plugin`) carries the record over to the incoming plugin, with
//! nothing of the outgoing plugin's state (its last good state, an in-flight
//! snapshot job, its mirror); replacing it with a native node
//! (`Graph::replace`, `crossfade`) or removing the node drops it.
//!
//! # The main thread
//!
//! An in-process plugin's handle (`PluginHandle`, VST2 today) works from any
//! thread, which is what the snapshot and parameter-read jobs below need: a
//! call off the main thread is posted to the session's
//! [`MainWork`](tutti_plugin_types::MainWork) and the caller waits, for at most two
//! seconds, for the main thread's next tick to run it
//! ([`Session::update`], called on the process's main thread, drains it). A host that does not
//! call it there
//! has those calls answer "unavailable" at the deadline (`StateError::Unavailable`, `None`,
//! `false`). A host blocking its main thread on such a job (joining it there)
//! gets the same.
//!
//! What retires an in-process record (a removal, a crossfade to another plugin
//! or to a native node, a reload's dead plugin) parks it in the graveyard
//! instead of dropping it, since the drop of a plugin's last owner tears down
//! its library and instance. `Session::update` on the main thread empties it;
//! [`Session::retired_plugins`] counts what waits, and `Phase::Plugins` warns
//! once when more than a handful do. A served plugin's record drops where it
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
//!   unhealthy observations spanning at least the window. A dead pooled
//!   plugin's node stays at its key, silent in its place, and the entry reads
//!   lost; a node of a plugin the host inserted itself (`Graph::insert_plugin`)
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
//!   parameter when it has one (doc 014 §8.2). A mirror rebuilt by a crossfade
//!   to another plugin, or by a rescan the plugin announced, is read as a job
//!   on the session's spawner (`SessionBuilder::spawn_with`): one round trip
//!   per parameter, landed in a later [`Session::update`]. Until then the
//!   mirror is not ready, and a lane on the plugin waits for it.
//! - **Snapshots.** A healthy plugin's state is captured every
//!   `snapshot_interval` (8 s by default, set on the `SessionBuilder`) of
//!   `now`, as a job on the same spawner (a blocking round trip), landed in a later
//!   update. One is in flight per plugin at most: the landing holds the
//!   plugin's snapshot token until it lands, whether the job ran, panicked or
//!   was dropped.
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
//!   the last good state as its state) is submitted to the spawner as a job.
//!   Its landing, in a later `Session::update` and on whatever thread runs it
//!   (no main-thread proof: a served replacement opens off the main thread,
//!   and one that came back unopened for the main thread is a failed attempt),
//!   swaps the loaded plugin in at the same node (`Graph::replace_plugin`)
//!   and keeps the record: the last good state, its age, the failure
//!   history. A job the spawner drops is [`RecoveryFailure::ReloadDropped`],
//!   one that panics [`RecoveryFailure::ReloadPanicked`]: a failed attempt.
//! - **Reported.** [`PluginChange::Recovered`] says whether the state
//!   came back and how old it was; a spent budget is
//!   [`PluginChange::GaveUp`], and the node is silenced (a pooled plugin's) or
//!   removed (a host-inserted one).
//!
//! An in-process plugin (VST2) is not recovered: a crash in it takes the
//! process, and a hang in it blocks the audio thread itself, which nothing
//! here observes (`block_counts` is zeros for it).
//!
//! # `Phase::RenderMode` (with `export`)
//!
//! Offline while an export token lives (§4.3), Realtime otherwise; each
//! plugin is told per `tutti_plugin_host::needs_notify`: on a change of
//! mode, or if it was never told.

use std::sync::{Arc, Mutex};

use std::time::Duration;

use tutti_platform::Instant;
use tutti_plugin_types::{MainThreadMarker, StateUse};
use tutti_rt::LockExt;

use tutti_graph::NodeKey;
use tutti_plugin::catalog::{AudioConfig, Hosting, Loadable, Plugin as HostPlugin, PluginRef};
use tutti_plugin::error::StateError;
use tutti_plugin_host::{Liveness, LivenessVerdict};
use tutti_plugin_host::{
    NodeControls, PluginControls, PluginHandle, PluginSignal, PluginStatus, Subscription,
};
use tutti_plugin_types::{
    BridgeError, ParamEdit, PluginFailure, PluginInvalidation, PluginRefresh, PluginTail,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

mod entry;
#[cfg(test)]
mod fake_hosting;
mod lost;
mod rebind;
pub use rebind::PortsRefusal;
mod preset_job;
mod state_job;
pub(crate) use lost::Lost;

use entry::PooledLink;
pub use entry::{
    AddPluginError, LoadedPlugin, PluginCause, PluginDeclaration, PluginEntry, PluginInfo,
    PluginPorts, PooledPlugin,
};
pub use preset_job::{LoadPreset, PresetJobError, PresetJobFailed};
pub use state_job::StateJobError;

use crate::jobs::{Landing, PluginLanding};
use crate::plugin_diagnostics::Figures;
use crate::{
    Diagnostics, GiveUp, HangDetector, Key, OtherPeer, Params, PeerState, PluginBuses, PortCounts,
    Recovery, RecoveryAction, RecoveryEvent, RecoveryFailure, ReloadCause, SavedPeer, Session,
    SessionEvent, WorkerError,
};

/// How often a healthy plugin's state is captured, by default: 8 s.
/// [`SessionBuilder::snapshot_interval`](crate::SessionBuilder::snapshot_interval)
/// sets another.
pub(crate) const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(8);

/// A plugin load: what the pool's plugin entry runs, and what a recovery reload
/// runs again. [`run`](Self::run) blocks (a `plugin-server` launch, half a
/// second to fifteen), so it runs on a worker, never on the control thread.
#[must_use = "a load does nothing until it is run"]
pub(crate) struct PluginLoad {
    /// The plugin, under the blacklist of the catalog that resolved it, asked
    /// by [`run`](Self::run) as the load opens.
    loadable: Loadable,
    rate: SampleRate,
    config: AudioConfig,
    state: Option<Arc<PeerState>>,
    /// The session's collector of main-thread work, which the plugin's handles hold.
    work: tutti_plugin_types::MainWorkHandle,
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
    /// Opens the plugin, restores the request's state, and (`modulation`)
    /// reads its parameter values, after the state: those are what a
    /// modulation lane's knob starts at. Any thread; it blocks.
    ///
    /// A plugin hosted in process is not opened here: an in-process load is
    /// confined to the main thread, so the job returns it unopened and the
    /// pool's main-thread stage opens it there (a library load and an
    /// instance's init, with no subprocess to wait for).
    ///
    /// A load the catalog refuses, or whose hosting the format cannot have
    /// ([`BridgeError::NotOutOfProcess`] for VST2 asked to be served), is
    /// refused here, before anything is opened.
    pub(crate) fn run(self) -> PluginLoaded {
        let loadable = self.loadable.clone();
        let config = self.config.clone();
        let refused = self
            .loadable
            .check()
            .err()
            .or_else(|| tutti_plugin::catalog::placement(&self.loadable).err());
        let opened = if let Some(refusal) = refused {
            Opened::Done(Box::new(Open::refused(refusal)))
        } else if matches!(
            tutti_plugin::catalog::placement(&self.loadable),
            Ok(Hosting::InProcess)
        ) {
            Opened::OnMain(Box::new(self))
        } else {
            Opened::Done(Box::new(self.open(None)))
        };
        PluginLoaded {
            loadable,
            config,
            opened,
        }
    }

    /// [`run`](Self::run)'s open, on this thread. `main` is the proof that it is
    /// the main thread, which a plugin hosted in process needs.
    fn open(self, main: Option<MainThreadMarker>) -> Open {
        // The state goes in **before the plugin is activated** (`open_with_state`):
        // a plugin may take its mode from the state at activation. A refusal
        // does not abort the load: a plugin at its defaults beats no plugin. But
        // it is not silent.
        let (plugin, restore) = match HostPlugin::open_with_state(
            main,
            &self.work,
            &self.config,
            &self.loadable,
            self.rate,
            self.state.as_deref().map(PeerState::bytes),
        ) {
            Ok((plugin, restore)) => (Ok(plugin), restore),
            // The server died, or stopped answering, inside the step that
            // restores the state it was staged (`StateRestore`; a failure
            // before that step is a load failure and blames nothing): the state
            // is what the load is held against (the reload's landing withholds it).
            Err(e @ tutti_plugin_types::BridgeError::StateRestore { .. })
                if self.state.is_some() =>
            {
                let reason = e.to_string();
                (Err(e), Some(Err(reason)))
            }
            Err(e) => (Err(e), None),
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
            // A read the plugin did not answer (its list unavailable) leaves
            // the mirror not ready and goes to the hosted record, to be handed
            // out as a job.
            let unread = match read.run() {
                Ok(read) => {
                    values.land(read);
                    None
                }
                Err(read) => Some(*read),
            };
            Mirror { values, unread }
        });
        Open {
            plugin,
            restore,
            mirror,
        }
    }
}

/// A new plugin's parameter mirror, and the read that still has to fill it
/// when the load-time read was not answered.
struct Mirror {
    values: tutti_plugin_host::PluginParamValues,
    /// Becomes the hosted record's `param_read`, to be run as a job.
    unread: Option<tutti_plugin_host::ParamRead>,
}

/// An opened plugin (or why it did not open) and its parameter mirror.
struct Open {
    plugin: Result<HostPlugin, BridgeError>,
    /// The state restore, if one was asked for: the plugin's refusal, as text.
    restore: Option<Result<(), String>>,
    mirror: Option<Mirror>,
}

impl Open {
    /// A load that did not open: `why`.
    fn refused(why: BridgeError) -> Self {
        Self {
            plugin: Err(why),
            restore: None,
            mirror: None,
        }
    }
}

/// What a parameter read gave: the values, or the read itself back to be run
/// again (the plugin's list was unavailable).
pub(crate) type ParamValues =
    Result<tutti_plugin_host::ParamSnapshot, Box<tutti_plugin_host::ParamRead>>;

/// What a `PluginLoad`'s run did.
enum Opened {
    /// Opened (or failed to) on the job's thread. Boxed: a plugin is far
    /// larger than an unopened load.
    Done(Box<Open>),
    /// Hosted in process: opened on the main thread.
    OnMain(Box<PluginLoad>),
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
        /// The plugin's refusal.
        reason: String,
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
    loadable: Loadable,
    config: AudioConfig,
    opened: Opened,
}

impl std::fmt::Debug for PluginLoaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginLoaded")
            .field("plugin", self.loadable.plugin())
            .finish_non_exhaustive()
    }
}

impl PluginLoaded {
    /// Opens what the run left: nothing for a plugin opened on the job's
    /// thread, the plugin itself for one hosted in process, which opens on
    /// `main` (without it the open is refused).
    fn finish(self, main: Option<MainThreadMarker>) -> (Loadable, AudioConfig, Open) {
        let open = match self.opened {
            Opened::Done(open) => *open,
            Opened::OnMain(load) => (*load).open(main),
        };
        (self.loadable, self.config, open)
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
    /// Written off (recovery off, or its budget spent): a pooled plugin's node
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
/// ([`PluginChange::Died`], [`PluginChange::GaveUp`]). A pooled plugin's node
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
    /// [`Reloading`](Self::Reloading), or, with recovery off, a pooled
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
    /// The plugin's bus layout changed. A pooled in-process plugin's node is
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
    /// The plugin's new ports were not made a node, and the node stays as it
    /// is: they are wider than a block serves, or the graph refused the swap
    /// ([`PortsRefusal`]). Reported once for the ports: they count as handled.
    PortsRefused {
        /// Why the ports were refused.
        why: PortsRefusal,
    },
    /// A hosted plugin was declared dead
    /// (`tutti_plugin_host::Liveness`) with recovery off: a pooled plugin's
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
    /// Recovery stopped: the failures spent the budget. A pooled plugin's node
    /// stays at its key, silent; a host-inserted plugin's node is removed.
    GaveUp {
        /// Why.
        why: GiveUp,
        /// The last state worth keeping, for a host that offers the user a
        /// manual reload: the state a reload withheld, else the last good one.
        state: Option<Box<PeerState>>,
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
    notices.lock_recover().push(notice);
}

/// A queue for what `handle` reports and the subscription that fills it.
fn hear(handle: &PluginHandle) -> (Signals, Subscription) {
    let notices = Signals::default();
    let queue = Arc::clone(&notices);
    let hearing = handle.subscribe(move |notice| queue.lock_recover().push(notice));
    (notices, hearing)
}

/// What `notice` is as an event, if it is one: a latency the plugin reports
/// is reported once, by the update that hands it to the graph; a rate refusal,
/// a crash and a reload are the plugin's health.
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
    /// The handle, split by where its plugin ran. A served plugin's handle
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
    /// A served plugin's, already dropped.
    Served,
}

impl Retired {
    /// `held` split by `placement`: an in-process owner is kept for the main
    /// thread, a served one drops here.
    fn of(placement: Placement, held: impl std::any::Any + Send) -> Self {
        match placement {
            Placement::InProcess => Self::InProcess(InProcessRecord {
                _held: Box::new(held),
            }),
            Placement::Served => Self::Served,
        }
    }

    /// Parks an in-process record in `yard`; a served one has dropped, and
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
/// and the view that holds the proof empties the yard. A served plugin's
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
    /// What the node counts and holds (its meter cell and delivery counters).
    /// The project meter is installed only for a plugin the session added.
    pub(crate) node: NodeControls,
    /// What the plugins this record held before the current one lost, as
    /// [`Plugin::losses`] counts: banked when a plugin or its node is replaced,
    /// so the record's count only grows.
    banked: tutti_graph::Losses,
    /// What the nodes before the held one did not render, or the plugin did
    /// not take ([`Diagnostics::block_losses`]): banked with `banked`.
    banked_blocks: tutti_plugin_host::BlockLosses,
    liveness: Liveness,
    hang: HangDetector,
    status: PluginLiveness,
    /// Told a render mode since it was added (or crossfaded in).
    pub(crate) told: bool,
    /// The plugin's own bypass against the node's mode.
    pub(crate) bypass: crate::bypass::BypassSync,
    snapshot: Option<PeerState>,
    /// When the snapshot's capture was asked for.
    snapshot_from: Option<Instant>,
    /// A snapshot left out of a reload ([`StateRestore::Withheld`]), kept for
    /// the host ([`StateView::withheld`]); later
    /// snapshots go to `snapshot` and never replace it.
    withheld: Option<PeerState>,
    /// Until when a plugin that has just restored a state is suspected of
    /// dying of it.
    probation_until: Option<Instant>,
    /// How this plugin was loaded, for a reload; `None`: not recoverable.
    origin: Option<Origin>,
    recovery: Option<Recovery>,
    reload: Reload,
    /// Held by the snapshot job in flight, if one is.
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
    /// The read that fills the mirror (a crossfade rebuilt it, the load's read
    /// was not answered, or the plugin announced a rescan), until
    /// `Phase::Plugins` hands it to the host as a job.
    param_read: Option<tutti_plugin_host::ParamRead>,
    /// What the outgoing mirror declared, when a replace put the same plugin
    /// ([`PluginRef::is_same_plugin`]) at the node: the value table keeps these
    /// declarations and values until the new read lands, which replaces them.
    carried: Option<Vec<crate::value::CarriedParam>>,
    /// Held by that job: a later read mints a new one, and the earlier read
    /// lands nowhere.
    mirror_token: Arc<()>,
    /// The node of a plugin that is gone renders silence in the plugin's
    /// place ([`lost`]); until the graph takes the swap it is not.
    pub(crate) silenced: bool,
    /// The pool entry this plugin's node is over, `None` for a plugin added
    /// outside the pool. Dropped when the record retires, which ends the hold.
    pub(crate) pooled: Option<PooledLink>,
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
    /// A node over `laid_out` was swapped in and the plugin's Processor has not
    /// reached it yet: no second rebuild starts before it does.
    Settling {
        rebinder: tutti_plugin::catalog::PluginRebinder,
        laid_out: tutti_plugin_types::Layout,
    },
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
        matches!(self, Self::Settling { .. })
    }

    pub(crate) fn laid_out(&self) -> &tutti_plugin_types::Layout {
        match self {
            Self::Idle { laid_out, .. } | Self::Settling { laid_out, .. } => laid_out,
        }
    }

    /// The same builder, over the ports `laid_out`, settling when `settling`.
    pub(crate) fn with(self, laid_out: tutti_plugin_types::Layout, settling: bool) -> Self {
        let rebinder = match self {
            Self::Idle { rebinder, .. } | Self::Settling { rebinder, .. } => rebinder,
        };
        if settling {
            Self::Settling { rebinder, laid_out }
        } else {
            Self::Idle { rebinder, laid_out }
        }
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
        self.0.lock_recover().ended = true;
    }

    /// The ids the mirror declares now (a read landed).
    pub(crate) fn declare(&self, mirror: &tutti_plugin_host::PluginParamValues) {
        self.0.lock_recover().declared = mirror.descriptors().iter().map(|p| p.id()).collect();
    }

    /// Whether the plugin is still there and declares `id`.
    pub(crate) fn holds(&self, id: ParamId) -> bool {
        let state = self.0.lock_recover();
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

/// A recovering plugin's reload, between its death and its replacement.
#[derive(Default)]
struct Reload {
    /// When to hand the load out.
    due: Option<Instant>,
    /// Held by the job in flight; a new one per job, so a late landing of an
    /// earlier job is recognised.
    token: Arc<()>,
    in_flight: bool,
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
            .field("recovery", &self.recovery)
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

    /// The record of a plugin served from a plugin server, with its `controls`.
    pub(crate) fn served(
        handle: PluginHandle,
        plugin: PluginRef,
        controls: PluginControls,
    ) -> Self {
        let mut record = Self::placed(Placement::Served, handle, plugin, (*controls).clone());
        record.controls = Some(controls);
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
        Self {
            placement,
            plugin,
            handle,
            controls: None,
            node,
            banked: tutti_graph::Losses::NONE,
            banked_blocks: tutti_plugin_host::BlockLosses::default(),
            liveness: Liveness::new(),
            hang: HangDetector::default(),
            status: PluginLiveness::Healthy,
            told: false,
            bypass: crate::bypass::BypassSync::default(),
            snapshot: None,
            snapshot_from: None,
            withheld: None,
            probation_until: None,
            origin: None,
            recovery: None,
            reload: Reload::default(),
            snapshot_token: Arc::new(()),
            snapshot_at: None,
            notices,
            hearing,
            mirror,
            gate: Gate::default(),
            driven: crate::plugin_params::Driven::default(),
            param_read: Some(read),
            carried: None,
            mirror_token: Arc::new(()),
            silenced: false,
            pooled: None,
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
    /// makes a job of the outgoing plugin still in flight land nowhere, the
    /// snapshot clock restarts, and its own-bypass state (the episode, the waits
    /// and the count) starts over.
    ///
    /// (`modulation`) A record that kept a parameter mirror gets one for
    /// the incoming plugin, from its handle (where its reports now arrive),
    /// not yet ready: its values are read by a job (one round trip per
    /// parameter, so not on the calling thread), which the next
    /// `Phase::Plugins` submits to the session's spawner.
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
    ) -> Outgoing {
        self.carry_declaration(&plugin);
        self.plugin = plugin;
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
        let outgoing = self.adopt(handle, Placement::InProcess);
        self.set_node(node);
        self.controls = None;
        outgoing
    }

    /// This record, retired: an in-process plugin's waits for the main
    /// thread, a served plugin's drops here.
    pub(crate) fn retire(mut self) -> Retired {
        self.gate.end();
        // The node over a pool entry is gone: the entry is no longer held.
        self.pooled = None;
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
    /// [`lost`](tutti_graph::Losses::lost)), the parameter points it did not
    /// deliver as they were, the chords and scales past the capacity of a
    /// block's sequencer context ([refused](tutti_graph::Losses::refused)), and,
    /// for a served plugin, the commands its bridge's queue
    /// [refused](tutti_graph::Losses::refused) and the responses it
    /// [lost](tutti_graph::Losses::lost).
    fn node_losses(&self) -> tutti_graph::Losses {
        use tutti_graph::Losses;
        let points = self.node.param_delivery();
        let bridge = self.controls.as_ref().map_or(Losses::NONE, |c| {
            Losses::refused(c.refused_commands()) + Losses::lost(c.lost_responses())
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

    /// What the plugin lost, over the record's life: what the held handle and
    /// node counted, and what the ones before them did.
    pub(crate) fn losses(&self) -> tutti_graph::Losses {
        self.banked
            + self.node_losses()
            + tutti_graph::Losses::lost(self.handle.dropped_parameter_events())
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
        self.banked += tutti_graph::Losses::lost(self.handle.dropped_parameter_events());
        let (mirror, read) = tutti_plugin_host::PluginParamValues::start(&handle);
        self.mirror = mirror;
        self.param_read = Some(read);
        self.mirror_token = Arc::new(());
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
        self.snapshot = None;
        self.snapshot_from = None;
        self.withheld = None;
        self.probation_until = None;
        self.recovery = self.recovery.as_ref().map(|r| Recovery::new(r.policy()));
        self.reload = Reload::default();
        // The incoming plugin's bypass is its own: nothing of the outgoing one's
        // episode, waits or count carries over.
        self.bypass = crate::bypass::BypassSync::default();
        self.silenced = false;
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
        // A job of an earlier attempt lands nowhere.
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
        queue(
            &self.notices,
            PluginSignal::Refresh(PluginRefresh::ParamValues),
        );
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
        );
        let mode = self
            .takes_automation_mode()
            .then(|| crate::value::AutomationModeSlot::cell(self.handle.clone()));
        table.provide_plugin(node, entry, mode);
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

/// The type of a hosted plugin's [`Key`]: what [`Session::add_pooled_plugin`]
/// and [`Graph::insert_plugin`](crate::Graph::insert_plugin) return, so a plugin's
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
    /// [`Param::target`](crate::Param::target) gives, and it converts into the
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
    /// ([`Losses`](crate::Losses)):
    ///
    /// - [`lost`](crate::Losses::lost): MIDI events on the node's event input
    ///   that never reached the plugin (in a
    ///   block the plugin did not run, or the rest of a SysEx message that
    ///   lost a packet; events past the node's declared event-input bound are
    ///   cut by the graph before the plugin runs, and are reported as
    ///   [`SessionEvent::EventsDropped`]
    ///   with `port: None`, not here); parameter points of a sent grid that a full queue
    ///   dropped; and parameter edits the plugin made that the host did not
    ///   hear (a lost edit costs a stale readout, not audio, and the session
    ///   re-reads the parameters after one).
    /// - [`evicted`](crate::Losses::evicted): parameter points that replaced
    ///   the last one of a full queue.
    /// - [`refused`](crate::Losses::refused): parameter points for a
    ///   parameter the node has no slot for, and chords and scales past the
    ///   capacity of a block's sequencer context.
    ///
    /// The MIDI the plugin sent that the node had no room to put out is in
    /// [`lost`](crate::Losses::lost) too.
    ///
    /// [`late`](crate::Losses::late) stays zero. Audio the plugin's pipeline did
    /// not answer in time is not an event loss: it is counted in chunks, by
    /// [`Diagnostics::missed_chunks`]; the blocks the node did not render and
    /// the entries the plugin refused are counted in blocks, by
    /// [`Diagnostics::block_losses`].
    #[must_use]
    pub fn losses(&self) -> crate::Losses {
        self.hosted.losses()
    }

    /// The plugin's saved states: the last good one, the withheld one, a save
    /// now, and a restore.
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
                attempt: hosted.recovery.as_ref().map_or(1, Recovery::failures),
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
    pub fn params(&self) -> Params<'a> {
        Params::new(self.node, self.hosted)
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

/// A hosted plugin's states ([`Plugin::state`]): what the session holds
/// of them, a save now, and a restore.
#[derive(Debug, Clone, Copy)]
pub struct StateView<'a> {
    hosted: &'a HostedPlugin,
}

impl<'a> StateView<'a> {
    /// The last state captured while the plugin was healthy: the
    /// crash-recovery copy a reload restores, up to a snapshot interval
    /// stale. A project is saved from [`save`](Self::save), not from this.
    pub fn last_good(&self) -> Option<&'a PeerState> {
        self.hosted.snapshot.as_ref()
    }

    /// The state a reload left out because the plugin died loading it, or
    /// soon after ([`StateRestore::Withheld`]): the user's last state, which a
    /// host may still offer to load by hand. Kept until the plugin is
    /// withheld from again or replaced by the host; the states captured since
    /// (of a plugin at its defaults) do not replace it.
    pub fn withheld(&self) -> Option<&'a PeerState> {
        self.hosted.withheld.as_ref()
    }

    /// Saves the plugin's state for `state_use` now, as a blocking round trip
    /// to it (ten seconds at worst; an in-process plugin's is run by the main
    /// thread's next tick, see the module docs). A pooled plugin's state goes through
    /// [`Session::save_state`], a job that lands in an update.
    ///
    /// # Errors
    ///
    /// The plugin's [`StateError`]: it is gone, busy, refused, or has no state
    /// route.
    pub fn save(&self, state_use: StateUse) -> Result<PeerState, StateError> {
        let bytes = self.hosted.handle.save_state(state_use)?;
        Ok(PeerState::of_plugin(self.hosted.plugin.clone(), bytes))
    }

    /// Restores `state` into the plugin. The parameter mirror is read again by
    /// the next `Phase::Plugins`, whatever values the state moved. A pooled
    /// plugin's state goes through [`Session::restore_state`], a job.
    ///
    /// # Errors
    ///
    /// [`RestoreError::OtherPeer`] when `state` was saved from another peer
    /// (nothing is sent to the plugin); otherwise the plugin's own
    /// [`RestoreError::State`].
    pub fn restore(&self, main: MainThreadMarker, state: &PeerState) -> Result<(), RestoreError> {
        state
            .check_for(&SavedPeer::Plugin(self.hosted.plugin.clone()))
            .map_err(RestoreError::OtherPeer)?;
        self.hosted.handle.load_state(main, state.bytes())?;
        self.hosted.refresh_values();
        Ok(())
    }
}

/// Why [`StateView::restore`] restored nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreError {
    /// The state was saved from another peer: nothing was sent to the plugin.
    #[error(transparent)]
    OtherPeer(OtherPeer),
    /// The plugin was asked and could not restore it.
    #[error(transparent)]
    State(#[from] StateError),
}

/// `Phase::RenderMode`'s record: the mode last announced.
#[derive(Debug)]
pub(crate) struct RenderModeState {
    pub(crate) announced: tutti_plugin::RenderMode,
}

impl Default for RenderModeState {
    fn default() -> Self {
        Self {
            announced: tutti_plugin::RenderMode::Realtime,
        }
    }
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// Applies what a hosted plugin's job ended with.
    pub(crate) fn land_plugin_job(
        &mut self,
        landing: PluginLanding<'_>,
        out: &mut Vec<SessionEvent>,
    ) {
        match landing.0 {
            Landing::Reload {
                node,
                token,
                snapshot_age,
                loaded,
            } => self.land_reload(node, token, snapshot_age, *loaded, out),
            Landing::ReloadFailed {
                node,
                token,
                failed,
            } => self.reload_job_failed(node, token, failed, out),
            Landing::Params {
                node,
                token,
                values,
            } => {
                self.land_params(node, token, values);
            }
            Landing::Snapshot {
                node,
                token,
                asked,
                state,
            } => {
                self.land_snapshot(node, token, asked, state);
            }
        }
    }

    /// Puts an opened plugin in the graph, unwired, and hosts it: the one
    /// place a plugin enters the session, through
    /// [`add_pooled_plugin`](Self::add_pooled_plugin).
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
                let (key, rebinder) = self.graph.insert_plugin_with_rebinder(client);
                if let Some(hosted) = self.graph.plugins.get_mut(&key.erase()) {
                    hosted.origin = Some(origin);
                    hosted.recovery = self.plugins.recovery.map(Recovery::new);
                }
                (key, rebinder)
            }
            Err(plugin) => {
                let node_controls = plugin.node_controls();
                node_controls.set_meter(meter);
                let (bound, rebinder) = (*plugin).bind_with_rebinder(self.graph.tempo.clone());
                let (node, _) = self.graph.place(bound);
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
    /// node's plugin; the entry's handle ([`plugin`](Self::plugin)) is the
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
    /// to another plugin since the job was asked for) lands nowhere: it is
    /// the outgoing plugin's state, not a recovery point for the node.
    pub(crate) fn land_snapshot(
        &mut self,
        node: NodeKey,
        token: &Arc<()>,
        asked: Instant,
        state: Result<Vec<u8>, StateError>,
    ) -> bool {
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(token, &hosted.snapshot_token) {
            tracing::debug!("plugin {node:?}: a snapshot of its previous plugin was discarded");
            return false;
        }
        match state {
            Ok(bytes) => {
                hosted.snapshot = Some(PeerState::of_plugin(hosted.plugin.clone(), bytes));
                hosted.snapshot_from = Some(asked);
                true
            }
            Err(e) => {
                tracing::warn!("plugin {node:?}: periodic state snapshot failed: {e}");
                false
            }
        }
    }

    /// Lands a finished parameter read: the plugin's mirror holds its
    /// values and is ready. Returns whether it landed. A read of a plugin no
    /// longer at its node (removed, or crossfaded again since) lands
    /// nowhere. A read the plugin did not answer (its list unavailable) does
    /// not land either: the mirror stays not ready, the read goes back to the
    /// plugin's record, and the next `Phase::Plugins` submits it again.
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
    /// main thread, and a served plugin's replacement must not be one, so a
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
            Opened::OnMain(_) => {
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
            (Some(Err(reason)), Some(age)) => StateRestore::Refused { age, reason },
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
        let recovery = hosted.recovery.take();
        let latency_before = hosted.reload.latency_before;
        let withheld = hosted.reload.withheld_age;
        let attempts = recovery.as_ref().map_or(1, Recovery::failures);
        match self.graph.replace_plugin_with_rebinder(
            node,
            client,
            tutti_graph::DECLICK,
            tutti_graph::FadeLaw::default(),
        ) {
            Ok(rebinder) => {
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.set_rebinder(rebinder);
                }
            }
            Err(crate::ReplaceRefused::Busy(client)) => {
                // Between a re-prepare's two commits: try again next phase.
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.snapshot = snapshot;
                    hosted.withheld = withheld_bytes;
                    hosted.recovery = recovery;
                    hosted.reload.ready = Some(Ready {
                        client,
                        restore,
                        mirror,
                    });
                }
                return;
            }
            Err(crate::ReplaceRefused::Failed(refused)) => {
                tracing::error!("plugin {node:?}: the replacement was refused: {refused}");
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.snapshot = snapshot;
                    hosted.withheld = withheld_bytes;
                    hosted.recovery = recovery;
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
        hosted.snapshot = snapshot;
        hosted.withheld = withheld_bytes;
        hosted.snapshot_from = snapshot_from;
        hosted.recovery = recovery;
        if let Some(recovery) = hosted.recovery.as_mut() {
            recovery.step(RecoveryEvent::Loaded, now);
        }
        // The replacement's own read, after the restore, is the mirror: the
        // swap built an unread one, and nothing of the old plugin's values
        // carries over (they are the values read at load, which the restored
        // state has replaced; writing them back would undo the restore and
        // bypass the lanes that own those parameters).
        //
        // A modulation lane on a parameter needs no seeding here: the swap
        // moved the node's generation, so the reconciler rebuilds the lane (a
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
        hosted.probation_until = match (&restore, hosted.recovery.as_ref()) {
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

    /// One reload attempt failed (or its job was dropped): the next backoff,
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
        let Some(recovery) = hosted.recovery.as_mut() else {
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
    /// off, or recovery gave up). The node of a pooled plugin stays at its
    /// key, silent in the plugin's place, and the entry reads lost
    /// ([`PluginCause`]) until a `retry` brings a plugin to it; any other
    /// plugin's node goes.
    fn end_node(&mut self, node: NodeKey) {
        if self
            .graph
            .plugins
            .get(&node)
            .is_some_and(|hosted| hosted.pooled.is_some())
        {
            self.silence_node(node);
        } else {
            let _ = self.remove(node); // gone already is as good as removed
        }
    }

    /// Puts the silent stand-in at `node`, which releases the dead plugin on
    /// the control side. A graph that is re-preparing is asked again at the
    /// next phase.
    fn silence_node(&mut self, node: NodeKey) {
        let done = match self.graph.silence_plugin(node) {
            Ok(done) => done,
            Err(refused) => {
                tracing::warn!("plugin {node:?}: its node could not be silenced: {refused}");
                true // nothing more to ask of a graph that refuses
            }
        };
        if done && let Some(hosted) = self.graph.plugins.get_mut(&node) {
            hosted.silenced = true;
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
            if let Some(link) = self
                .graph
                .plugins
                .get(&node)
                .and_then(|h| h.pooled.as_ref())
            {
                link.gave_up(why.clone());
            }
            self.end_node(node);
            out.push(event(
                node,
                PluginChange::GaveUp {
                    why,
                    state: state.map(Box::new),
                },
            ));
        }
    }

    /// One phase's work on a plugin being replaced: install a replacement
    /// that was waiting, count a dropped or overdue job as failed, or hand
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
        let (tick, load_timeout) = hosted
            .recovery
            .as_mut()
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
        let (Some(origin), Some(due)) = (hosted.origin.clone(), hosted.reload.due) else {
            return;
        };
        if hosted.reload.in_flight || now < due {
            return;
        }
        hosted.reload.due = None;
        hosted.reload.in_flight = true;
        let load = PluginLoad {
            loadable: origin.loadable.retargeted(hosted.plugin.clone()),
            rate,
            config: origin.config,
            state: hosted.snapshot.clone().map(Arc::new),
            work: self.plugins.work.clone(),
        };
        let token = Arc::clone(&hosted.reload.token);
        let snapshot_age = hosted
            .snapshot_from
            .map(|t| now.saturating_duration_since(t));
        self.jobs.submit(
            move || load.run(),
            move |session, ended, out| match ended {
                Ok(loaded) => session.land_plugin(
                    Landing::Reload {
                        node,
                        token: &token,
                        snapshot_age,
                        loaded: Box::new(loaded),
                    },
                    out,
                ),
                Err(failed) => session.land_plugin(
                    Landing::ReloadFailed {
                        node,
                        token: &token,
                        failed,
                    },
                    out,
                ),
            },
        );
    }

    /// The reload's job ended without a result (the spawner dropped it, or it
    /// panicked): a failed attempt, unless the attempt no longer applies.
    pub(crate) fn reload_job_failed(
        &mut self,
        node: NodeKey,
        token: &Arc<()>,
        failed: WorkerError,
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
        let cause = match failed {
            WorkerError::Panicked { message } => RecoveryFailure::ReloadPanicked { message },
            // `Dropped`, and any end a later version names: no replacement
            // came of the job.
            _ => RecoveryFailure::ReloadDropped,
        };
        let mut gave_up = Vec::new();
        self.fail_reload(node, cause, now, out, &mut gave_up);
        self.finish_gave_up(gave_up, out);
    }

    /// The render mode last announced to the hosted plugins.
    pub fn render_mode(&self) -> tutti_plugin::RenderMode {
        self.plugins.render_mode.announced
    }

    /// `Phase::Plugins`. See the module docs.
    pub(crate) fn plugins_phase(&mut self, cx: crate::PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        self.graph.graveyard.warn_if_piling();
        self.graph.reconcile_own_bypass();
        let now = cx.now;
        self.plugins.now = now;
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
                if hosted.pooled.is_some() && !hosted.silenced {
                    unsilenced.push(node);
                }
                continue;
            }
            // What the plugin reported since the last update, in the order
            // it reported it.
            let heard = std::mem::take(&mut *hosted.notices.lock_recover());
            // A rescan the plugin announced re-reads what it declares and the
            // value of each, as a job below.
            if heard.iter().any(|notice| {
                matches!(
                    notice,
                    PluginSignal::Refresh(PluginRefresh::ParamValues | PluginRefresh::ParamTitles)
                )
            }) {
                hosted.reread_params();
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
            if let (Some(cause), Some(link)) = (&cause, hosted.pooled.as_ref()) {
                link.died(cause.clone());
            }
            if matches!(hosted.status, PluginLiveness::Recovering { .. }) {
                self.drive_reload(node, now, out, &mut gave_up);
                continue;
            }
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
                    let action = hosted.recovery.as_mut().map(|r| {
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
                (hosted.handle.clone(), Arc::clone(&hosted.snapshot_token))
            });

            if let Some(read) = hosted.param_read.take() {
                let token = Arc::clone(&hosted.mirror_token);
                self.jobs.submit(
                    move || read.run(),
                    move |session, ended, out| match ended {
                        Ok(values) => session.land_plugin(
                            Landing::Params {
                                node,
                                token: &token,
                                values,
                            },
                            out,
                        ),
                        Err(failed) => {
                            tracing::warn!("plugin {node:?}: its parameter read ended: {failed}");
                        }
                    },
                );
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
            if let Some((handle, token)) = snapshot {
                let asked = now;
                self.jobs.submit(
                    move || handle.save_state(StateUse::Project),
                    move |session, ended, out| match ended {
                        Ok(state) => session.land_plugin(
                            Landing::Snapshot {
                                node,
                                token: &token,
                                asked,
                                state,
                            },
                            out,
                        ),
                        Err(failed) => {
                            tracing::warn!("plugin {node:?}: its state snapshot ended: {failed}");
                        }
                    },
                );
            }
        }
        for node in unsilenced {
            self.silence_node(node);
        }
        for (node, cause) in dead {
            self.end_node(node);
            out.push(event(node, PluginChange::Died { cause }));
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
                tutti_plugin::RenderMode::Offline
            } else {
                tutti_plugin::RenderMode::Realtime
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
    //! and a failing plugin's skipped snapshot. The session's spawner is a
    //! queue the test drains, so it runs, drops or holds each job. The death, the render mode
    //! and the export are pinned end to end over the CLAP test plugin, in
    //! `tests/plugin/plugin.rs`.

    use super::*;
    use crate::{RecoveryPolicy, Task};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tutti_plugin_types::ParameterInfo;
    use tutti_units::Normalized;
    use tutti_units::ParamId;

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
    }

    impl tutti_plugin_types::AnyThread for Fake {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
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
        fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
            self.params
                .iter()
                .find(|(a, _)| *a == id)
                .map(|&(_, v)| Normalized::clamped(f64::from(v)))
        }
        fn set_parameter_value(&self, _id: ParamId, _value: Normalized) {}
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(&self, _id: ParamId, _plain: f64) -> Option<Normalized> {
            None
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
        ) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.state
                .clone()
                .ok_or(tutti_plugin::error::StateError::NoStateRoute)
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
        fn rate_refusal(&self) -> Option<(tutti_units::SampleRate, String)> {
            None
        }
        fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
            tutti_plugin_types::BlockCounts::default()
        }
        fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
    }

    struct Rig {
        session: crate::DetachedSession,
        node: NodeKey,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        t0: Instant,
        /// What the session's spawner was handed and nobody has taken.
        held: Arc<Mutex<Vec<Task>>>,
    }

    /// A detached session snapshotting every second, hosting a fake that
    /// answers `state` at a dc node.
    fn rig(state: Option<Vec<u8>>) -> Rig {
        let held: Arc<Mutex<Vec<Task>>> = Arc::default();
        let queue = Arc::clone(&held);
        let mut session = Session::builder()
            .snapshot_interval(Duration::from_secs(1))
            .spawn_with(move |job| queue.lock_recover().push(job))
            .build_detached(SampleRate::SR_48K)
            .unwrap();
        let saves = Arc::new(AtomicUsize::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                state,
                saves: Arc::clone(&saves),
                dead: Arc::clone(&dead),
                params: Vec::new(),
                unlisted: Arc::default(),
            }),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let (node, _) =
            session
                .graph()
                .add(tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(
                    0.0,
                )));
        super::fake_hosting::host(&mut session, node, handle, PluginRef::file("fake.clap"));
        let node = node.erase();
        // The load's parameter read is not what these tests count jobs of.
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
            held,
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
                state,
                saves: Arc::default(),
                dead: Arc::default(),
                params,
                unlisted: Arc::default(),
            }),
            None,
            Default::default(),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    impl Rig {
        /// `Phase::Plugins` at `t0 + ms`: every event it reported.
        fn events_at(&mut self, ms: u64) -> Vec<SessionEvent> {
            let mut out = Vec::new();
            let cx = crate::PhaseCx::at(self.t0 + Duration::from_millis(ms));
            self.session.run_phase(crate::Phase::Plugins, cx, &mut out);
            out
        }

        /// `Phase::Plugins` at `t0 + ms`: the jobs it submitted.
        fn plugins_at(&mut self, ms: u64) -> Vec<Task> {
            let _ = self.events_at(ms);
            std::mem::take(&mut *self.held.lock_recover())
        }

        /// `Phase::Collect`: what the jobs sent lands; the events it reported.
        fn land(&mut self) -> Vec<SessionEvent> {
            let mut out = Vec::new();
            let cx = crate::PhaseCx::at(self.t0);
            self.session.run_phase(crate::Phase::Collect, cx, &mut out);
            out
        }

        /// Runs `job` on another thread, and lands its end.
        fn run_and_land(&mut self, job: Task) -> Vec<SessionEvent> {
            std::thread::spawn(move || job.run()).join().unwrap();
            self.land()
        }

        /// The node's record as a plugin being recovered, its reload due now,
        /// whose load resolves in process (CLAP, `Hosting::InProcess`).
        #[cfg(feature = "clap")]
        fn recovering_in_process(&mut self) {
            let plugin = PluginRef::file("fake.clap");
            self.recovering(
                plugin.clone(),
                Loadable::uncatalogued(plugin).hosting(Hosting::InProcess),
            );
        }

        /// The node's record as `plugin` being recovered, its reload due now,
        /// from a load that was `loadable`.
        fn recovering(&mut self, plugin: PluginRef, loadable: Loadable) {
            let hosted = self.session.graph.plugins.get_mut(&self.node).unwrap();
            hosted.plugin = plugin;
            hosted.origin = Some(Origin {
                config: AudioConfig::default(),
                loadable,
            });
            let mut recovery = Recovery::new(RecoveryPolicy::default());
            let _ = recovery.step(
                RecoveryEvent::Died {
                    cause: PluginFailure::StreamClosed,
                },
                self.t0,
            );
            hosted.recovery = Some(recovery);
            hosted.status = PluginLiveness::Recovering {
                cause: ReloadCause::Died(PluginFailure::StreamClosed),
            };
            hosted.reload.due = Some(self.t0);
        }
    }

    /// **Snapshots are taken on the interval of `now`, not every update,
    /// and one at a time.** The first update starts the clock; updates
    /// inside the interval ask for nothing; the first at or past it asks
    /// once; while that job is unlanded nothing more is asked, however late
    /// (the job is held, not run); landing it stores the bytes and
    /// restarts the cadence; a dropped job frees the plugin too.
    #[test]
    fn snapshots_are_taken_on_the_interval_one_at_a_time() {
        let mut rig = rig(Some(vec![1, 2, 3]));
        assert!(
            rig.plugins_at(0).is_empty(),
            "the first update starts the clock"
        );
        assert!(rig.plugins_at(500).is_empty(), "inside the interval");
        let mut jobs = rig.plugins_at(1_000);
        assert_eq!(jobs.len(), 1, "one job at the interval");
        let job = jobs.pop().unwrap();
        assert!(
            rig.plugins_at(5_000).is_empty(),
            "nothing more while the job is in flight"
        );
        rig.run_and_land(job);
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PeerState::bytes),
            Some(&[1u8, 2, 3][..])
        );
        assert_eq!(rig.saves.load(Ordering::SeqCst), 1, "one job is one save");
        assert_eq!(
            rig.plugins_at(5_500).len(),
            1,
            "the cadence restarted when the last job was asked for (at 1 s)"
        );
        // That job was dropped unrun (a cancelled task) and its end landed:
        // the plugin is free again, on the interval.
        rig.land();
        assert!(rig.plugins_at(6_000).is_empty(), "inside the interval");
        assert_eq!(rig.plugins_at(6_500).len(), 1, "a dropped job frees it");
    }

    /// **A declined save keeps the previous snapshot.** A refusal (the
    /// plugin declines, or the bridge dropped mid-call) replacing a good
    /// recovery point with nothing is worse than a stale one.
    #[test]
    fn a_declined_save_keeps_the_previous_snapshot() {
        let mut rig = rig(None);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.snapshot = Some(PeerState::of_plugin(hosted.plugin.clone(), vec![4, 2]));
        rig.plugins_at(0);
        let job = rig.plugins_at(1_000).pop().expect("a job at the interval");
        rig.run_and_land(job);
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PeerState::bytes),
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
        rig.plugins_at(0);
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
        rig.plugins_at(0);
        rig.dead.store(true, Ordering::SeqCst);
        assert!(rig.plugins_at(1_000).is_empty(), "failing: no snapshot");
        assert!(matches!(
            rig.session.plugin_view(rig.node).unwrap().health(),
            PluginHealth::Died { .. }
        ));
    }

    /// **A hosted plugin replaced by a native node is no longer hosted**
    /// (`Graph::replace`, and `Graph::crossfade` through it): its record
    /// goes, so a later death of the outgoing plugin removes nothing (the
    /// node is the native one's now) and reports nothing, and the record's
    /// handle no longer keeps the outgoing plugin alive.
    #[test]
    fn a_plugin_replaced_by_a_native_node_is_no_longer_hosted() {
        for crossfade in [false, true] {
            let mut rig = rig(None);
            let replacement = tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(1.0));
            let fade = tutti_units::Seconds::new(0.0);
            let law = tutti_graph::FadeLaw::EqualAmplitude;
            if crossfade {
                assert!(matches!(
                    rig.session
                        .graph()
                        .crossfade(rig.node, replacement, fade, law),
                    Ok(crate::Crossfade::Landed(_))
                ));
            } else {
                assert!(
                    rig.session
                        .graph()
                        .replace(rig.node, replacement, fade, law)
                        .is_ok()
                );
            }
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
                assert!(
                    !died,
                    "a native node is reported dead (crossfade: {crossfade})"
                );
            }
            assert!(
                rig.session.graph_ref().contains(rig.node),
                "the native replacement stays in the graph (crossfade: {crossfade})"
            );
        }
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
                    state: None,
                    saves: Arc::default(),
                    dead: Arc::default(),
                    params: Vec::new(),
                    unlisted: Arc::default(),
                }),
                Some(control),
                Default::default(),
                Default::default(),
                Default::default(),
            );
            let mut record =
                HostedPlugin::new(outgoing, PluginRef::file("fake.clap"), node_controls());
            let mut yard = Graveyard::default();
            let node = NodeKey::fresh();
            let mut editor = yard.editors.make(node, record.handle.clone(), main);
            if open {
                editor
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
    /// snapshot job of the outgoing plugin still in flight lands nowhere
    /// (its token is not the record's any more); the incoming plugin gets
    /// its own on the interval from the crossfade. Driven through
    /// `HostedPlugin::adopt`, `replace`'s handle half (a crossfade to
    /// another plugin needs a `PluginClient`, a real `plugin-server`).
    #[test]
    fn a_crossfade_to_another_plugin_drops_the_old_snapshot() {
        let mut rig = rig(Some(vec![1, 2, 3]));
        rig.plugins_at(0);
        let first = rig.plugins_at(1_000).pop().expect("a job at the interval");
        rig.run_and_land(first);
        let in_flight = rig.plugins_at(2_000).pop().expect("the next job");

        let (incoming, _) = fake_handle(Some(vec![9]), Vec::new());
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming, Placement::InProcess);
        // The incoming plugin's parameter read is not what this counts jobs of.
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
                .map(PeerState::bytes),
            None,
            "the outgoing plugin's snapshot is not the incoming one's"
        );
        rig.run_and_land(in_flight);
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PeerState::bytes),
            None,
            "the outgoing plugin's job must not land on the incoming plugin"
        );

        assert!(rig.plugins_at(2_000).is_empty(), "its clock starts here");
        let own = rig.plugins_at(3_000).pop().expect("its own job");
        rig.run_and_land(own);
        assert_eq!(
            rig.session
                .plugin_view(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PeerState::bytes),
            Some(&[9u8][..])
        );
    }

    /// **A crossfade to another plugin rebuilds the parameter mirror from
    /// the incoming plugin, read off the control thread.** Right after the
    /// crossfade the mirror is not ready (a lane waits rather than start
    /// from the outgoing plugin's value); the next `Phase::Plugins` submits
    /// the read to the spawner as a job, once; the job runs on another thread
    /// and lands the incoming plugin's values in a later update. A read asked for before a
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
        mirror.land(read.run().expect("the read lands"));
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

        let mut jobs = rig.plugins_at(0);
        assert_eq!(jobs.len(), 1, "the read is submitted as a job");
        assert!(rig.plugins_at(10).is_empty(), "once");
        let job = jobs.pop().unwrap();
        rig.run_and_land(job);
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            Some(Some(0.75)),
            "the mirror holds the incoming plugin's values"
        );

        // A read asked for, then a second crossfade: the first lands nowhere.
        let (second, _) = fake_handle(None, vec![(param, 0.5)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(second, Placement::InProcess);
        let stale = rig.plugins_at(20).pop().expect("its read");
        let (third, _) = fake_handle(None, vec![(param, 0.125)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(third, Placement::InProcess);
        rig.run_and_land(stale);
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            None,
            "a read of the previous plugin must not land"
        );
        let own = rig.plugins_at(30).pop().expect("its own read");
        rig.run_and_land(own);
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            Some(Some(0.125))
        );
    }

    /// **A read whose parameter list is unavailable does not make the mirror
    /// ready, and is submitted again.** The job lands as not landed (the
    /// mirror stays not ready), the next phase submits the read for the
    /// same plugin once more, and once the list answers the re-run read lands
    /// the values.
    #[test]
    fn a_read_whose_parameter_list_is_unavailable_is_submitted_again() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (outgoing, _) = fake_handle(None, vec![(param, 0.25)]);
        let (mut mirror, read) = tutti_plugin_host::PluginParamValues::start(&outgoing);
        mirror.land(read.run().expect("the read lands"));
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.mirror = mirror;

        let unlisted = Arc::new(AtomicBool::new(true));
        let incoming = PluginHandle::from_backend(
            Arc::new(Fake {
                state: None,
                saves: Arc::default(),
                dead: Arc::default(),
                params: vec![(param, 0.75)],
                unlisted: Arc::clone(&unlisted),
            }),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        hosted.adopt(incoming, Placement::InProcess);

        let job = rig.plugins_at(0).pop().expect("the read is submitted");
        rig.run_and_land(job);
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            None,
            "the mirror is not ready"
        );

        let job = rig
            .plugins_at(10)
            .pop()
            .expect("the read comes round again");
        unlisted.store(false, Ordering::SeqCst);
        rig.run_and_land(job);
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(ParamId::new(3)),
            Some(Some(0.75)),
            "the re-run read filled the mirror"
        );
        assert!(rig.plugins_at(20).is_empty(), "and it is done");
    }

    /// **A reload job that ends without a result is a failed attempt, named by
    /// how it ended.** A job the spawner dropped is `ReloadDropped`, one that
    /// panicked `ReloadPanicked` with its message; either schedules the next
    /// attempt (`Reloading`). The end of an attempt that was already replaced
    /// (its token is not the record's any more) changes nothing.
    #[test]
    fn a_reload_job_that_ends_without_a_result_is_a_failed_attempt() {
        for failed in [
            WorkerError::Dropped,
            WorkerError::Panicked {
                message: "boom".into(),
            },
        ] {
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
            hosted.recovery = Some(recovery);
            hosted.status = PluginLiveness::Recovering {
                cause: ReloadCause::Died(PluginFailure::StreamClosed),
            };
            hosted.reload.in_flight = true;
            let token = Arc::clone(&hosted.reload.token);
            rig.session.plugins.now = now;

            let mut out = Vec::new();
            rig.session
                .reload_job_failed(node, &token, failed.clone(), &mut out);
            let [
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading { cause, .. },
                    ..
                }),
            ] = &out[..]
            else {
                panic!("the next attempt is scheduled: {out:?}");
            };
            match (&failed, cause) {
                (WorkerError::Dropped, ReloadCause::Attempt(RecoveryFailure::ReloadDropped)) => {}
                (
                    WorkerError::Panicked { message },
                    ReloadCause::Attempt(RecoveryFailure::ReloadPanicked { message: got }),
                ) => assert_eq!(got, message),
                other => panic!("{other:?}"),
            }

            // The same end again, with the attempt's old token: nothing.
            let mut again = Vec::new();
            rig.session
                .reload_job_failed(node, &token, failed, &mut again);
            assert!(again.is_empty(), "a replaced attempt's end is stale");
        }
    }

    /// **A reload of a plugin whose load resolves in process is refused as
    /// `ReplacementInProcess`, with no main thread and nothing opened.** The
    /// decision is `Loadable::placement` (a pure resolve, no module
    /// read): `PluginLoad::run` returns such a load unopened (`Opened::OnMain`),
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
        let job = rig.plugins_at(0).pop().expect("the reload is submitted");
        let events = rig.run_and_land(job);
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
        use tutti_plugin::catalog::{Catalog, CatalogConfig, PluginDb};
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
        let job = rig.plugins_at(0).pop().expect("the reload is submitted");

        catalog.blacklist(&file, "crashed the scanner");
        let events = rig.run_and_land(job);
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
            matches!(&**source, BridgeError::Blacklisted { reason, .. } if reason == "crashed the scanner"),
            "{source:?}"
        );
    }

    /// **A reload job that outlives its node lands nowhere, silently.** The
    /// spawner holds the job; the node is removed; the job then runs and its end
    /// lands: no event, and no failure recorded for a node that is gone.
    #[cfg(feature = "clap")]
    #[test]
    fn a_reload_job_that_outlives_its_node_lands_quietly() {
        let mut rig = rig(None);
        rig.recovering_in_process();
        let job = rig.plugins_at(0).pop().expect("the reload is submitted");
        rig.session.remove(rig.node).expect("the node is removed");
        assert!(
            rig.session.plugin_view(rig.node).is_err(),
            "its record is gone"
        );
        let events = rig.run_and_land(job);
        assert!(events.is_empty(), "nothing is reported: {events:?}");
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
