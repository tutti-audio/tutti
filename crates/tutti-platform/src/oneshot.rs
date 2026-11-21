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
    /// Answers, and wakes the receiver's task.
    ///
    /// # Errors
    ///
    /// `value` back when the receiver is gone.
    pub fn send(self, value: T) -> Result<(), T> {
        let waker = {
            let mut state = lock(&self.shared);
            if state.receiver_gone {
                return Err(value);
            }
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
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
    /// # Errors
    ///
    /// [`RecvError`] when the sender dropped without answering.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let mut state = lock(&self.shared);
        match state.value.take() {
            Some(value) => Poll::Ready(Ok(value)),
            None if state.sender_gone => Poll::Ready(Err(RecvError)),
            None => {
                state.waker = Some(cx.waker().clone());
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
        lock(&self.shared).receiver_gone = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Count>, Waker) {
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        (count, waker)
    }

    /// A value sent before the receiver is polled is read by the first poll.
    #[test]
    fn a_value_sent_before_the_poll_is_read_by_it() {
        let (tx, rx) = oneshot::<u8>();
        tx.send(5).unwrap();
        let (_, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(5)));
    }

    /// A send wakes the task that polled the receiver pending, once, and the poll
    /// after it reads the value.
    #[test]
    fn a_send_wakes_the_pending_task_and_the_next_poll_reads_it() {
        let (tx, rx) = oneshot::<u8>();
        let (count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        tx.send(7).unwrap();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(7)));
    }

    /// A sender dropped unanswered ends the receiver with `RecvError`, and wakes its
    /// task.
    #[test]
    fn a_dropped_sender_ends_the_receiver_with_a_recv_error_and_wakes() {
        let (tx, rx) = oneshot::<u8>();
        let (count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        drop(tx);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Err(RecvError)));
    }

    /// A send to a dropped receiver hands the value back.
    #[test]
    fn a_send_to_a_dropped_receiver_returns_the_value() {
        let (tx, rx) = oneshot::<u8>();
        drop(rx);
        assert_eq!(tx.send(3), Err(3));
    }
}
