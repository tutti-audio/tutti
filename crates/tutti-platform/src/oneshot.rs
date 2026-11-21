//! A one-shot channel: one control-side thread answers once and the asker reads
//! once, by awaiting.
//!
//! A send wakes the receiver's [`Waker`]. The sender is never the audio thread, so
//! the wake may do what a `Waker` does.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

struct State<T> {
    value: Option<T>,
    waker: Option<Waker>,
    sender_gone: bool,
    receiver_gone: bool,
}

type Shared<T> = Arc<Mutex<State<T>>>;

fn lock<T>(shared: &Shared<T>) -> MutexGuard<'_, State<T>> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sending end: answers once.
pub struct OneshotSender<T> {
    shared: Shared<T>,
}

/// The receiving end: a future that resolves to the answer.
pub struct OneshotReceiver<T> {
    shared: Shared<T>,
}

/// The sender dropped without answering: nothing will ever come.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("the sender was dropped without answering")]
pub struct RecvError;

/// A sender and its receiver.
#[must_use]
pub fn oneshot<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let shared = Arc::new(Mutex::new(State {
        value: None,
        waker: None,
        sender_gone: false,
        receiver_gone: false,
    }));
    (
        OneshotSender {
            shared: Arc::clone(&shared),
        },
        OneshotReceiver { shared },
    )
}

impl<T> OneshotSender<T> {
    /// Answers: dropping the sender, which this does, wakes the receiver's task.
    ///
    /// # Errors
    ///
    /// `value` back when the receiver is gone.
    pub fn send(self, value: T) -> Result<(), T> {
        let mut state = lock(&self.shared);
        if state.receiver_gone {
            return Err(value);
        }
        state.value = Some(value);
        Ok(())
    }
}

impl<T> Drop for OneshotSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = lock(&self.shared);
            state.sender_gone = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> OneshotReceiver<T> {
    /// Reads the answer if it came; pending stores the task's waker.
    ///
    /// The answer is read once: a poll after it reads [`RecvError`], as the sender
    /// is gone.
    ///
    /// # Errors
    ///
    /// [`RecvError`] when the sender dropped without answering, or its answer was
    /// already read.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let mut state = lock(&self.shared);
        match state.value.take() {
            Some(value) => Poll::Ready(Ok(value)),
            None if state.sender_gone => Poll::Ready(Err(RecvError)),
            None => {
                if !state
                    .waker
                    .as_ref()
                    .is_some_and(|w| w.will_wake(cx.waker()))
                {
                    state.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_recv(cx)
    }
}

impl<T> std::fmt::Debug for OneshotSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneshotSender").finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for OneshotReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneshotReceiver").finish_non_exhaustive()
    }
}

impl<T> Drop for OneshotReceiver<T> {
    fn drop(&mut self) {
        // The task that stored its waker is no longer waiting: the waker goes with
        // the receiver, outside the lock, so a later send wakes no one.
        let waker = {
            let mut state = lock(&self.shared);
            state.receiver_gone = true;
            state.waker.take()
        };
        drop(waker);
    }
}

#[cfg(test)]
mod tests;
