//! [`PluginPart`] with the `plugin` feature: the policies a hosted plugin is served by, the
//! clock its phase was last run at, and the collector of the work other threads hand to the
//! main thread.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tutti_platform::{Executor, Instant, WeakExecutor};

use tutti_buffers::Float;
use tutti_plugin_types::{MainThreadMarker, MainWork, MainWorkHandle, Timeouts};
use tutti_rt::LockExt;

use crate::graph::Graph;
use crate::plugin::{Inbox, RenderModeState, SNAPSHOT_INTERVAL};
use crate::{HangPolicy, RecoveryPolicy};

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

/// A loop the session runs for a hosted plugin: which round trip a task is inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loop {
    /// A crash recovery's reload: a server launch and the state restore.
    Reload,
    /// A parameter mirror's read.
    ParamRead,
    /// A state snapshot.
    Snapshot,
}

impl Loop {
    fn name(self) -> &'static str {
        match self {
            Self::Reload => "reload",
            Self::ParamRead => "parameter read",
            Self::Snapshot => "state snapshot",
        }
    }

    /// The round trip's own IPC deadline in `timeouts`, the plugin's configured ones: how
    /// long a session's close waits for a task of this loop (`load`, `param`, `state`).
    pub(crate) fn deadline(self, timeouts: &Timeouts) -> Duration {
        match self {
            Self::Reload => timeouts.load,
            Self::ParamRead => timeouts.param,
            Self::Snapshot => timeouts.state,
        }
    }
}

/// How long a close that is pumping the main thread's work sleeps on its condvar between
/// two pumps: an in-process plugin's task waits for the main thread to run its call. The
/// main thread's work queue cannot notify the condvar, so the wait wakes on a slice; it
/// runs only during a close or drop with a task running.
const PUMP_SLICE: Duration = Duration::from_millis(5);

/// The plugin tasks in flight, from the spawn that queued one to the end of its closure.
#[derive(Clone, Debug, Default)]
pub(crate) struct Tasks(Arc<TaskCount>);

#[derive(Debug, Default)]
struct TaskCount {
    state: Mutex<InFlight>,
    /// Notified as a task ends.
    idle: Condvar,
    /// Spawns the executor refused, since the session began.
    refused: AtomicU64,
}

#[derive(Debug, Default)]
struct InFlight {
    /// Tasks queued or running.
    count: usize,
    /// Tasks that began to run and have not ended: what a close waits for.
    running: usize,
    /// The latest of the running tasks' deadlines, each counted from when it began.
    until: Option<Instant>,
    /// Set when a close or drop begins: a task that starts after it returns at once.
    closing: bool,
}

/// A task's place in the count: dropped when its closure ends, however it ends, and with
/// the closure when the executor drops it unrun.
#[must_use]
pub(crate) struct TaskGuard {
    tasks: Tasks,
    /// How long a close waits for the task once it runs: its round trip's IPC deadline.
    bound: Duration,
    running: bool,
}

impl TaskGuard {
    /// The task's first step: whether it may run. A close that began before the task was
    /// picked up from the pool's queue ends it here, unrun; otherwise the task counts as
    /// running, with its deadline counted from now.
    fn begin(&mut self) -> bool {
        let mut state = self.tasks.0.state.lock_recover();
        if state.closing {
            return false;
        }
        state.running += 1;
        let deadline = Instant::now() + self.bound;
        state.until = Some(state.until.map_or(deadline, |until| until.max(deadline)));
        self.running = true;
        true
    }
}

impl Tasks {
    fn enter(&self, bound: Duration) -> TaskGuard {
        self.0.state.lock_recover().count += 1;
        TaskGuard {
            tasks: self.clone(),
            bound,
            running: false,
        }
    }

    /// Marks a close begun: a task not yet running never runs.
    pub(crate) fn close(&self) {
        self.0.state.lock_recover().closing = true;
    }

    /// How many tasks are queued or running.
    pub(crate) fn in_flight(&self) -> usize {
        self.0.state.lock_recover().count
    }

    /// How many spawns the executor refused.
    pub(crate) fn refused(&self) -> u64 {
        self.0.refused.load(Ordering::Relaxed)
    }

    /// Waits for every running task to end, or until the latest of their deadlines:
    /// whether it did. A task still queued is not waited for: it ends unrun once
    /// [`close`](Self::close) was called. `pump` runs between waits when the caller is on the main thread, so
    /// an in-process plugin's task, which waits for the main thread, can end.
    pub(crate) fn wait_idle(&self, mut pump: Option<&mut dyn FnMut()>) -> bool {
        let mut state = self.0.state.lock_recover();
        loop {
            if state.running == 0 {
                return true;
            }
            let left = state.until.map_or(Duration::ZERO, |until| {
                until.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                return false;
            }
            let slice = if pump.is_some() {
                left.min(PUMP_SLICE)
            } else {
                left
            };
            state = self
                .0
                .idle
                .wait_timeout(state, slice)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            if let Some(pump) = pump.as_mut() {
                drop(state);
                pump();
                state = self.0.state.lock_recover();
            }
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut state = self.tasks.0.state.lock_recover();
        state.count -= 1;
        if self.running {
            state.running -= 1;
        }
        if state.running == 0 {
            state.until = None;
        }
        drop(state);
        self.tasks.0.idle.notify_all();
    }
}

/// Plugin hosting: what runs a hosted plugin's phases and what its closes hand over.
#[derive(Debug)]
pub(crate) struct PluginPart {
    /// How often a healthy plugin's state is captured.
    pub(crate) snapshot_interval: Duration,
    /// What a hosted plugin's death or hang does; `None`: a loaded plugin's
    /// node stays silent at its key, a host-inserted plugin's is removed.
    pub(crate) recovery: Option<RecoveryPolicy>,
    /// When a hosted plugin's unanswered blocks are a hang.
    pub(crate) hang: HangPolicy,
    /// The `now` of the last `Phase::Plugins`: the time a landing reload is
    /// measured at.
    pub(crate) now: Instant,
    /// The render mode last announced.
    pub(crate) render_mode: RenderModeState,
    /// What every plugin this session loads hands to the main thread: the closes of
    /// a drop off the main thread. The handle is cloned into each load.
    pub(crate) work: MainWorkHandle,
    /// The session's executor, reached by the loads and loops this session queues
    /// without keeping it alive: a task upgrades it only to spawn.
    pub(crate) executor: WeakExecutor,
    /// What the plugin loops sent back, drained by `Phase::Plugins`.
    pub(crate) inbox: Inbox,
    /// The loops' tasks in flight, which a close waits for.
    pub(crate) tasks: Tasks,
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
        let tasks = Tasks::default();
        let early = EarlyWork(main_work.clone(), tasks.clone());
        (
            Self {
                snapshot_interval: options.snapshot_interval,
                recovery: options.recovery,
                hang: options.hang,
                now: Instant::now(),
                render_mode: RenderModeState::default(),
                work,
                executor: executor.downgrade(),
                inbox: Inbox::new(),
                tasks,
                main_work,
            },
            early,
        )
    }

    /// Starts `task`, a blocking round trip of `kind` bounded by the plugin's `timeouts`, on
    /// the session's executor, and says whether it started.
    ///
    /// The task runs on the executor's blocking pool, which the session's other blocking work
    /// shares and which holds 32 threads: past them a task waits in the pool's queue, so a
    /// task starts when a thread is free, not when it is spawned. It counts in
    /// [`Tasks::in_flight`] from here to its end, queued or running.
    ///
    /// The executor is upgraded for this call only; the task holds none. A refused task
    /// (the executor shut down, or the OS refused a thread) is dropped unrun, which is how
    /// each loop's task answers a refusal: its tokens free the plugin for the next ask, a
    /// reload ends as dropped, and the refusal is counted in [`Tasks::refused`].
    #[must_use = "a refused task is the loop's to ask again"]
    pub(crate) fn spawn(
        &self,
        kind: Loop,
        timeouts: &Timeouts,
        task: impl FnOnce() + Send + 'static,
    ) -> bool {
        let guard = self.tasks.enter(kind.deadline(timeouts));
        let Some(executor) = self.executor.upgrade() else {
            self.tasks.0.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let started = executor.unblock(move || {
            let mut guard = guard;
            // A close that began while the task queued ends it unrun: its captures drop here.
            if guard.begin() {
                // The task's own captures drop when it returns, before the count does.
                task();
            }
            drop(guard);
        });
        match started {
            Ok(running) => {
                running.detach();
                true
            }
            Err(error) => {
                if self.tasks.0.refused.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        "a plugin {} was not started: {error}; refusals are counted, not repeated",
                        kind.name()
                    );
                }
                false
            }
        }
    }

    /// Starts `task`, a future an answer wakes (it never blocks a thread), of `kind` on the
    /// session's executor, and says whether it started. It counts in [`Tasks::in_flight`]
    /// from here to its end, and a close waits for it up to `kind`'s deadline, as for
    /// [`spawn`](Self::spawn); a close that began before its first poll ends it unrun.
    ///
    /// The executor is upgraded for this call only. A refused task is dropped unrun, and the
    /// refusal is counted in [`Tasks::refused`].
    #[must_use = "a refused task is the loop's to ask again"]
    pub(crate) fn spawn_future(
        &self,
        kind: Loop,
        timeouts: &Timeouts,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        let guard = self.tasks.enter(kind.deadline(timeouts));
        let Some(executor) = self.executor.upgrade() else {
            self.tasks.0.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        executor
            .spawn(async move {
                let mut guard = guard;
                if guard.begin() {
                    task.await;
                }
                drop(guard);
            })
            .detach();
        true
    }

    /// Waits for the plugin tasks in flight to end, each bounded by its round trip's
    /// deadline ([`Loop::deadline`]), pumping the main thread's work when `main` proves the
    /// caller is on it; past the bound it warns and goes on. What a task still holds then is
    /// dropped by the task, and an in-process plugin's close it hands over is counted in
    /// `MainWork::stranded()` if the collector has gone.
    pub(crate) fn drain_tasks(&self, main: Option<MainThreadMarker>) {
        self.tasks.close();
        let work = &self.main_work;
        let mut pump = main.map(|main| {
            move || {
                let _ = work.run(main);
            }
        });
        let drained = self
            .tasks
            .wait_idle(pump.as_mut().map(|p| p as &mut dyn FnMut()));
        if !drained {
            tracing::warn!(
                "{} plugin task(s) outlived their deadline; the session closes without them",
                self.tasks.in_flight()
            );
        }
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
pub(crate) struct EarlyWork(SharedWork, Tasks);

impl Drop for EarlyWork {
    fn drop(&mut self) {
        self.1.close();
        let main = MainThreadMarker::new();
        let work = &self.0;
        let mut pump = main.map(|main| {
            move || {
                let _ = work.run(main);
            }
        });
        // A close has waited already, and this returns at once. A plain drop waits here,
        // before any plugin closes: a task holds clones of its plugin's handle.
        if !self
            .1
            .wait_idle(pump.as_mut().map(|p| p as &mut dyn FnMut()))
        {
            tracing::warn!(
                "plugin task(s) outlived their deadline; the session drops without them"
            );
        }
        if let Some(main) = main {
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
