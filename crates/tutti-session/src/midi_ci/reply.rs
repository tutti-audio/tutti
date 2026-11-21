//! Where a peer's answer goes: a [`PeerReply`] future that the session's update
//! resolves.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::oneshot::{Canceled, Receiver, Sender, channel};

use super::PeFailure;
use crate::PeerError;

/// The end of a request the session answers: the sender of the future that
/// asked. A request that ends without an answer drops it, and the future
/// resolves to [`PeerError::Disconnected`].
pub(crate) type ReplySender<T, E = PeFailure> = Sender<Result<T, PeerError<E>>>;

/// Resolves the future that asked with `result`: it goes nowhere when the future was
/// dropped, since no one reads what it asked for.
pub(crate) fn resolve<T, E>(reply: ReplySender<T, E>, result: Result<T, PeerError<E>>) {
    drop(reply.send(result));
}

/// Declares the public future of one kind of request: a [`PeerReply`] that
/// resolves with the request's answer.
macro_rules! peer_future {
    (
        $(#[$meta:meta])*
        $name:ident => $output:ty, $error:ty, $must:literal
    ) => {
        /// Fed by [`Session::update`](crate::Session::update): keep updating while
        /// you await it.
        ///
        $(#[$meta])*
        ///
        /// Dropping it drops the answer; a device or session that goes away
        /// resolves it to [`PeerError::Disconnected`](crate::PeerError::Disconnected).
        #[derive(Debug)]
        #[must_use = $must]
        pub struct $name {
            reply: $crate::midi_ci::reply::PeerReply<$output, $error>,
        }

        impl $name {
            pub(crate) const fn new(
                reply: $crate::midi_ci::reply::PeerReply<$output, $error>,
            ) -> Self {
                Self { reply }
            }
        }

        impl ::std::future::Future for $name {
            type Output = Result<$output, $crate::PeerError<$error>>;

            fn poll(
                mut self: ::std::pin::Pin<&mut Self>,
                cx: &mut ::std::task::Context<'_>,
            ) -> ::std::task::Poll<Self::Output> {
                ::std::future::Future::poll(::std::pin::Pin::new(&mut self.reply), cx)
            }
        }
    };
}
pub(crate) use peer_future;

/// Fed by [`Session::update`](crate::Session::update): keep updating while you
/// await it.
///
/// A peer's reply to a request, as a future: it resolves with what the peer
/// answered, or with the [`PeerError`] that says why it could not. The session
/// sends the request and reads the reply in its update. Dropping it is harmless: the reply, when it
/// comes, is discarded. A request whose session or device goes away unanswered
/// resolves to [`PeerError::Disconnected`]; it never waits for ever.
#[must_use = "a reply is read only when the future is awaited"]
pub(crate) struct PeerReply<T, E = PeFailure> {
    reply: Receiver<Result<T, PeerError<E>>>,
}

impl<T, E> PeerReply<T, E> {
    /// A reply to await and the sender that answers it.
    pub(crate) fn new() -> (ReplySender<T, E>, Self) {
        let (sender, reply) = channel();
        (sender, Self { reply })
    }

    /// A reply that ended at the call, as `error`.
    pub(crate) fn failed(error: PeerError<E>) -> Self {
        let (sender, reply) = Self::new();
        drop(sender.send(Err(error)));
        reply
    }
}

impl<T, E> Future for PeerReply<T, E> {
    type Output = Result<T, PeerError<E>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.reply).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(answer)) => Poll::Ready(answer),
            // The sender went without answering: the session ended it.
            Poll::Ready(Err(Canceled)) => Poll::Ready(Err(PeerError::Disconnected)),
        }
    }
}

impl<T, E> std::fmt::Debug for PeerReply<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerReply").finish_non_exhaustive()
    }
}
