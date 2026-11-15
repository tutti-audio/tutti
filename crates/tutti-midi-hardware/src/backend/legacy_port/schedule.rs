//! Items held until their time and released in order.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// What waits for a time: items released earliest first, those due at the same
/// instant in the order they were pushed.
#[derive(Debug)]
pub(in crate::backend) struct Schedule<T> {
    /// Sorted by due time; ties keep push order (a later batch goes after every
    /// item due at or before it).
    held: VecDeque<(Instant, T)>,
}

impl<T> Schedule<T> {
    /// A schedule holding nothing.
    pub(in crate::backend) fn new() -> Self {
        Self {
            held: VecDeque::new(),
        }
    }

    /// Drops every held item; returns how many.
    pub(in crate::backend) fn clear(&mut self) -> usize {
        let held = self.held.len();
        self.held.clear();
        held
    }

    /// Holds a batch, each item until its time but never before the batch's
    /// item ahead of it: a batch is in order, so an event stamped earlier than
    /// the one before it goes right after it, not ahead. At most `room` items
    /// are taken; returns how many (a prefix).
    ///
    /// The clamped batch is in time order, so it is appended to what is held,
    /// with no allocation beyond the deque's growth: the common case, a batch
    /// that is no earlier than the last held item. A batch that starts before
    /// it is put in place by a stable sort of the two runs, which keeps a held
    /// item ahead of a batch item due at the same time; that costs the held
    /// count plus the batch's, and only for such a batch.
    pub(in crate::backend) fn push_batch(
        &mut self,
        room: usize,
        batch: impl IntoIterator<Item = (Instant, T)>,
    ) -> usize {
        let before = self.held.len();
        let mut floor: Option<Instant> = None;
        self.held
            .extend(batch.into_iter().take(room).map(|(due, item)| {
                let due = floor.map_or(due, |floor| due.max(floor));
                floor = Some(due);
                (due, item)
            }));
        let taken = self.held.len() - before;
        let out_of_order = before > 0 && taken > 0 && self.held[before - 1].0 > self.held[before].0;
        if out_of_order {
            self.held.make_contiguous().sort_by_key(|(due, _)| *due);
        }
        taken
    }

    /// The earliest held item that is due at `now`, removed; `None` when
    /// every item is still to wait.
    pub(in crate::backend) fn pop_due(&mut self, now: Instant) -> Option<T> {
        if self.held.front()?.0 <= now {
            self.held.pop_front().map(|(_, item)| item)
        } else {
            None
        }
    }

    /// How long from `now` until the earliest item is due; `None` when nothing
    /// is held, zero when one is due already.
    pub(in crate::backend) fn wait(&self, now: Instant) -> Option<Duration> {
        let (due, _) = self.held.front()?;
        Some(due.saturating_duration_since(now))
    }
}

/// What an output with a sender thread shares with it, behind one lock: the
/// held events, whether the sender is alive, and how many events were taken and
/// not yet sent. Every change of the three is one method, so no count is
/// touched outside the lock that decides it.
#[derive(Debug)]
pub(in crate::backend) struct Outbox<T> {
    queue: Schedule<T>,
    stop: bool,
    alive: bool,
    /// Taken and not yet sent: held, plus the one the sender has in hand.
    pending: usize,
}

impl<T> Outbox<T> {
    /// An outbox with a live sender and nothing held.
    pub(in crate::backend) fn new() -> Self {
        Self {
            queue: Schedule::new(),
            stop: false,
            alive: true,
            pending: 0,
        }
    }

    /// How many more events a sender bounded at `limit` pending takes now:
    /// none once it is dead.
    pub(in crate::backend) fn room(&self, limit: usize) -> usize {
        if self.alive {
            limit.saturating_sub(self.pending)
        } else {
            0
        }
    }

    /// Takes a batch as [`Schedule::push_batch`] does, at most
    /// [`room`](Self::room)`(limit)` of it. A dead sender takes nothing.
    pub(in crate::backend) fn take(
        &mut self,
        limit: usize,
        batch: impl IntoIterator<Item = (Instant, T)>,
    ) -> usize {
        let taken = self.queue.push_batch(self.room(limit), batch);
        self.pending += taken;
        taken
    }

    /// The sender's next step: `None` to stop, or the earliest event due at
    /// `now` (it stays pending until [`done`](Self::done)), or how long to
    /// wait.
    pub(in crate::backend) fn next(&mut self, now: Instant) -> Next<T> {
        if self.stop {
            return Next::Stop;
        }
        match self.queue.pop_due(now) {
            Some(event) => Next::Send(event),
            None => Next::Wait(self.queue.wait(now)),
        }
    }

    /// The event [`next`](Self::next) gave has been handled.
    pub(in crate::backend) fn done(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }

    /// Asks the sender to stop.
    pub(in crate::backend) fn stop(&mut self) {
        self.stop = true;
    }

    /// The sender is gone: it takes nothing more, and what it held and the
    /// event in its hand (if it panicked sending) are dropped. Returns how many
    /// events that was: the pending count, before it is zeroed.
    pub(in crate::backend) fn end(&mut self) -> usize {
        self.alive = false;
        self.queue.clear();
        std::mem::take(&mut self.pending)
    }

    /// Whether the sender can still send.
    pub(in crate::backend) fn alive(&self) -> bool {
        self.alive
    }

    /// How many events were taken and not yet sent.
    pub(in crate::backend) fn pending(&self) -> usize {
        self.pending
    }
}

/// What a sender does next ([`Outbox::next`]).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::backend) enum Next<T> {
    /// The output was dropped.
    Stop,
    /// An event is due.
    Send(T),
    /// Nothing is due: wait this long (for ever for `None`) or for a wake.
    Wait(Option<Duration>),
}
