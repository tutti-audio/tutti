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

/// A receiver dropped after a pending poll, then a send: the send hands the value
/// back, and nothing wakes.
#[test]
fn a_send_after_a_pending_poll_and_a_dropped_receiver_returns_the_value_and_wakes_nothing() {
    let (tx, rx) = oneshot::<u8>();
    let (count, waker) = counting();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
    drop(rx);
    assert_eq!(tx.send(9), Err(9));
    assert_eq!(count.0.load(Ordering::SeqCst), 0, "no one is left to wake");
}

/// The answer is read once: a poll after it was read is a `RecvError`.
#[test]
fn a_poll_after_the_answer_was_read_is_a_recv_error() {
    let (tx, rx) = oneshot::<u8>();
    tx.send(1).unwrap();
    let (_, waker) = counting();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(1)));
    assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Err(RecvError)));
}
