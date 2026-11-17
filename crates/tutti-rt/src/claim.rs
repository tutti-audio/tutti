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
//! compare-exchange with `Acquire`, a release is `Release`, so whatever a
//! holder wrote is visible to the next claimer of the word.
//!
//! # A refused claim writes nothing
//!
//! Every claim is a compare-exchange, and a refused one is a load: the word is
//! changed only by the claims that hold it. Two things follow.
//!
//! - **An exclusive release is a store of zero.** While the exclusive bit is
//!   set no other claim writes the word, so the holder's store cannot lose an
//!   update.
//! - **A refusal is never observable.** A shared claim that added one before
//!   it looked at the exclusive bit (a `fetch_add`, undone on refusal) would,
//!   for that instant, refuse a concurrent exclusive claim on a word nobody
//!   holds, and under `loom` 0.7 its undo is a write left unordered against
//!   the holder's release, which reports a phantom refusal after every claim
//!   has been released. The compare-exchange loop retries only when another
//!   *shared* claim changed the count in between, so it is lock-free: some
//!   claim always makes progress, and an exclusive holder never makes one
//!   wait.
//!
//! The price against a `fetch_add` is a load before the compare-exchange; an
//! uncontended claim is still one read-modify-write.

use sync::{AtomicU32, Ordering};

/// The word's atomic: `std`'s in a real build, `loom`'s under `--cfg loom`, so
/// the models check this code rather than a replica.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU32, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU32, Ordering};
}

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
    /// reach the exclusive bit; `false` then, with the word unchanged.
    #[inline]
    pub(crate) fn try_shared(&self) -> bool {
        let mut seen = self.0.load(Ordering::Relaxed);
        loop {
            if seen >= EXCLUSIVE - 1 {
                return false;
            }
            match self
                .0
                .compare_exchange_weak(seen, seen + 1, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
    }

    /// Takes the word if nothing holds it; `false` if anything does.
    #[inline]
    pub(crate) fn try_exclusive(&self) -> bool {
        self.0
            .compare_exchange(0, EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Turns the caller's shared claim into an exclusive one if it is the only
    /// shared claim; `false` if another is held, with the caller's claim kept.
    #[inline]
    pub(crate) fn try_upgrade(&self) -> bool {
        self.0
            .compare_exchange(1, EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Drops one shared claim.
    #[inline]
    pub(crate) fn release_shared(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }

    /// Drops the exclusive claim. A store: see the module docs.
    #[inline]
    pub(crate) fn release_exclusive(&self) {
        self.0.store(0, Ordering::Release);
    }
}
