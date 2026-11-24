//! A `loom` model of [`Claim`]: the shipped code, whose atomics are loom's
//! under the flag (see `src/claim_lock.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test claim_loom   # with the loom cfg flag set
//! ```
//!
//! The claimed thing is a `loom::cell::UnsafeCell` written by each holder, so
//! loom reports two holders at once, or a holder not ordered after the last
//! release. Over every interleaving:
//!
//! - **never two holders**, whether the takers are two `try_claim`s or a
//!   `try_claim` against a `claim`;
//! - **a waiter wakes after the release** and sees the holder's writes;
//! - **a holder's panic is terminal**: a waiter gets `None`, and nobody claims
//!   after Dead.

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;
use tutti_rt::Claim;

struct Thing(UnsafeCell<u32>);

// SAFETY (test): shared only through the claim's protocol, which the model checks.
unsafe impl Sync for Thing {}

impl Thing {
    fn bump(&self) {
        // SAFETY (test): an exclusive access loom checks.
        self.0.with_mut(|p| unsafe { *p += 1 });
    }
    fn get(&self) -> u32 {
        // SAFETY (test): a shared access loom checks.
        self.0.with(|p| unsafe { *p })
    }
}

#[test]
fn two_try_claims_are_never_both_holders() {
    loom::model(|| {
        let claim = Arc::new(Claim::new());
        let thing = Arc::new(Thing(UnsafeCell::new(0)));
        let other = {
            let (claim, thing) = (claim.clone(), thing.clone());
            thread::spawn(move || claim.try_claim().map(|_g| thing.bump()).is_some())
        };
        let mine = claim.try_claim().map(|_g| thing.bump()).is_some();
        let theirs = other.join().unwrap();
        assert_eq!(thing.get(), u32::from(mine) + u32::from(theirs));
    });
}

#[test]
fn a_waiter_is_granted_after_the_release_and_sees_its_writes() {
    loom::model(|| {
        let claim = Arc::new(Claim::new());
        let thing = Arc::new(Thing(UnsafeCell::new(0)));
        let audio = {
            let (claim, thing) = (claim.clone(), thing.clone());
            thread::spawn(move || claim.try_claim().map(|_g| thing.bump()).is_some())
        };
        let guard = claim.claim().expect("never dead");
        thing.bump();
        drop(guard);
        let audio_won = audio.join().unwrap();
        assert_eq!(thing.get(), 1 + u32::from(audio_won));
    });
}

#[test]
fn a_parked_waiter_gets_none_when_the_holder_panics() {
    loom::model(|| {
        let claim = Arc::new(Claim::new());
        let guard = claim.try_claim().expect("free");
        let waiter = {
            let claim = claim.clone();
            thread::spawn(move || claim.claim().is_none())
        };
        let racer = {
            let claim = claim.clone();
            thread::spawn(move || claim.try_claim().is_none())
        };
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("the holder panics");
        }));
        assert!(caught.is_err());
        assert!(waiter.join().unwrap(), "the waiter wakes and gets None");
        assert!(
            racer.join().unwrap(),
            "nobody claims while held or after Dead"
        );
        assert!(claim.is_dead());
        assert!(claim.try_claim().is_none());
    });
}
