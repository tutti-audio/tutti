//! The one future behind the session's file loads: work that reads a file or
//! folds audio runs as a task on the session's executor, and the future awaits
//! its answer.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::oneshot;
use tutti_platform::WeakExecutor;

use crate::loaded::caught_by;

/// An error type that can say why work ended without its output: the session's
/// executor was gone, or the work panicked. A family's error implements it
/// with its own variants.
pub(crate) trait Unanswered {
    /// The session's executor is gone or refused the task, or dropped it before
    /// it ran: the host is shutting down and the work did not fail.
    fn shut_down() -> Self;

    /// The work panicked; `message` is the panic's, when it was a string.
    fn panicked(message: String) -> Self;
}

/// `work` over `input`, run as a task on the session's executor and awaited.
///
/// The first poll upgrades the executor's weak handle for the spawn only, starts
/// a task that owns the sender of a oneshot and detaches it, and awaits the
/// receiver. **The task fills the sender**: it checks that the receiver is still
/// there (a future dropped before the task began has nobody to answer, and the
/// task does nothing), runs `work` with a panic caught as `E::panicked`, and
/// sends the output, which wakes the future. A poll never blocks.
///
/// Dropped before it resolves, the future drops the receiver: a task that has not
/// begun reads nothing, and one in flight finishes and its output is dropped.
/// Dropped before its first poll, it starts nothing.
///
/// The output is `E::shut_down()` when the executor is gone or refuses the task, or
/// the sender is dropped unanswered (the executor dropped the task before it ran).
///
/// # Panics
///
/// When polled after it resolved.
#[must_use = "work does nothing unless it is awaited"]
pub(crate) struct Unblocked<W, T, E> {
    executor: WeakExecutor,
    work: fn(W) -> Result<T, E>,
    stage: Stage<W, T, E>,
}

enum Stage<W, T, E> {
    Unstarted(W),
    Running(oneshot::Receiver<Result<T, E>>),
    Finished,
}

impl<W, T, E> Unblocked<W, T, E> {
    pub(crate) const fn new(executor: WeakExecutor, input: W, work: fn(W) -> Result<T, E>) -> Self {
        Self {
            executor,
            work,
            stage: Stage::Unstarted(input),
        }
    }
}

impl<W, T, E> Unblocked<W, T, E>
where
    W: Send + 'static,
    T: Send + 'static,
    E: Unanswered + Send + 'static,
{
    /// Starts the task over `input`: it owns the sender of the reply this
    /// returns the receiver of.
    fn start(&self, input: W) -> Result<oneshot::Receiver<Result<T, E>>, E> {
        // Upgraded for the spawn only: the task holds no strong handle.
        let Some(executor) = self.executor.upgrade() else {
            return Err(E::shut_down());
        };
        let work = self.work;
        let (reply, receiver) = oneshot::channel();
        let task = executor
            .unblock(move || {
                if reply.is_canceled() {
                    return;
                }
                let output = caught_by(|| work(input), E::panicked);
                // Refused when the future was dropped meanwhile: the output drops here.
                drop(reply.send(output));
            })
            .map_err(|_| E::shut_down())?;
        // The task runs on without the future: its end is the reply.
        task.detach();
        Ok(receiver)
    }
}

impl<W, T, E> Future for Unblocked<W, T, E>
where
    W: Send + 'static,
    T: Send + 'static,
    E: Unanswered + Send + 'static,
{
    type Output = Result<T, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if matches!(this.stage, Stage::Unstarted(_)) {
            let Stage::Unstarted(input) = std::mem::replace(&mut this.stage, Stage::Finished)
            else {
                unreachable!("matched above");
            };
            match this.start(input) {
                Ok(receiver) => this.stage = Stage::Running(receiver),
                Err(refused) => return Poll::Ready(Err(refused)),
            }
        }
        let Stage::Running(receiver) = &mut this.stage else {
            panic!("a load future is polled after it resolved");
        };
        let answered = std::task::ready!(Pin::new(receiver).poll(cx));
        this.stage = Stage::Finished;
        // A sender dropped unanswered: the executor dropped the task before it ran.
        Poll::Ready(answered.unwrap_or_else(|_| Err(E::shut_down())))
    }
}

// The input is never pinned: a poll moves it into the task.
impl<W, T, E> Unpin for Unblocked<W, T, E> {}

impl<W, T, E> fmt::Debug for Unblocked<W, T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Unblocked").finish_non_exhaustive()
    }
}
