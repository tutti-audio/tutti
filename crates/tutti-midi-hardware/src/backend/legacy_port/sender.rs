//! An output's sender: a thread's loop over an [`Outbox`], with the sending and
//! the waiting left to the platform.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::LockExt;

use super::schedule::{Next, Outbox};
use tutti_rt::{Counter, CounterWriter, counter};
use tutti_units::Losses;

/// What an output and its sender thread share: the [`Outbox`] behind its lock
/// and the counts of what the sender could not send.
#[derive(Debug)]
pub(in crate::backend) struct Mailbox<T> {
    state: Mutex<Outbox<T>>,
    /// What the sender could not send: sends the device refused are `lost`,
    /// events dropped when the sender ended are `refused`. The sender's end
    /// guard holds the one writer.
    losses: Counter<Losses>,
}

impl<T> Mailbox<T> {
    /// A mailbox with a live sender and nothing held, and the writer of its
    /// losses, which [`run`] takes.
    pub(in crate::backend) fn new() -> (Self, CounterWriter<Losses>) {
        let (writer, losses) = counter();
        let mailbox = Self {
            state: Mutex::new(Outbox::new()),
            losses,
        };
        (mailbox, writer)
    }

    /// The outbox, locked. The lock is the sender's too, held for one step.
    pub(in crate::backend) fn state(&self) -> MutexGuard<'_, Outbox<T>> {
        self.state.lock_recover()
    }

    /// What the sender could not send: refused sends are lost, and what was
    /// held when it ended had nowhere to go (refused).
    pub(in crate::backend) fn losses(&self) -> Losses {
        self.losses.get()
    }
}

/// Marks the mailbox's sender dead when its loop ends, however it ends, a panic
/// included: what it held had nowhere to go.
struct Ends<'a, T>(&'a Mailbox<T>, CounterWriter<Losses>);

impl<T> Drop for Ends<'_, T> {
    fn drop(&mut self) {
        let dropped = self.0.state().end();
        // A count of events held in memory fits `u64` on every platform.
        let dropped = u64::try_from(dropped).unwrap_or(u64::MAX);
        self.1.add(Losses::refused(dropped));
    }
}

/// Sends what is due, in order, with the `send` that `setup` makes, and waits
/// with its `wait` (for the time it is given, `None` for ever) until the next.
/// Returns on a stop or when `wait` fails; the mailbox is dead then, and when
/// the loop unwinds. `setup` runs after the guard that marks the mailbox dead
/// exists, so a setup that panics ends the sender the same way.
pub(in crate::backend) fn run<T, S, W, Send, Wait>(
    mailbox: &Mailbox<T>,
    losses: CounterWriter<Losses>,
    setup: impl FnOnce() -> (Send, Wait),
) where
    Send: FnMut(T) -> Result<(), S>,
    Wait: FnMut(Option<Duration>) -> Result<(), W>,
{
    let mut ends = Ends(mailbox, losses);
    let (mut send, mut wait) = setup();
    loop {
        // The stop is looked at with each event, not once per batch: a long run
        // of due events must not keep a dropped output sending.
        let step = mailbox.state().next(Instant::now());
        match step {
            Next::Stop => return,
            Next::Send(event) => {
                // A refused send is counted: the sender's thread is a driver's,
                // and logs nothing.
                if send(event).is_err() {
                    ends.1.add(Losses::lost(1));
                }
                mailbox.state().done();
            }
            Next::Wait(time) => {
                // A wait that fails ends the sender; the mailbox reads dead and
                // what it held is counted refused.
                if wait(time).is_err() {
                    return;
                }
            }
        }
    }
}
