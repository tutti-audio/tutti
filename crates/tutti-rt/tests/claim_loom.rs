//! A `loom` model of [`Claim`]: the shipped code, whose atomics and value cell
//! are loom's under the flag (see `src/sync.rs`).
//!
//! ```text
//! cargo test -p tutti-rt --release --test claim_loom   # with the loom cfg flag set
//! ```
//!
//! The claimed value is the claim's own, so loom reports two holders at once,
//! or a holder not ordered after the last release, as a data race. Over every
//! interleaving:
//!
//! - **never two holders**, whether the takers are two `try_claim`s or a
//!   `try_claim` against a `claim`;
//! - **a waiter wakes after the release** and sees the holder's writes;
//! - **a holder's panic is terminal**: a waiter gets `None`, and nobody claims
//!   after Dead;
//! - **`take` ends the claim**: it never overlaps a holder, nothing claims
//!   after it, two `take`s give the value to exactly one, and a claim killed
//!   by a panic still gives its value.

#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;
use tutti_rt::Claim;

#[test]
fn two_try_claims_are_never_both_holders() {
    loom::model(|| {
        let claim = Arc::new(Claim::new(0u32));
        let other = {
            let claim = claim.clone();
            thread::spawn(move || claim.try_claim().map(|mut g| *g += 1).is_some())
        };
        let mine = claim.try_claim().map(|mut g| *g += 1).is_some();
        let theirs = other.join().unwrap();
        assert_eq!(
            *claim.claim().expect("never dead"),
            u32::from(mine) + u32::from(theirs)
        );
    });
}

#[test]
fn a_waiter_is_granted_after_the_release_and_sees_its_writes() {
    loom::model(|| {
        let claim = Arc::new(Claim::new(0u32));
        let audio = {
            let claim = claim.clone();
            thread::spawn(move || claim.try_claim().map(|mut g| *g += 1).is_some())
        };
        let mut guard = claim.claim().expect("never dead");
        *guard += 1;
        drop(guard);
        let audio_won = audio.join().unwrap();
        assert_eq!(
            *claim.claim().expect("never dead"),
            1 + u32::from(audio_won)
        );
    });
}

#[test]
fn a_parked_waiter_gets_none_when_the_holder_panics() {
    loom::model(|| {
        let claim = Arc::new(Claim::new(()));
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

#[test]
fn take_and_try_claim_are_never_both_holders_and_nothing_claims_after_gone() {
    loom::model(|| {
        let claim = Arc::new(Claim::new(0u32));
        let audio = {
            let claim = claim.clone();
            thread::spawn(move || claim.try_claim().map(|mut g| *g += 1).is_some())
        };
        let taken = claim.take().expect("the only taker");
        let audio_won = audio.join().unwrap();
        // The audio side either ran whole before `take` or was refused.
        assert_eq!(taken, u32::from(audio_won));
        assert!(claim.try_claim().is_none());
        assert!(!claim.is_dead(), "gone is not dead");
    });
}

#[test]
fn two_takes_give_the_value_to_exactly_one() {
    loom::model(|| {
        let claim = Arc::new(Claim::new(7u32));
        let other = {
            let claim = claim.clone();
            thread::spawn(move || claim.take())
        };
        let mine = claim.take();
        let theirs = other.join().unwrap();
        assert_eq!(mine.xor(theirs), Some(7), "exactly one take gets the value");
    });
}

#[test]
fn take_after_a_holder_panicked_gets_the_value() {
    loom::model(|| {
        let claim = Arc::new(Claim::new(4u32));
        let guard = claim.try_claim().expect("free");
        let taker = {
            let claim = claim.clone();
            thread::spawn(move || claim.take())
        };
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("the holder panics");
        }));
        assert!(caught.is_err());
        assert_eq!(taker.join().unwrap(), Some(4));
    });
}
