//! One-shot reply pair for command/response choreography.
//!
//! Each `ControlCommand` that expects a reply carries a [`Reply<T>`] of the right
//! type. The dispatch handler sends *into the reply*, and the calling
//! thread blocks on the paired [`Ask<T>`]. Per-request channels mean two
//! in-flight `SaveState` calls can't accidentally swap their results — the
//! one-shot semantics are enforced by the type system, not by hoping
//! responses come back in the order requests went out.
//!
//! A reply is answered into a blocking [`Ask`] or into an [`Answer`] a caller awaits: the lane's thread that sends the
//! reply wakes the awaiting task, and a reply dropped unanswered resolves it as
//! canceled.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;
use tutti_plugin_types::{Answer, AnswerSender};
use tutti_rt::{Condvar, Mutex};

/// Whether a control request is owed a reply: at most one is, and a caller who
/// finds it held waits for it, up to a bound of its own, instead of queueing a
/// second request behind it on the lane.
#[derive(Clone, Default)]
pub(super) struct ControlSlot(Arc<SlotState>);

#[derive(Default)]
struct SlotState {
    held: Mutex<Held>,
    freed: Condvar,
}

#[derive(Default)]
struct Held {
    owed: bool,
}

/// The slot, held: released when it drops, which is when its request's
/// [`Reply`] is answered or dropped unanswered.
struct Owing(Arc<SlotState>);

impl Drop for Owing {
    fn drop(&mut self) {
        self.0.held.lock().owed = false;
        self.0.freed.notify_one();
    }
}

/// Sender half. Consumed on [`Self::send`] — exactly one message can be
/// delivered, and forgetting to send is a compile error in any branch
/// that should have consumed `self`.
pub(super) struct Reply<T> {
    tx: Sink<T>,
    /// The slot this reply holds, for a request that owes one.
    owing: Option<Owing>,
}

/// Where a reply's value goes: the blocking caller's channel, or the oneshot a
/// future awaits.
enum Sink<T> {
    Blocking(SyncSender<T>),
    Awaited(AnswerSender<T>),
}

impl<T> Reply<T> {
    /// Answers the request. The slot is released **before** the value is
    /// sent: the caller the value wakes may ask again at once, and must find
    /// the slot free.
    pub(super) fn send(self, value: T) {
        let Self { tx, owing } = self;
        drop(owing);
        // A caller that stopped waiting (a dropped future) has no one to tell.
        match tx {
            Sink::Blocking(tx) => drop(tx.try_send(value)),
            Sink::Awaited(tx) => drop(tx.send(value)),
        }
    }
}

/// Receiver half. Consumed on `recv` / `recv_timeout` — exactly one
/// message can be observed.
pub(super) struct Ask<T>(Receiver<T>);

impl<T> Ask<T> {
    pub(super) fn recv_timeout(self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.0.recv_timeout(timeout)
    }

    /// Poll for the reply without consuming the `Ask`.
    ///
    /// The one-shot guarantee survives: the channel holds one message, so the
    /// first `Ok` is the only one any number of calls can produce. What this
    /// gives up is the *compile-time* proof that a caller looked exactly once,
    /// and it is needed by a caller that must wake periodically for a second
    /// signal — a state transfer watches a progress counter alongside the reply,
    /// and cannot express that as a single blocking read.
    pub(super) fn recv_timeout_borrowed(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.0.recv_timeout(timeout)
    }
}

/// Make a paired one-shot channel.
pub(super) fn ask<T>() -> (Ask<T>, Reply<T>) {
    let (tx, rx) = sync_channel(1);
    (
        Ask(rx),
        Reply {
            tx: Sink::Blocking(tx),
            owing: None,
        },
    )
}

/// Make a paired reply and the [`Answer`] a future awaits: the thread that
/// sends the reply fills it. It holds no slot ([`ControlSlot`]): the request
/// queues behind an owed one and the lane's deadline bounds each in turn, with
/// no caller waiting for either.
pub(super) fn awaited<T>() -> (Answer<T>, Reply<T>) {
    let (tx, answer) = Answer::channel();
    (
        answer,
        Reply {
            tx: Sink::Awaited(tx),
            owing: None,
        },
    )
}

impl ControlSlot {
    /// A paired one-shot channel that holds the slot until it is answered or
    /// dropped. Waits up to `wait` for another request to release it (parked on
    /// a condvar the release notifies, so no spinning); `None` when it is still
    /// held then.
    pub(super) fn ask<T>(&self, wait: Duration) -> Option<(Ask<T>, Reply<T>)> {
        let state = &self.0;
        let mut held = state.held.lock();
        if held.owed {
            held = state.freed.wait_timeout_while(held, wait, |h| h.owed).0;
            if held.owed {
                return None;
            }
        }
        held.owed = true;
        drop(held);
        let (waiting, mut reply) = ask();
        reply.owing = Some(Owing(Arc::clone(&self.0)));
        Some((waiting, reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **One request owes a reply at a time**: a second `ask` is refused while
    /// the first is unanswered, and the slot is free again at the answer (before
    /// its value is readable) and when the reply drops unanswered.
    #[test]
    fn the_slot_is_held_until_the_reply_is_answered_or_dropped() {
        let slot = ControlSlot::default();
        let now = Duration::ZERO;
        let (first, reply) = slot.ask::<u8>(now).expect("a free slot");
        assert!(slot.ask::<u8>(now).is_none(), "the slot is held");
        reply.send(1);
        let free = slot.ask::<u8>(now);
        assert!(
            free.is_some(),
            "answered: free by the time the value is sent"
        );
        assert_eq!(first.recv_timeout(Duration::from_secs(60)), Ok(1));
        drop(free);
        assert!(
            slot.ask::<u8>(now).is_some(),
            "dropped unanswered: free again"
        );
    }

    /// **A caller that finds the slot held takes it at the release**, in place
    /// of failing. Its budget is a hang guard only.
    #[test]
    fn a_caller_takes_the_slot_at_the_release() {
        let slot = ControlSlot::default();
        let (_first, reply) = slot.ask::<u8>(Duration::ZERO).expect("a free slot");
        let waiter = {
            let slot = slot.clone();
            std::thread::spawn(move || slot.ask::<u8>(Duration::from_secs(60)).is_some())
        };
        drop(reply);
        assert!(waiter.join().expect("the waiter"), "taken at the release");
    }

    #[test]
    fn test_ask_reply_oneshot() {
        let (ask, reply) = ask::<u32>();
        reply.send(42);
        assert_eq!(ask.recv_timeout(Duration::from_millis(10)).unwrap(), 42);
    }

    /// **An awaited reply fills its answer, and a dropped one cancels it.**
    #[test]
    fn an_awaited_reply_fills_the_answer_and_a_dropped_one_cancels_it() {
        let (mut answer, reply) = awaited::<u32>();
        let mut peek =
            || futures_lite::future::block_on(futures_lite::future::poll_once(&mut answer));
        assert_eq!(peek(), None, "nothing sent yet");
        reply.send(7);
        assert_eq!(peek(), Some(Some(7)));
        let (answer, reply) = awaited::<u32>();
        drop(reply);
        assert_eq!(
            futures_lite::future::block_on(answer),
            None,
            "dropped unanswered: canceled"
        );
    }

    #[test]
    fn test_ask_disconnect_returns_err() {
        let (ask, reply) = ask::<u32>();
        drop(reply);
        let err = ask.recv_timeout(Duration::from_millis(10)).unwrap_err();
        assert_eq!(err, RecvTimeoutError::Disconnected);
    }

    #[test]
    fn test_ask_no_reply_times_out() {
        let (ask, _reply) = ask::<u32>();
        let err = ask.recv_timeout(Duration::from_millis(5)).unwrap_err();
        assert_eq!(err, RecvTimeoutError::Timeout);
    }
}
