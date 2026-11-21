//! The tick where a thread can start: one task on an executor's dedicated thread
//! running [`Shared::tick_once`](super::Shared::tick_once) until the set is shut.

#[cfg(not(loom))]
use std::io;
#[cfg(not(loom))]
use std::time::Duration;

#[cfg(not(loom))]
use tutti_platform::{Executor, Task, join_within};

#[cfg(not(loom))]
use super::Arc;
use super::{Ordering, Parker, Shared, State};

/// How long the drop waits for the task to run its last pass and end.
#[cfg(not(loom))]
const JOIN_BOUND: Duration = Duration::from_secs(1);

/// The task, held for its drop, which waits for it to end.
///
/// A loom build has no executor (`tutti-platform` leaves it out): its models step the
/// tick by hand and hold no task.
pub(super) struct Tick {
    #[cfg(not(loom))]
    task: Option<Task<()>>,
}

#[cfg(not(loom))]
impl Tick {
    /// Spawns the tick on a dedicated thread of `executor`.
    ///
    /// The `Parker` binds the thread of its first park. The future first polls on
    /// the dedicated thread and never leaves it, so the binding holds.
    ///
    /// The `yield_now` after each pass returns to the executor's drive loop, which
    /// checks its stop. An executor that shuts down unparks its threads, which wakes
    /// the parker's `std::thread::park`; the next yield then ends the drive. An
    /// executor dropped while the bells live therefore never waits out its stop
    /// bound on a parked tick.
    pub(super) fn start(
        shared: &Arc<Shared>,
        mut parker: Parker,
        executor: &Executor,
    ) -> io::Result<Self> {
        let shared = Arc::clone(shared);
        let task = executor.spawn_dedicated("tutti-bells", None, async move {
            while shared.tick_once(&mut parker) {
                tutti_platform::yield_now().await;
            }
        })?;
        Ok(Self { task: Some(task) })
    }
}

impl Tick {
    /// The task ticks: nothing to do here.
    pub(super) fn run(&self, _shared: &Shared) {}

    /// The task runs the last pass on its way out: nothing to do here.
    pub(super) fn shut(&self, _shared: &Shared) {}

    #[cfg(any(test, loom))]
    pub(super) fn none() -> Self {
        Self {
            #[cfg(not(loom))]
            task: None,
        }
    }

    #[cfg(all(test, not(loom)))]
    pub(super) fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }
}

#[cfg(not(loom))]
impl Drop for Tick {
    /// Runs after `Bells`' drop has shut the set and unparked: the task runs the last
    /// pass and ends, and the drop waits for it.
    fn drop(&mut self) {
        if let Some(task) = self.task.take()
            && let Err(task) = join_within(task, JOIN_BOUND)
        {
            tracing::warn!(
                "the bells' tick task did not end within {JOIN_BOUND:?} of the drop: \
                 its last pass may not have run"
            );
            task.detach();
        }
    }
}

impl State {
    /// Whether the tick has a waker to wake: rung, and someone waiting.
    fn wakeable(&self) -> bool {
        self.rung.load(Ordering::Acquire) && self.waiting.load(Ordering::Acquire)
    }
}

impl Shared {
    /// Some bell is rung and has a waiter: the tick task's pending check.
    pub(super) fn pending(&self) -> bool {
        self.bells().iter().any(|state| state.wakeable())
    }

    /// Park until a bell has a waiter to wake or the set is shut, then run
    /// one pass; `false` once shut, after the last pass. The tick task loops on it.
    pub(super) fn tick_once(&self, parker: &mut Parker) -> bool {
        parker.park(|| self.shut.load(Ordering::Acquire) || self.pending());
        if self.shut.load(Ordering::Acquire) {
            self.last_pass();
            return false;
        }
        self.pass();
        true
    }
}
