//! The web's executor edge: a page starts no threads, so nothing but the caller's drive
//! runs the queue, a blocking closure is a task of that queue, and a dedicated future is
//! queued like any other.

use std::future::Future;
use std::io;

use super::{Executor, Handle, Shared, Task};
use crate::priority::PriorityRequest;

/// No thread is ever parked on the web.
#[derive(Default)]
pub(super) struct Parked;

impl Parked {
    pub(super) fn unpark_all(&self) {}
}

/// The executor starts nothing of its own on the web.
pub(super) struct Own;

impl Own {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn shut_down(&self, _shared: &Shared) {}
}

/// A blocking closure runs inside the next drive, as a task of the queue.
pub(super) fn unblock<T: Send + 'static>(
    handle: &Handle,
    f: impl FnOnce() -> T + Send + 'static,
) -> Task<T> {
    handle.shared.queue.spawn(async move { f() })
}

/// A dedicated future is a queued one: there is no thread to give it, and no priority.
pub(super) fn spawn_dedicated<T: Send + 'static>(
    executor: &Executor,
    _name: &str,
    _priority: Option<PriorityRequest>,
    future: impl Future<Output = T> + Send + 'static,
) -> io::Result<Task<T>> {
    Ok(executor.spawn(future))
}
