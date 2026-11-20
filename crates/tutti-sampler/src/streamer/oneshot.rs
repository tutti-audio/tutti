//! A one-shot channel, used for the reply of an open: the streamer sends once and the open's owner
//! reads once, by [`try_recv`](Receiver::try_recv) or by awaiting.
//!
//! A send wakes the receiver's [`Waker`]; the streamer is a control-side future,
//! never the audio thread, so the wake may do what a `Waker` does.

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
pub(crate) struct Sender<T> {
    shared: Shared<T>,
}

/// The receiving end: reads the answer once.
pub(crate) struct Receiver<T> {
    shared: Shared<T>,
}

/// Nothing to read: the answer has not come, or never will.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// Not sent yet.
    Empty,
    /// The sender dropped without sending.
    Disconnected,
}

/// A sender and its receiver.
pub(crate) fn oneshot<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Mutex::new(State {
        value: None,
        waker: None,
        sender_gone: false,
        receiver_gone: false,
    }));
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

impl<T> Sender<T> {
    /// Answers, and wakes the receiver's task.
    ///
    /// # Errors
    ///
    /// `value` back when the receiver is gone.
    pub(crate) fn send(self, value: T) -> Result<(), T> {
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

impl<T> Drop for Sender<T> {
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

impl<T> Receiver<T> {
    /// The answer if it came.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] when it has not, or never will.
    pub(crate) fn try_recv(&self) -> Result<T, Unanswered> {
        let mut state = lock(&self.shared);
        match state.value.take() {
            Some(value) => Ok(value),
            None if state.sender_gone => Err(Unanswered::Disconnected),
            None => Err(Unanswered::Empty),
        }
    }

    /// [`try_recv`](Self::try_recv) for a future: pending stores the task's waker.
    pub(crate) fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, Unanswered>> {
        let mut state = lock(&self.shared);
        match state.value.take() {
            Some(value) => Poll::Ready(Ok(value)),
            None if state.sender_gone => Poll::Ready(Err(Unanswered::Disconnected)),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        lock(&self.shared).receiver_gone = true;
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
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

    /// A send wakes the task that polled the receiver pending, once, and the poll
    /// after it reads the value.
    #[test]
    fn a_send_wakes_the_pending_task_and_the_next_poll_reads_it() {
        let (tx, rx) = oneshot::<u8>();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        tx.send(7).unwrap();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(7)));
    }

    /// A sender dropped unanswered disconnects the receiver, and wakes its task.
    #[test]
    fn a_dropped_sender_disconnects_and_wakes() {
        let (tx, rx) = oneshot::<u8>();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        drop(tx);
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(rx.try_recv(), Err(Unanswered::Disconnected));
    }

    /// A send to a dropped receiver hands the value back.
    #[test]
    fn a_send_to_a_dropped_receiver_returns_the_value() {
        let (tx, rx) = oneshot::<u8>();
        drop(rx);
        assert_eq!(tx.send(3), Err(3));
    }
}
