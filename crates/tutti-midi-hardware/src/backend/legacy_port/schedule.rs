//! Items held until their time and released in order.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// What waits for a time: items released earliest first, those due at the same
/// instant in the order they were pushed.
#[derive(Debug)]
pub(in crate::backend) struct Schedule<T> {
    /// Sorted by due time; ties keep push order (a later push goes after every
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

    /// How many items are held.
    pub(in crate::backend) fn len(&self) -> usize {
        self.held.len()
    }

    /// Drops every held item; returns how many.
    pub(in crate::backend) fn clear(&mut self) -> usize {
        let held = self.held.len();
        self.held.clear();
        held
    }

    /// Holds `item` until `due`.
    pub(in crate::backend) fn push(&mut self, due: Instant, item: T) {
        let at = self.held.partition_point(|(held, _)| *held <= due);
        self.held.insert(at, (due, item));
    }

    /// Holds a batch, each item until its time but never before the batch's
    /// item ahead of it: a batch is in order, so an event stamped earlier than
    /// the one before it goes right after it, not ahead. At most `room` items
    /// are taken; returns how many (a prefix).
    pub(in crate::backend) fn push_batch(
        &mut self,
        room: usize,
        batch: impl IntoIterator<Item = (Instant, T)>,
    ) -> usize {
        let mut floor: Option<Instant> = None;
        let mut taken = 0;
        for (due, item) in batch.into_iter().take(room) {
            let due = floor.map_or(due, |floor| due.max(floor));
            self.push(due, item);
            floor = Some(due);
            taken += 1;
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
