//! [`Claim`]: one holder at a time, taken by the audio side without waiting
//! or by a control thread that waits, and killable for good.
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
//! - **Release** is one `Release` store of `FREE`, or of `DEAD` for
//!   [`kill`](ClaimGuard::kill) and a guard dropped by a panic that began
//!   while it was held (a `Mutex`'s poison rule: panicking now, and not
//!   panicking when the guard was taken). Only the holder writes the word while it is `HELD`, so a
//!   store is exact. The audio side that releases signals nothing: waiters poll,
//!   because a wake-up that cannot lock would need an unparker per waiter and
//!   the wait is a control-side rarity.
//!
//! # Identity
//!
//! The holder may publish a thread identity ([`ClaimGuard::publish_identity`]):
//! a per-thread number from a counter, in a second word, stored `Release` and
//! cleared before the claim is released. [`Claim::is_held_by_current_thread`]
//! reads it `Acquire`. Only the holder writes it, and a reader asks about its own
//! thread, which can only be the holder's, so the two words need not change
//! together: a stale read is another thread's number or zero, never the
//! reader's own. A taker that does not publish (a lifecycle taker) leaves it
//! zero.
//!
//! The model in `tests/claim_loom.rs` runs this code.

use crate::sync::{AtomicU32, AtomicU64, Ordering};

use crate::backoff::Backoff;
#[cfg(not(loom))]
use crate::mint::Mint;

const FREE: u32 = 0;
const HELD: u32 = 1;
const DEAD: u32 = 2;

/// The number of the current thread: nonzero, stable for its life, unique
/// among live and dead threads of the process.
#[cfg(not(loom))]
fn current_thread_number() -> u64 {
    use std::cell::Cell;
    static NEXT: Mint = Mint::new();
    thread_local! { static ID: Cell<u64> = const { Cell::new(0) }; }
    ID.with(|id| {
        if id.get() == 0 {
            id.set(NEXT.mint().get());
        }
        id.get()
    })
}

/// The number of the current model thread: a hash of its `loom` id (loom's
/// threads share one OS thread, so a thread-local would be shared).
#[cfg(loom)]
fn current_thread_number() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    loom::thread::current().id().hash(&mut hasher);
    hasher.finish() | 1
}

/// One exclusive claim. See the module docs (`src/claim_lock.rs`).
#[derive(Debug)]
pub struct Claim {
    word: AtomicU32,
    owner: AtomicU64,
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
    published: bool,
    kill: bool,
    /// Whether the thread was panicking when the guard was taken.
    panicking_at_take: bool,
}

impl Claim {
    /// A free claim.
    #[must_use]
    pub fn new() -> Self {
        Self {
            word: AtomicU32::new(FREE),
            owner: AtomicU64::new(0),
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
                published: false,
                kill: false,
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

    /// Whether the claim is dead: killed, or its holder panicked. Terminal.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.word.load(Ordering::Acquire) == DEAD
    }

    /// Whether the current thread holds the claim and published its
    /// identity ([`ClaimGuard::publish_identity`]).
    #[must_use]
    pub fn is_held_by_current_thread(&self) -> bool {
        self.owner.load(Ordering::Acquire) == current_thread_number()
    }
}

impl ClaimGuard<'_> {
    /// Records the current thread as the holder, for
    /// [`Claim::is_held_by_current_thread`] until the guard drops. The guard
    /// is the holder's, so call it on the thread that holds it.
    pub fn publish_identity(&mut self) {
        self.claim
            .owner
            .store(current_thread_number(), Ordering::Release);
        self.published = true;
    }

    /// Runs `f` with the current thread published as the holder, and clears
    /// the identity when `f` returns or unwinds, while the claim stays held: a
    /// holder that acts as another role for one span (a lifecycle claim that
    /// runs a stop as the audio thread). The identity is cleared afterwards
    /// even if it was published before.
    pub fn as_identity<R>(&mut self, f: impl FnOnce() -> R) -> R {
        struct Clear<'g, 'a>(&'g mut ClaimGuard<'a>);
        impl Drop for Clear<'_, '_> {
            fn drop(&mut self) {
                self.0.claim.owner.store(0, Ordering::Release);
                self.0.published = false;
            }
        }
        self.publish_identity();
        let _clear = Clear(self);
        f()
    }

    /// Marks the claim dead for good and releases it: every later take, and
    /// every waiter, gets `None`.
    pub fn kill(mut self) {
        self.kill = true;
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if self.published {
            self.claim.owner.store(0, Ordering::Release);
        }
        let next = if self.kill || (std::thread::panicking() && !self.panicking_at_take) {
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

    /// **The identity is the holder's, from publish to release.**
    #[test]
    fn identity_is_published_by_the_holder_and_cleared_on_release() {
        let claim = Claim::new();
        let mut guard = claim.try_claim().expect("free");
        assert!(!claim.is_held_by_current_thread(), "not published yet");
        guard.publish_identity();
        assert!(claim.is_held_by_current_thread());
        std::thread::scope(|s| {
            s.spawn(|| assert!(!claim.is_held_by_current_thread()))
                .join()
                .expect("other thread");
        });
        drop(guard);
        assert!(!claim.is_held_by_current_thread());
    }

    /// **A scoped identity is the holder's inside the span only**, the claim
    /// staying held after it, and an unwind out of the span clears it too.
    #[test]
    fn a_scoped_identity_ends_with_its_span_and_the_claim_stays_held() {
        let claim = Claim::new();
        let mut guard = claim.claim().expect("free");
        let inside = guard.as_identity(|| claim.is_held_by_current_thread());
        assert!(inside, "published for the span");
        assert!(!claim.is_held_by_current_thread(), "cleared after it");
        assert!(claim.try_claim().is_none(), "the claim is still held");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.as_identity(|| panic!("inside the span"));
        }));
        assert!(unwound.is_err());
        assert!(!claim.is_held_by_current_thread(), "cleared by the unwind");
        drop(guard);
        assert!(
            !claim.is_dead(),
            "the panic began and ended inside the span, not at the release"
        );
    }

    /// **A taker that does not publish is no thread's holder.**
    #[test]
    fn an_unpublished_holder_has_no_identity() {
        let claim = Claim::new();
        let _guard = claim.claim().expect("free");
        assert!(!claim.is_held_by_current_thread());
    }

    /// **Kill is terminal**: nothing takes the claim afterwards.
    #[test]
    fn a_killed_claim_is_dead_for_every_taker() {
        let claim = Claim::new();
        claim.try_claim().expect("free").kill();
        assert!(claim.is_dead());
        assert!(claim.try_claim().is_none());
        assert!(claim.claim().is_none());
    }

    /// **A waiter parked in `claim` gets `None` when the holder kills it.**
    #[test]
    fn a_waiter_gets_none_when_the_holder_kills() {
        let claim = Claim::new();
        let guard = claim.try_claim().expect("free");
        std::thread::scope(|s| {
            let waiter = s.spawn(|| claim.claim().is_none());
            guard.kill();
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
