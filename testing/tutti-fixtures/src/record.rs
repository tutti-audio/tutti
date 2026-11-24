//! The shared records a test node writes on the audio thread.

use std::sync::{LockResult, MutexGuard};
use tutti_rt::CappedVec;

use crate::Shared;

/// How many entries a [`Record`] holds unless built [`with_capacity`](Record::with_capacity):
/// more than any test's run writes.
const RECORD_CAP: usize = 1 << 16;

/// How many blocks a [`BlockLog`] holds, and how many entries each.
const BLOCKS: usize = 1 << 10;
const PER_BLOCK: usize = 1 << 8;

/// A shared, clonable log a test node pushes into from `process` and the test
/// reads afterwards.
///
/// The room is a [`CappedVec`] reserved when the record is built, on the control
/// side, behind a [`Shared`] mutex. A push past the room is refused and latches the overflow
/// flag, and [`lock`](Self::lock) panics on it, so a refused entry never hides
/// what a test reads. Clones share one log.
#[derive(Debug)]
pub struct Record<T>(Shared<CappedVec<T>>);

impl<T> Clone for Record<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for Record<T> {
    fn default() -> Self {
        Self::with_capacity(RECORD_CAP)
    }
}

impl<T> Record<T> {
    /// An empty record with room for the default run of entries.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty record with room for `cap` entries.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self(Shared::new(CappedVec::with_capacity(cap)))
    }

    /// The log, locked.
    ///
    /// # Errors
    ///
    /// When the lock is poisoned.
    ///
    /// # Panics
    ///
    /// When a push was refused because the log was full.
    pub fn lock(&self) -> LockResult<MutexGuard<'_, CappedVec<T>>> {
        let log = self.0.lock();
        if let Ok(log) = &log {
            assert!(!log.is_overflowed(), "the record overflowed its room");
        }
        log
    }
}

/// A log of per-block runs of `T`, for a test node that records what each
/// block held: the blocks and every block's run are [`CappedVec`]s reserved
/// when the log is built, so [`push_block`](Self::push_block) allocates nothing
/// on the audio thread. It reads as the blocks so far, and a read panics when
/// a block or a run overflowed.
#[derive(Debug)]
pub struct BlockLog<T> {
    done: CappedVec<CappedVec<T>>,
    spare: CappedVec<CappedVec<T>>,
}

impl<T> BlockLog<T> {
    /// An empty log, [`Shared`].
    #[must_use]
    pub fn shared() -> Shared<Self> {
        let mut spare = CappedVec::with_capacity(BLOCKS);
        for _ in 0..BLOCKS {
            spare.push(CappedVec::with_capacity(PER_BLOCK));
        }
        Shared::new(Self {
            done: CappedVec::with_capacity(BLOCKS),
            spare,
        })
    }

    /// Records one block holding `items`. A block past the room, or items past
    /// a block's, latch the overflow flag a read panics on.
    pub fn push_block(&mut self, items: impl IntoIterator<Item = T>) {
        let Some(mut block) = self.spare.pop() else {
            self.done.mark_overflowed();
            return;
        };
        block.extend(items);
        self.done.push(block);
    }

    /// Forgets every recorded block, keeping the room for more.
    pub fn clear(&mut self) {
        while let Some(mut block) = self.done.pop() {
            block.clear();
            self.spare.push(block);
        }
        self.done.clear();
    }
}

impl<T> std::ops::Deref for BlockLog<T> {
    type Target = [CappedVec<T>];

    /// # Panics
    ///
    /// When a block or a block's run overflowed.
    fn deref(&self) -> &[CappedVec<T>] {
        assert!(
            !self.done.is_overflowed() && self.done.iter().all(|b| !b.is_overflowed()),
            "the block log overflowed its room"
        );
        &self.done
    }
}
