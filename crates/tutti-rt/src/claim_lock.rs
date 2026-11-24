//! [`Claim`]: one holder at a time, taken by the audio side without waiting
//! or by a control thread that waits, and dead for good once a holder panics.
//!
//! # The word
//!
//! A `u32`: `FREE`, `HELD` or `DEAD` (terminal). It is its own word rather
//! than a `ClaimWord`, which has no terminal state and counts shared claims
//! that a `Claim` never grants.
//!
//! - [`try_claim`](Claim::try_claim) (audio side): one compare-exchange
//!   `FREE` to `HELD` (`Acquire`); a refusal writes nothing. Wait-free; never
//!   locks or allocates.
//! - [`claim`](Claim::claim) (control side): the same compare-exchange in a
//!   loop with a `Backoff` between tries: a spin, yields, then sleeps of at
//!   most a millisecond. It returns `None` on seeing `DEAD`.
//! - **Release** is one `Release` store of `FREE`, or of `DEAD` for a guard
//!   dropped by a panic that began while it was held (a `Mutex`'s poison rule:
//!   panicking now, and not panicking when the guard was taken). Only the
//!   holder writes the word while it is `HELD`, so a store is exact. The audio
//!   side that releases signals nothing: waiters poll, because a wake-up that
//!   cannot lock would need an unparker per waiter and the wait is a
//!   control-side rarity.
//!
//! The model in `tests/claim_loom.rs` runs this code.

use crate::sync::{AtomicU32, Ordering};

use crate::backoff::Backoff;

const FREE: u32 = 0;
const HELD: u32 = 1;
const DEAD: u32 = 2;

/// One exclusive claim. See the module docs (`src/claim_lock.rs`).
#[derive(Debug)]
pub struct Claim {
    word: AtomicU32,
}

impl Default for Claim {
    fn default() -> Self {
        Self::new()
    }
}

/// The claim held until it drops ([`Claim::try_claim`], [`Claim::claim`]).
///
/// Dropping it frees the claim, or kills it when a panic began while it was
/// held: the thread is panicking now and was not when the guard was taken (a
/// `Mutex`'s poison rule). A guard taken and dropped during an unrelated unwind
/// frees the claim.
#[derive(Debug)]
#[must_use = "the claim is held only while the guard is held"]
pub struct ClaimGuard<'a> {
    claim: &'a Claim,
    /// Whether the thread was panicking when the guard was taken.
    panicking_at_take: bool,
}

impl Claim {
    /// A free claim.
    #[must_use]
    pub fn new() -> Self {
        Self {
            word: AtomicU32::new(FREE),
        }
    }

    /// Takes the claim if it is free, else `None` (held or dead). Wait-free;
    /// never locks or allocates.
    pub fn try_claim(&self) -> Option<ClaimGuard<'_>> {
        self.word
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| ClaimGuard {
                claim: self,
                panicking_at_take: std::thread::panicking(),
            })
    }

    /// Takes the claim, waiting while it is held (module docs: it backs off,
    /// never spins hot). `None` once the claim is dead, including when it dies
    /// during the wait. Control side. It never returns when called from the
    /// thread that holds the claim: that thread is the one waiting.
    pub fn claim(&self) -> Option<ClaimGuard<'_>> {
        let mut backoff = Backoff::new();
        loop {
            if let Some(guard) = self.try_claim() {
                return Some(guard);
            }
            if self.is_dead() {
                return None;
            }
            backoff.snooze();
        }
    }

    /// Whether the claim is dead: its holder panicked. Terminal.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.word.load(Ordering::Acquire) == DEAD
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        let next = if std::thread::panicking() && !self.panicking_at_take {
            DEAD
        } else {
            FREE
        };
        self.claim.word.store(next, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **One holder at a time**: a second `try_claim` is refused until the guard drops.
    #[test]
    fn a_held_claim_refuses_try_claim() {
        let claim = Claim::new();
        let guard = claim.try_claim().expect("free");
        assert!(claim.try_claim().is_none());
        drop(guard);
        assert!(claim.try_claim().is_some());
    }

    /// **A waiter gets the claim once the holder releases it.**
    #[test]
    fn a_waiting_claim_is_granted_after_the_release() {
        let claim = Claim::new();
        let guard = claim.try_claim().expect("free");
        std::thread::scope(|s| {
            let waiter = s.spawn(|| claim.claim().is_some());
            drop(guard);
            assert!(waiter.join().expect("waiter"));
        });
    }

    /// **A waiter parked in `claim` gets `None` when the holder panics.**
    #[test]
    fn a_waiter_gets_none_when_the_holder_panics() {
        let claim = Claim::new();
        let guard = claim.try_claim().expect("free");
        std::thread::scope(|s| {
            let waiter = s.spawn(|| claim.claim().is_none());
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _guard = guard;
                panic!("the holder panics");
            }));
            assert!(caught.is_err());
            assert!(waiter.join().expect("waiter"));
        });
    }

    /// **A guard dropped by a panic that began while held kills the claim.**
    #[test]
    fn a_guard_dropped_while_panicking_kills_the_claim() {
        let claim = Claim::new();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = claim.try_claim().expect("free");
            panic!("the holder panics");
        }));
        assert!(caught.is_err());
        assert!(claim.is_dead());
        assert!(claim.try_claim().is_none());
        assert!(claim.claim().is_none());
    }

    /// **A guard taken and dropped during an unrelated unwind leaves the claim usable.**
    #[test]
    fn a_guard_taken_during_an_unwind_does_not_kill_the_claim() {
        struct TakesDuringUnwind<'a>(&'a Claim);
        impl Drop for TakesDuringUnwind<'_> {
            fn drop(&mut self) {
                drop(self.0.try_claim().expect("free"));
            }
        }
        let claim = Claim::new();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _taker = TakesDuringUnwind(&claim);
            panic!("an unrelated panic");
        }));
        assert!(caught.is_err());
        assert!(!claim.is_dead());
        assert!(claim.try_claim().is_some());
    }
}
