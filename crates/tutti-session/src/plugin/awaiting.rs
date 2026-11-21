//! The stage of an out-of-process state or preset request being awaited: the
//! request is queued with the plugin's bridge at the first poll, and the future
//! is the receiver of the [`Answer`] the bridge's control lane thread fills.

use std::pin::Pin;
use std::task::{Context, Poll};

use tutti_plugin_types::Answer;

/// Where an awaited request is: not queued yet (`P` is what queues it), queued
/// (the answer's receiver), or resolved.
pub(super) enum Stage<P, X, E> {
    Unstarted(P),
    Running(Answer<Result<X, E>>),
    Finished,
}

impl<P, X, E> Stage<P, X, E> {
    /// Queues the request on the first poll with `start`, which answers an error
    /// at once when it has no route, then polls the answer. **Nothing here
    /// waits**: the answer wakes the task when the control lane thread fills it,
    /// whether with the plugin's reply or with the deadline's error. A sender
    /// dropped unanswered (the bridge ended or the server died) is `lost`.
    ///
    /// # Panics
    ///
    /// When polled after it resolved.
    pub(super) fn poll(
        &mut self,
        cx: &mut Context<'_>,
        start: impl FnOnce(P) -> Result<Answer<Result<X, E>>, E>,
        lost: E,
    ) -> Poll<Result<X, E>> {
        if matches!(self, Self::Unstarted(_)) {
            let Self::Unstarted(request) = std::mem::replace(self, Self::Finished) else {
                unreachable!("matched above");
            };
            match start(request) {
                Ok(answer) => *self = Self::Running(answer),
                Err(refused) => return Poll::Ready(Err(refused)),
            }
        }
        let Self::Running(answer) = self else {
            panic!("a future is polled after it resolved");
        };
        let answered = std::task::ready!(Pin::new(answer).poll(cx));
        *self = Self::Finished;
        Poll::Ready(answered.unwrap_or(Err(lost)))
    }
}
