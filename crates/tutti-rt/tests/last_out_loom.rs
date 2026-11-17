//! A `loom` model of [`LastOut`]: the shipped code, whose atomic is loom's
//! under the flag (see `src/last_out.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test last_out_loom   # with the loom cfg flag set
//! ```
//!
//! Each party writes its own `loom::cell::UnsafeCell` before it leaves; the
//! one that takes the value reads both. Over every interleaving, with both
//! parties racing:
//!
//! - **exactly one `leave` returns the value**;
//! - **the taker sees both parties' writes** (loom reports a missing
//!   happens-before as a causality violation).

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;
use tutti_rt::{LastOut, Party};

struct Cells([UnsafeCell<u32>; 2]);

// SAFETY (test): each party writes its own cell; the taker reads after both
// leaves, which is what the model checks.
unsafe impl Sync for Cells {}

impl Cells {
    fn write(&self, i: usize) {
        // SAFETY (test): an exclusive access loom checks.
        self.0[i].with_mut(|p| unsafe { *p = 1 });
    }
    fn sum(&self) -> u32 {
        // SAFETY (test): shared accesses loom checks.
        self.0.iter().map(|c| c.with(|p| unsafe { *p })).sum()
    }
}

#[test]
fn exactly_one_party_takes_the_value_and_sees_both_writes() {
    loom::model(|| {
        let cells = Arc::new(Cells([UnsafeCell::new(0), UnsafeCell::new(0)]));
        let out = Arc::new(LastOut::new(()));
        let party = |p: Party, i: usize| {
            let (cells, out) = (cells.clone(), out.clone());
            thread::spawn(move || {
                cells.write(i);
                out.leave(p).map(|()| cells.sum())
            })
        };
        let a = party(Party::A, 0);
        let b = party(Party::B, 1);
        let (a, b) = (a.join().unwrap(), b.join().unwrap());
        let taken: Vec<u32> = a.into_iter().chain(b).collect();
        assert_eq!(taken, [2], "one taker, who saw both writes");
    });
}

#[test]
fn a_value_not_taken_is_dropped_once() {
    loom::model(|| {
        let value = Arc::new(());
        let out = Arc::new(LastOut::new(value.clone()));
        let other = {
            let out = out.clone();
            thread::spawn(move || out.leave(Party::B).is_some())
        };
        let mine = out.leave(Party::A).is_some();
        let theirs = other.join().unwrap();
        assert!(mine ^ theirs);
        drop(out);
        assert_eq!(Arc::strong_count(&value), 1);
    });
}
