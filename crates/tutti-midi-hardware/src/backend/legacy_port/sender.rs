//! An output's sender: a thread's loop over an [`Outbox`], with the sending and
//! the waiting left to the platform.

use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::schedule::{Next, Outbox};
use tutti_graph::Losses;

/// What an output and its sender thread share: the [`Outbox`] behind its lock
/// and the counts of what the sender could not send.
#[derive(Debug)]
pub(in crate::backend) struct Mailbox<T> {
    state: Mutex<Outbox<T>>,
    /// Sends the device refused.
    lost: AtomicU64,
    /// Events dropped when the sender ended.
    refused: AtomicU64,
}

impl<T> Mailbox<T> {
    /// A mailbox with a live sender and nothing held.
    pub(in crate::backend) fn new() -> Self {
        Self {
            state: Mutex::new(Outbox::new()),
            lost: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }
    }

    /// The outbox, locked. The lock is the sender's too, held for one step.
    pub(in crate::backend) fn state(&self) -> MutexGuard<'_, Outbox<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the sender could not send: refused sends are lost, and what was
    /// held when it ended had nowhere to go (refused).
    pub(in crate::backend) fn losses(&self) -> Losses {
        Losses::lost(self.lost.load(Ordering::Acquire))
            + Losses::refused(self.refused.load(Ordering::Acquire))
    }
}

/// Marks the mailbox's sender dead when its loop ends, however it ends, a panic
/// included: what it held had nowhere to go.
struct Ends<'a, T>(&'a Mailbox<T>);

impl<T> Drop for Ends<'_, T> {
    fn drop(&mut self) {
        let dropped = self.0.state().end();
        // A count of events held in memory fits `u64` on every platform.
        let dropped = u64::try_from(dropped).unwrap_or(u64::MAX);
        self.0.refused.fetch_add(dropped, Ordering::AcqRel);
    }
}

/// Sends what is due, in order, with the `send` that `setup` makes, and waits
/// with its `wait` (for the time it is given, `None` for ever) until the next.
/// Returns on a stop or when `wait` fails; the mailbox is dead then, and when
/// the loop unwinds. `setup` runs after the guard that marks the mailbox dead
/// exists, so a setup that panics ends the sender the same way.
pub(in crate::backend) fn run<T, S, W, Send, Wait>(
    mailbox: &Mailbox<T>,
    setup: impl FnOnce() -> (Send, Wait),
) where
    S: Display,
    W: Display,
    Send: FnMut(T) -> Result<(), S>,
    Wait: FnMut(Option<Duration>) -> Result<(), W>,
{
    let _ends = Ends(mailbox);
    let (mut send, mut wait) = setup();
    loop {
        // The stop is looked at with each event, not once per batch: a long run
        // of due events must not keep a dropped output sending.
        let step = mailbox.state().next(Instant::now());
        match step {
            Next::Stop => return,
            Next::Send(event) => {
                if let Err(e) = send(event) {
                    tracing::debug!("MIDI send: {e}");
                    mailbox.lost.fetch_add(1, Ordering::AcqRel);
                }
                mailbox.state().done();
            }
            Next::Wait(time) => {
                if let Err(e) = wait(time) {
                    tracing::warn!("MIDI output stopped: {e}");
                    return;
                }
            }
        }
    }
}
