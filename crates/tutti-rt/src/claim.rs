//! The claim word: one atomic that grants either any number of shared claims
//! or one exclusive claim, never both.
//!
//! [`AudioThreadCell`](crate::AudioThreadCell) has one, and every element of a
//! [`ClaimTable`](crate::ClaimTable) has one. The `loom` models in
//! `tests/cell_loom.rs` and `tests/split_loom.rs` run this code through both.
//!
//! # The word
//!
//! A `u32`: [`EXCLUSIVE`] (the top bit) while an exclusive claim is held,
//! otherwise the number of shared claims. A claim is granted by one
//! atomic read-modify-write with `Acquire`, a release is `Release`, so whatever a
//! holder wrote is visible to the next claimer of the word.
//!
//! # Claims
//!
//! A shared claim is one `fetch_add` of one: it adds before it looks at the
//! exclusive bit, and a refused one subtracts it again. For that instant a
//! concurrent exclusive claim on the same word is refused too, and only a
//! schedule that already conflicts can see it. A shared claim never retries.
//!
//! An exclusive claim and an upgrade are compare-exchanges, so a refused one
//! writes nothing. An exclusive release subtracts the exclusive bit rather
//! than storing zero, so it keeps a refused shared claim's add that is not yet
//! undone.

use crate::sync::{AtomicU32, Ordering};

/// The word's atomic: `std`'s in a real build, `loom`'s under `--cfg loom`, so
/// the models check this code rather than a replica.
/// The exclusive bit. Below it: the number of shared claims.
const EXCLUSIVE: u32 = 1 << 31;

/// One shared-or-exclusive claim state. See the module docs.
#[derive(Debug)]
pub(crate) struct ClaimWord(AtomicU32);

impl ClaimWord {
    /// A word with no claim on it.
    #[cfg(not(loom))]
    pub(crate) const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// A word with no claim on it (`loom`'s atomics are not `const`).
    #[cfg(loom)]
    pub(crate) fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Releases every claim. `&mut` proves none is live; a claim whose holder
    /// was leaked is discarded.
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Adds a shared claim unless an exclusive one is held, or the count would
    /// reach the exclusive bit; `false` then, with the word restored.
    #[inline]
    pub(crate) fn claim_shared(&self) -> bool {
        let prev = self.0.fetch_add(1, Ordering::Acquire);
        if prev & EXCLUSIVE != 0 || prev + 1 >= EXCLUSIVE {
            // Leave the word as found.
            self.0.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Takes the word if nothing holds it; `false` if anything does.
    #[inline]
    pub(crate) fn claim_exclusive(&self) -> bool {
        self.0
            .compare_exchange(0, EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Turns the caller's shared claim into an exclusive one if it is the only
    /// shared claim; `false` if another is held, with the caller's claim kept.
    #[inline]
    pub(crate) fn upgrade(&self) -> bool {
        self.0
            .compare_exchange(1, EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Drops one shared claim.
    #[inline]
    pub(crate) fn release_shared(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }

    /// Drops the exclusive claim: see the module docs.
    #[inline]
    pub(crate) fn release_exclusive(&self) {
        self.0.fetch_sub(EXCLUSIVE, Ordering::Release);
    }
}
