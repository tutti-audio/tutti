//! A native target: one thread of tutti's drives the session's executor.

use std::io;
use std::num::NonZeroUsize;

use tutti_platform::{Executor, Threads};

/// A thread can start.
pub(crate) const THREADS: bool = true;

/// The thread that drives the executor, stopped and joined on drop.
#[must_use = "dropping it stops the thread"]
pub(crate) struct Running {
    _thread: Threads,
}

/// Starts the one thread that fires `executor`'s sleeps and runs its tasks.
///
/// # Errors
///
/// The OS's refusal to start the thread.
pub(crate) fn start(executor: &Executor) -> io::Result<Running> {
    Ok(Running {
        _thread: executor.spawn_threads(NonZeroUsize::MIN)?,
    })
}

/// Nothing to run here: the thread `start` made runs the executor's tasks.
pub(crate) fn drive(_executor: &Executor) {}
