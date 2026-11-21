//! [`PluginPart`] with the `plugin` feature: the policies a hosted plugin is served by, the
//! clock its phase was last run at, and the collector of the work other threads hand to the
//! main thread.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::{Executor, Instant, WeakExecutor};

use tutti_buffers::Float;
use tutti_plugin_types::{MainThreadMarker, MainWork, MainWorkHandle};
use tutti_rt::LockExt;

use crate::graph::Graph;
use crate::plugin::{Inbox, RenderModeState, SNAPSHOT_INTERVAL};
use crate::pool::Defer;
use crate::{HangPolicy, PoolKey, RecoveryPolicy};

/// The plugin policies a builder sets.
#[derive(Debug)]
pub(crate) struct PluginOptions {
    /// How often a healthy plugin's state is captured.
    pub(crate) snapshot_interval: Duration,
    /// What a hosted plugin's death or hang does.
    pub(crate) recovery: Option<RecoveryPolicy>,
    /// When a hosted plugin's unanswered blocks are a hang.
    pub(crate) hang: HangPolicy,
}

impl Default for PluginOptions {
    fn default() -> Self {
        Self {
            snapshot_interval: SNAPSHOT_INTERVAL,
            recovery: Some(RecoveryPolicy::default()),
            hang: HangPolicy::default(),
        }
    }
}

/// Plugin hosting: what runs a hosted plugin's phases and what its closes hand over.
#[derive(Debug)]
pub(crate) struct PluginPart {
    /// How often a healthy plugin's state is captured.
    pub(crate) snapshot_interval: Duration,
    /// What a hosted plugin's death or hang does; `None`: a pooled plugin's
    /// node stays silent at its key, a host-inserted plugin's is removed.
    pub(crate) recovery: Option<RecoveryPolicy>,
    /// When a hosted plugin's unanswered blocks are a hang.
    pub(crate) hang: HangPolicy,
    /// The `now` of the last `Phase::Plugins`: the time a landing reload is
    /// measured at.
    pub(crate) now: Instant,
    /// The render mode last announced.
    pub(crate) render_mode: RenderModeState,
    /// The plugin entries whose node waits to swap to a reloaded plugin.
    pub(crate) pooled_swaps: BTreeSet<PoolKey>,
    /// What every plugin this session loads hands to the main thread: the closes of
    /// a drop off the main thread. The handle is cloned into each load.
    pub(crate) work: MainWorkHandle,
    /// The session's executor, reached by the loads and loops this session queues
    /// without keeping it alive: a task upgrades it only to spawn.
    pub(crate) executor: WeakExecutor,
    /// What the plugin loops sent back, drained by `Phase::Plugins`.
    pub(crate) inbox: Inbox,
    /// The receiving end of `work`, drained on the main thread by the session's
    /// update. Declared last, so it drops after everything the part holds: a
    /// close handed over while the session drops is still collected, and
    /// [`MainWork`]'s own drop runs what is left.
    pub(crate) main_work: SharedWork,
}

impl PluginPart {
    /// The part for a session built with `options`, and the guard that runs
    /// the work already handed over when the session drops. The guard is
    /// stored as `Session`'s first field. Fields drop in order, so it runs the
    /// work already handed over before any plugin closes, and the part's
    /// `main_work`, dropped last, runs what the closing plugins hand over.
    pub(crate) fn new(options: &PluginOptions, executor: &Executor) -> (Self, EarlyWork) {
        let (main_work, work) = MainWork::new();
        let main_work = SharedWork::new(main_work);
        let early = EarlyWork(main_work.clone());
        (
            Self {
                snapshot_interval: options.snapshot_interval,
                recovery: options.recovery,
                hang: options.hang,
                now: Instant::now(),
                render_mode: RenderModeState::default(),
                pooled_swaps: BTreeSet::new(),
                work,
                executor: executor.downgrade(),
                inbox: Inbox::new(),
                main_work,
            },
            early,
        )
    }

    /// Runs `task`, a blocking round trip of the loop `what`, on the session's executor.
    ///
    /// The executor is upgraded for this call only; the task holds none. A task the
    /// executor refuses (it shut down, or the OS refused a thread) is dropped unrun, which
    /// is how each loop's task answers a refusal: the token it holds frees the plugin for
    /// the next ask, and a reload ends as dropped.
    pub(crate) fn spawn(&self, what: &str, task: impl FnOnce() + Send + 'static) {
        let Some(executor) = self.executor.upgrade() else {
            tracing::warn!("a plugin {what} was not started: the executor has shut down");
            return;
        };
        match executor.unblock(task) {
            Ok(running) => running.detach(),
            Err(error) => tracing::warn!("a plugin {what} was not started: {error}"),
        }
    }

    /// What a main stage dropped off the main thread hands its discard to: this
    /// part's collector.
    pub(crate) fn main_deferral(&self) -> Option<Defer> {
        let work = self.work.clone();
        Some(Arc::new(move |discard| work.defer(discard)))
    }

    /// Runs the work other threads handed to the main thread, when `main` is the
    /// calling thread's proof.
    pub(crate) fn run_main_work(&self, main: Option<MainThreadMarker>) {
        if let Some(main) = main {
            self.main_work.run(main);
        }
    }

    /// What the main thread owns of the plugins the graph retired and of the open
    /// editors.
    pub(crate) fn service_retired<F: Float>(
        &self,
        graph: &mut Graph<F>,
        main: Option<MainThreadMarker>,
    ) {
        if let Some(main) = main {
            graph.graveyard.exhume(main);
            graph.graveyard.service(main);
        }
    }

    /// Ends the part on the main thread: runs the work handed over, and hands
    /// back the collector to run once the plugins have closed, leaving an
    /// empty one.
    pub(crate) fn take_collector(&mut self, main: MainThreadMarker) -> SharedWork {
        self.main_work.run(main);
        // The plugins close in the drop that follows and hand what they cannot do
        // there to their handles, which feed the collector kept out here;
        // the part keeps an empty one.
        let (inert, _) = MainWork::new();
        std::mem::replace(&mut self.main_work, SharedWork::new(inert))
    }
}

/// The fallback of `Session::close`, as a field: on the main thread, runs the
/// work other threads handed over, and the plugins then drop with the fields,
/// their closes collected by the last field's own drop.
///
/// End a session that hosted an in-process plugin with `close` on the main
/// thread; dropped elsewhere, it leaks those plugins and counts them in
/// `tutti_plugin_types::MainWork::stranded()`; `MainWork::deferred_drops()` and
/// `contained_panics()`, both in `tutti_plugin_types`, are the other two
/// process-wide readings. It is a field, not a `Drop` of the session, so a
/// session moves to another clock field by field.
#[derive(Debug)]
pub(crate) struct EarlyWork(SharedWork);

impl Drop for EarlyWork {
    fn drop(&mut self) {
        if let Some(main) = MainThreadMarker::new() {
            self.0.run(main);
        }
    }
}

/// The session's collector, held by the plugin part and the early-work guard:
/// the collector's receiving end is not `Sync`, so it sits behind a lock the
/// control thread alone takes.
#[derive(Clone, Debug)]
pub(crate) struct SharedWork(Arc<Mutex<MainWork>>);

impl SharedWork {
    fn new(work: MainWork) -> Self {
        Self(Arc::new(Mutex::new(work)))
    }

    /// [`MainWork::run`].
    pub(crate) fn run(&self, main: MainThreadMarker) -> usize {
        self.0.lock_recover().run(main)
    }
}
