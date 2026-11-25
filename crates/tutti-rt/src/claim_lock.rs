//! [`Claim`]: a value one holder at a time, taken by the audio side without
//! waiting or by a control thread that waits, dead for good once a holder
//! panics, and gone once the control side takes the value out.
//!
//! # The word
//!
//! A `u32`: `FREE`, `HELD`, `DEAD` or `GONE`, the last two terminal. It is
//! its own word rather than a `ClaimWord`, which has no terminal state and
//! counts shared claims that a `Claim` never grants.
//!
//! - [`try_claim`](Claim::try_claim) (audio side): one compare-exchange
//!   `FREE` to `HELD` (`Acquire`); a refusal writes nothing. Wait-free; never
//!   locks or allocates.
//! - [`claim`](Claim::claim) (control side): the same compare-exchange in a
//!   loop with a `Backoff` between tries: a spin, yields, then sleeps of at
//!   most a millisecond. It returns `None` on seeing `DEAD` or `GONE`.
//! - **Release** is one `Release` store of `FREE`, or of `DEAD` for a guard
//!   dropped by a panic that began while it was held (a `Mutex`'s poison rule:
//!   panicking now, and not panicking when the guard was taken). Only the
//!   holder writes the word while it is `HELD`, so a store is exact. The audio
//!   side that releases signals nothing: waiters poll, because a wake-up that
//!   cannot lock would need an unparker per waiter and the wait is a
//!   control-side rarity.
//! - [`take`](Claim::take) (control side): waits as `claim` does, then one
//!   compare-exchange `FREE` or `DEAD` to `GONE` (`Acquire`) and moves the
//!   value out. A claim killed by a panic still has its value, so teardown
//!   gets it.
//!
//! # Why nothing enters after `GONE`
//!
//! The compare-exchange is the only way out of `FREE` and `DEAD`, and a
//! guard's `Release` store happens before it, so the taker sees every write of
//! the last holder. After `GONE` no `try_claim` or `claim` can leave `FREE`,
//! so the value is touched by the one winner of that compare-exchange alone.
//!
//! The model in `tests/claim_loom.rs` runs this code.

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use crate::RtScope;
use crate::backoff::Backoff;
use crate::sync::{AtomicU32, Ordering, UnsafeCell};

const FREE: u32 = 0;
const HELD: u32 = 1;
const DEAD: u32 = 2;
const GONE: u32 = 3;

/// One exclusive claim on a value. See the module docs (`src/claim_lock.rs`).
pub struct Claim<T = ()> {
    word: AtomicU32,
    /// `Some` until [`take`](Self::take) moves it out; reached only by the
    /// holder (`HELD`) or the winner of the `GONE` compare-exchange.
    value: UnsafeCell<Option<T>>,
}

// SAFETY: the value is reached only through the word's protocol: by the one
// holder, or by the one winner of the terminal compare-exchange. Another
// thread can therefore receive the value by move (`take`) or by `&mut`
// (a guard), both of which need `T: Send`; `T: Sync` is never used.
unsafe impl<T: Send> Sync for Claim<T> {}

impl<T: Default> Default for Claim<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> fmt::Debug for Claim<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Claim")
            .field("word", &self.word)
            .finish_non_exhaustive()
    }
}

/// The claim held until it drops ([`Claim::try_claim`], [`Claim::claim`]); it
/// dereferences to the claimed value.
///
/// Dropping it frees the claim, or kills it when a panic began while it was
/// held: the thread is panicking now and was not when the guard was taken (a
/// `Mutex`'s poison rule). A guard taken and dropped during an unrelated unwind
/// frees the claim.
///
/// A guard over a value that is not `Sync` is not `Sync`:
///
/// ```compile_fail
/// use std::cell::Cell;
/// use tutti_rt::Claim;
///
/// fn sync<T: Sync>(_: &T) {}
/// let claim = Claim::new(Cell::new(0));
/// let guard = claim.try_claim().unwrap();
/// sync(&guard);
/// ```
#[must_use = "the claim is held only while the guard is held"]
pub struct ClaimGuard<'a, T = ()> {
    claim: &'a Claim<T>,
    /// Whether the thread was panicking when the guard was taken.
    panicking_at_take: bool,
    _held: PhantomData<&'a mut T>,
}

impl<T> fmt::Debug for ClaimGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimGuard").finish_non_exhaustive()
    }
}

impl<T> Claim<T> {
    /// A free claim on `value`.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            word: AtomicU32::new(FREE),
            value: UnsafeCell::new(Some(value)),
        }
    }

    /// Takes the claim if it is free, else `None` (held, dead or gone).
    /// Wait-free; never locks or allocates.
    pub fn try_claim(&self) -> Option<ClaimGuard<'_, T>> {
        self.word
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| ClaimGuard {
                claim: self,
                panicking_at_take: std::thread::panicking(),
                _held: PhantomData,
            })
    }

    /// Takes the claim, waiting while it is held (module docs: it backs off,
    /// never spins hot). `None` once the claim is dead or gone, including when
    /// it ends during the wait. Control side. It never returns when called
    /// from the thread that holds the claim: that thread is the one waiting.
    ///
    /// # Panics
    ///
    /// In a debug build, inside an [`RtScope`].
    pub fn claim(&self) -> Option<ClaimGuard<'_, T>> {
        RtScope::check_not_blocking("Claim::claim");
        let mut backoff = Backoff::new();
        loop {
            if let Some(guard) = self.try_claim() {
                return Some(guard);
            }
            if matches!(self.word.load(Ordering::Acquire), DEAD | GONE) {
                return None;
            }
            backoff.snooze();
        }
    }

    /// Ends the claim and moves the value out, after any holder releases it
    /// (it waits as [`claim`](Self::claim) does). A claim killed by a panic
    /// still has its value. `None` when the value was already taken. No entry
    /// runs after this returns. Control side. It never returns when called
    /// from the thread that holds the claim.
    ///
    /// # Panics
    ///
    /// In a debug build, inside an [`RtScope`].
    pub fn take(&self) -> Option<T> {
        RtScope::check_not_blocking("Claim::take");
        let mut backoff = Backoff::new();
        loop {
            let won = [FREE, DEAD].into_iter().any(|from| {
                self.word
                    .compare_exchange(from, GONE, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            });
            if won {
                // SAFETY: the compare-exchange left `FREE` or `DEAD` for `GONE`,
                // the only way out of either, so no guard exists or can be made
                // and this is the one access to the value.
                return self.value.with_mut(|p| unsafe { (*p).take() });
            }
            if self.word.load(Ordering::Acquire) == GONE {
                return None;
            }
            backoff.snooze();
        }
    }

    /// Whether the claim is dead: a holder panicked. Terminal, until
    /// [`take`](Self::take) ends it (a taken claim is gone, not dead).
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.word.load(Ordering::Acquire) == DEAD
    }
}

impl<T> Deref for ClaimGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard holds the claim (`HELD`), so this is the one
        // access to the value, and `take` has not run: it needs `FREE` or `DEAD`.
        self.claim
            .value
            .with(|p| unsafe { (*p).as_ref().expect("a held claim has its value") })
    }
}

impl<T> DerefMut for ClaimGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes the borrow exclusive.
        self.claim
            .value
            .with_mut(|p| unsafe { (*p).as_mut().expect("a held claim has its value") })
    }
}

impl<T> Drop for ClaimGuard<'_, T> {
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
        let claim = Claim::new(());
        let guard = claim.try_claim().expect("free");
        assert!(claim.try_claim().is_none());
        drop(guard);
        assert!(claim.try_claim().is_some());
    }

    /// **A waiter gets the claim once the holder releases it.**
    #[test]
    fn a_waiting_claim_is_granted_after_the_release() {
        let claim = Claim::new(());
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
        let claim = Claim::new(());
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
        let claim = Claim::new(());
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
        let claim = Claim::new(());
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _taker = TakesDuringUnwind(&claim);
            panic!("an unrelated panic");
        }));
        assert!(caught.is_err());
        assert!(!claim.is_dead());
        assert!(claim.try_claim().is_some());
    }

    /// **A guard reads and writes the claimed value**, and a later holder sees the write.
    #[test]
    fn a_guard_reads_and_writes_the_value() {
        let claim = Claim::new(5u32);
        {
            let mut guard = claim.try_claim().expect("free");
            assert_eq!(*guard, 5);
            *guard += 1;
        }
        assert_eq!(*claim.claim().expect("free"), 6);
    }

    /// **`take` waits out a holder**, then returns the holder's last write.
    #[test]
    fn take_waits_for_the_holder_and_gets_its_write() {
        let claim = Claim::new(0u32);
        let mut guard = claim.try_claim().expect("free");
        std::thread::scope(|s| {
            let taker = s.spawn(|| claim.take());
            *guard = 9;
            drop(guard);
            assert_eq!(taker.join().expect("taker"), Some(9));
        });
        assert!(!claim.is_dead(), "gone is not dead");
        assert!(claim.try_claim().is_none());
        assert!(claim.claim().is_none());
        assert_eq!(claim.take(), None);
    }

    /// **A claim killed by a panic still gives its value to `take`.**
    #[test]
    fn take_after_a_holder_panicked_gets_the_value() {
        let claim = Claim::new(3u32);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = claim.try_claim().expect("free");
            panic!("the holder panics");
        }));
        assert!(caught.is_err());
        assert_eq!(claim.take(), Some(3));
        assert_eq!(claim.take(), None);
    }

    /// Counts its drops.
    struct Counted<'a>(&'a std::sync::atomic::AtomicU32);
    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// **The value drops once**: with an untaken claim, after a `take`, and
    /// with a claim killed by a panic.
    #[test]
    fn the_value_drops_exactly_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let drops = AtomicU32::new(0);
        drop(Claim::new(Counted(&drops)));
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        let claim = Claim::new(Counted(&drops));
        drop(claim.take().expect("present"));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        drop(claim);
        assert_eq!(drops.load(Ordering::Relaxed), 2);

        let claim = Claim::new(Counted(&drops));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = claim.try_claim().expect("free");
            panic!("the holder panics");
        }));
        assert!(caught.is_err());
        drop(claim);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }
}
