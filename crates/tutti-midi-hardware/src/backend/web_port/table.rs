//! The page thread's table of browser objects, and the ids another thread gave back.
//!
//! A browser object (`MIDIAccess`, a port, a `Closure`) is not `Send`, so it never sits in
//! a type that crosses threads: it sits in a [`Table`] on the thread of the page, and what
//! the rest of the crate holds is the `u32` id [`mint`] gave it. A call from a thread
//! whose table has no such id finds nothing ([`Table::get`] is `None`). An id released
//! from such a thread is queued in an [`Orphans`] for the page thread to [`sweep`](Table::sweep).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

use tutti_rt::Mutex;

/// A new id, distinct from every other in the process, so an id from one thread's table
/// never names an entry of another's.
pub(in crate::backend) fn mint() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The ids released off the thread of their table, waiting for it to sweep them.
#[derive(Debug, Default)]
pub(in crate::backend) struct Orphans {
    queue: Mutex<Vec<u32>>,
}

impl Orphans {
    pub(in crate::backend) const fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
        }
    }

    /// Queues `id` for the thread whose table holds it.
    pub(in crate::backend) fn push(&self, id: u32) {
        self.queue.lock().push(id);
    }

    /// How many ids wait.
    #[cfg(test)]
    pub(in crate::backend) fn len(&self) -> usize {
        self.queue.lock().len()
    }
}

/// The values one thread holds, by id.
#[derive(Debug)]
pub(in crate::backend) struct Table<T> {
    entries: BTreeMap<u32, T>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl<T> Table<T> {
    /// Holds `value` under a new id.
    pub(in crate::backend) fn insert(&mut self, value: T) -> u32 {
        let id = mint();
        self.entries.insert(id, value);
        id
    }

    /// The value under `id`; `None` where this table does not hold it (another thread's
    /// id, or one already released).
    pub(in crate::backend) fn get(&self, id: u32) -> Option<&T> {
        self.entries.get(&id)
    }

    /// Drops every value whose id is in `orphans`, and leaves in it the ids this table does
    /// not hold (another thread's). Returns how many it dropped.
    pub(in crate::backend) fn sweep(&mut self, orphans: &Orphans) -> usize {
        let mut queue = orphans.queue.lock();
        let before = queue.len();
        queue.retain(|id| self.entries.remove(id).is_none());
        before - queue.len()
    }

    /// How many values the table holds.
    #[cfg(test)]
    pub(in crate::backend) fn len(&self) -> usize {
        self.entries.len()
    }
}
