//! Where a peer's answer goes: a [`Job`](crate::Job) that lands in an update, or a
//! [`Request`] future that tutti's MIDI thread resolves.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tutti_rt::{OneshotReceiver, OneshotSender, Unanswered, oneshot};

use super::PeFailure;
use crate::job::Ticket;
use crate::{JobKind, PeerError};

/// The end of a request the session answers: it lands the request's result once.
pub(crate) enum Reply<J: JobKind> {
    /// A job's ticket: the result lands in a later update.
    Job(Ticket<J>),
    /// A future's sender: the result resolves it, and goes nowhere when the
    /// future was dropped.
    Future(OneshotSender<Result<J::Output, J::Error>>),
}

impl<J: JobKind> Reply<J> {
    pub(crate) fn land(self, result: Result<J::Output, J::Error>) {
        match self {
            Self::Job(ticket) => ticket.land(result),
            // A dropped future has no one to read what it asked for.
            Self::Future(sender) => drop(sender.send(result)),
        }
    }
}

/// A request to a peer, as a future: it resolves with what the peer answered, on
/// tutti's MIDI thread, or with the [`PeerError`] that says why it could not.
///
/// Dropping it is harmless: the answer, when it comes, is discarded. A request
/// whose session or device goes away unanswered resolves to
/// [`PeerError::Disconnected`]; it never waits for ever.
#[must_use = "a request is answered only when it is awaited"]
pub(crate) struct Request<T> {
    reply: OneshotReceiver<Result<T, PeerError<PeFailure>>>,
}

impl<T> Request<T> {
    /// A request and the sender that answers it.
    pub(crate) fn new() -> (OneshotSender<Result<T, PeerError<PeFailure>>>, Self) {
        let (sender, reply) = oneshot();
        (sender, Self { reply })
    }

    /// A request that ended at the call, as `error`.
    pub(crate) fn failed(error: PeerError<PeFailure>) -> Self {
        let (sender, request) = Self::new();
        drop(sender.send(Err(error)));
        request
    }
}

impl<T> Future for Request<T> {
    type Output = Result<T, PeerError<PeFailure>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.reply.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(answer)) => Poll::Ready(answer),
            // The sender went without answering: the session ended it.
            Poll::Ready(Err(Unanswered::Disconnected | Unanswered::Empty)) => {
                Poll::Ready(Err(PeerError::Disconnected))
            }
        }
    }
}

impl<T> std::fmt::Debug for Request<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request").finish_non_exhaustive()
    }
}
