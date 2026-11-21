//! Runs a future to its output on the calling thread.

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

/// Wakes the thread that waits on the future.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Polls `future` (or what it turns into) on this thread until it resolves, parking between polls: the
/// future's waker is what ends the park.
pub fn block_on<F: IntoFuture>(future: F) -> <F::IntoFuture as Future>::Output {
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future.into_future());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A future another thread wakes resolves with its output.
    #[test]
    fn a_future_woken_by_another_thread_resolves() {
        let (send, receive) = std::sync::mpsc::channel::<Waker>();
        let mut woken = false;
        let future = std::future::poll_fn(move |cx| {
            if woken {
                return Poll::Ready(7);
            }
            woken = true;
            send.send(cx.waker().clone()).unwrap();
            Poll::Pending
        });
        let waker = thread::spawn(move || receive.recv().unwrap().wake());
        assert_eq!(block_on(future), 7);
        waker.join().unwrap();
    }
}
