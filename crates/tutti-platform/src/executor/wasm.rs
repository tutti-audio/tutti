//! The web's executor edge: a page starts no threads, so nothing but the caller's drive
//! runs the queue, a blocking closure is a task of that queue, and a dedicated future is
//! queued like any other.

use std::future::Future;
use std::io;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{Executor, Handle, Shared, Task};
use crate::priority::PriorityRequest;

/// No thread drives on the web.
pub(super) fn is_driving() -> bool {
    false
}

/// The web cannot block: one look at `task`, its output when it has ended.
pub(super) fn join<T>(task: Task<T>) -> Option<T> {
    let mut ended = pin!(task.fallible());
    match ended.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => None,
    }
}

/// The web cannot block: one look at `task`, handed back when it has not ended.
pub(super) fn join_within<T>(task: Task<T>, _bound: Duration) -> Result<Option<T>, Task<T>> {
    if !task.is_finished() {
        return Err(task);
    }
    Ok(join(task))
}

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
) -> io::Result<Task<T>> {
    Ok(handle.shared.queue.spawn(async move { f() }))
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

/// A future that is not `Send` needs a thread of its own, and a page starts none.
pub(super) fn spawn_dedicated_local<T, F>(
    _executor: &Executor,
    _name: &str,
    _priority: Option<PriorityRequest>,
    _make: impl FnOnce() -> F + Send + 'static,
) -> io::Result<Task<T>>
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a web page starts no threads for a future that is not Send",
    ))
}

/// No value: a web page starts no threads, so nothing builds a [`super::Threads`].
#[derive(Debug)]
pub(super) enum Threads {}
