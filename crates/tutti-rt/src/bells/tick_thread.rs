//! The tick where a thread can start: one task on an executor's dedicated thread
//! running [`Shared::tick_once`](super::Shared::tick_once) until the set is shut.

use std::io;
use std::time::Duration;

use tutti_platform::{Executor, Task, is_driving, join_within};

use super::{Arc, Parker, Shared};

/// How long the drop waits for the task to run its last pass and end.
const JOIN_BOUND: Duration = Duration::from_secs(1);

/// The task, held for its drop, which waits for it to end.
pub(super) struct Tick {
    task: Option<Task<()>>,
}

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

    #[cfg(test)]
    pub(super) fn none() -> Self {
        Self { task: None }
    }

    #[cfg(test)]
    pub(super) fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Tick {
    /// Runs after `Bells`' drop has shut the set and unparked: the task runs the last
    /// pass and ends, and the drop waits for it.
    ///
    /// On a thread that drives an executor a wait would stop that thread's tasks (and
    /// `join_within` panics there), so the task is detached with a warning instead.
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        if is_driving() {
            tracing::warn!(
                "the bells dropped on a thread that drives an executor: their tick task is detached, and its last pass may not have run"
            );
            task.detach();
            return;
        }
        if let Err(task) = join_within(task, JOIN_BOUND) {
            tracing::warn!(
                "the bells' tick task did not end within {JOIN_BOUND:?} of the drop: \
                 its last pass may not have run"
            );
            task.detach();
        }
    }
}
