//! Where a peer's answer goes: a [`Job`](crate::Job) that lands in an update, or
//! a [`PeerReply`] future that the update resolves.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tutti_rt::{OneshotReceiver, OneshotSender, SenderGone, oneshot};

use super::PeFailure;
use crate::job::Ticket;
use crate::{JobKind, PeerError};

/// The end of a request the session answers: it lands the request's result once.
pub(crate) enum Landing<J: JobKind> {
    /// A job's ticket: the result lands in a later update.
    Job(Ticket<J>),
    /// A future's sender: the result resolves it, and goes nowhere when the
    /// future was dropped.
    Future(OneshotSender<Result<J::Output, J::Error>>),
}

impl<J: JobKind> Landing<J> {
    /// Lands `result` on the job or the future that asked.
    pub(crate) fn land(self, result: Result<J::Output, J::Error>) {
        match self {
            Self::Job(ticket) => ticket.land(result),
            // A dropped future has no one to read what it asked for.
            Self::Future(sender) => drop(sender.send(result)),
        }
    }
}

/// A peer's reply to a request, as a future: it resolves with what the peer
/// answered, or with the [`PeerError`] that says why it could not.
///
/// The session sends the request and reads the reply in its
/// [`update`](crate::Session::update), so the future resolves in a later update:
/// keep updating while you await it. Dropping it is harmless: the reply, when it
/// comes, is discarded. A request whose session or device goes away unanswered
/// resolves to [`PeerError::Disconnected`]; it never waits for ever.
#[must_use = "a reply is read only when the future is awaited"]
pub(crate) struct PeerReply<T> {
    reply: OneshotReceiver<Result<T, PeerError<PeFailure>>>,
}

impl<T> PeerReply<T> {
    /// A reply to await and the sender that answers it.
    pub(crate) fn new() -> (OneshotSender<Result<T, PeerError<PeFailure>>>, Self) {
        let (sender, reply) = oneshot();
        (sender, Self { reply })
    }

    /// A reply that ended at the call, as `error`.
    pub(crate) fn failed(error: PeerError<PeFailure>) -> Self {
        let (sender, reply) = Self::new();
        drop(sender.send(Err(error)));
        reply
    }
}

impl<T> Future for PeerReply<T> {
    type Output = Result<T, PeerError<PeFailure>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.reply.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(answer)) => Poll::Ready(answer),
            // The sender went without answering: the session ended it.
            Poll::Ready(Err(SenderGone)) => Poll::Ready(Err(PeerError::Disconnected)),
        }
    }
}

impl<T> std::fmt::Debug for PeerReply<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerReply").finish_non_exhaustive()
    }
}
