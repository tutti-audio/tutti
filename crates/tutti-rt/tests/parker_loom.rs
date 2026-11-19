//! A `loom` model of [`Unparker`]: the shipped code, whose atomics, fences
//! and park are loom's under the flag (see `src/parker.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test parker_loom   # with the loom cfg flag set
//! ```
//!
//! The parker parks until a flag the unparker sets, and the unparker then
//! unparks it: the shape of every user (a pool's gate, a bridge's queue).
//! Loom reports a lost wake-up as a deadlock (the parker parked, nobody left
//! to unpark it). Over every interleaving:
//!
//! - **unpark before the park** (`an_unpark_before_the_park_is_not_lost`):
//!   the unparker may run entirely before, during or after the parker's
//!   `park`;
//! - **unpark during the park**: the same model covers the unpark landing
//!   between the parker's `parked` store, its check and its park;
//! - **many unparks** (`several_unparks_wake_one_parker_for_each_round`): a
//!   parker that is unparked for two rounds in a row wakes for both,
//!   whichever unpark finds it parked, and an unpark that finds it awake
//!   leaves at most a stale token (a spurious return, which the loop
//!   absorbs);
//! - **one fence, many unparkers** (`unpark_first_*`): the pool's `wake`,
//!   unparking two parkers after one fence;
//! - **two unparkers** (`two_unparkers_one_parker`): both may unpark; the
//!   parker sees the work either way.
//!
//! # The swaps
//!
//! Every write to `parked` is a `swap`, not a store. With a plain `store`
//! for the parker's arm and disarm, loom reports a lost wake-up in
//! `several_unparks_wake_one_parker_for_each_round` from 4 preemptions (the
//! other tests still pass): the first unpark's `swap` reads the parker's
//! disarm, the parker's next arm is then ordered inside that `swap`'s
//! read-modify-write, and the second unpark reads the `false`. That
//! execution is a loom artefact: under the C++20 model a read-modify-write
//! is atomic, so no store can fall inside it, and a plain-store `Unparker`
//! is correct. The swaps keep the model honest: with every write to `parked`
//! a read-modify-write they are totally ordered, and loom has no such
//! execution to report. Because the artefact needs 4 preemptions, CI and
//! `cargo xtask loom` run this model unbounded.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use tutti_rt::{Unparker, parker};

#[test]
fn an_unpark_before_the_park_is_not_lost() {
    loom::model(|| {
        let (unparker, mut parker) = parker();
        let work = Arc::new(AtomicUsize::new(0));
        let waker = {
            let work = Arc::clone(&work);
            thread::spawn(move || {
                work.store(1, Ordering::Release);
                unparker.unpark();
            })
        };
        while work.load(Ordering::Acquire) == 0 {
            parker.park(|| work.load(Ordering::Acquire) != 0);
        }
        waker.join().unwrap();
    });
}

#[test]
fn several_unparks_wake_one_parker_for_each_round() {
    loom::model(|| {
        let (unparker, mut parker) = parker();
        let work = Arc::new(AtomicUsize::new(0));
        let waker = {
            let work = Arc::clone(&work);
            thread::spawn(move || {
                for round in 1..=2 {
                    work.store(round, Ordering::Release);
                    unparker.unpark();
                }
            })
        };
        let mut seen = 0;
        while seen < 2 {
            let now = work.load(Ordering::Acquire);
            if now > seen {
                seen = now;
                continue;
            }
            parker.park(|| work.load(Ordering::Acquire) > seen);
        }
        waker.join().unwrap();
    });
}

#[test]
fn two_unparkers_one_parker() {
    loom::model(|| {
        let (unparker, mut parker) = parker();
        let work = Arc::new(AtomicUsize::new(0));
        let wakers: Vec<_> = (0..2)
            .map(|_| {
                let (unparker, work) = (unparker.clone(), Arc::clone(&work));
                thread::spawn(move || {
                    work.fetch_add(1, Ordering::Release);
                    unparker.unpark();
                })
            })
            .collect();
        while work.load(Ordering::Acquire) < 2 {
            parker.park(|| work.load(Ordering::Acquire) >= 2);
        }
        for w in wakers {
            w.join().unwrap();
        }
    });
}

/// One `unpark_first` after a single publish wakes two parkers through their unparkers:
/// the shipped one-fence path the pool's `wake` takes.
#[test]
fn unpark_first_wakes_every_parker_it_is_asked_to() {
    loom::model(|| {
        let (unparkers, parkers): (Vec<_>, Vec<_>) = (0..2).map(|_| parker()).unzip();
        let work = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = parkers
            .into_iter()
            .map(|mut parker| {
                let work = Arc::clone(&work);
                thread::spawn(move || {
                    while work.load(Ordering::Acquire) == 0 {
                        parker.park(|| work.load(Ordering::Acquire) != 0);
                    }
                })
            })
            .collect();
        work.store(1, Ordering::Release);
        Unparker::unpark_first(&unparkers, 2);
        for t in threads {
            t.join().unwrap();
        }
    });
}
