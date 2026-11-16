//! A `loom` model of [`Gauge`] and [`Counter`]: the shipped code, whose atomics
//! are loom's under the loom cfg (see `src/gauge.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test gauge_loom   (with the loom cfg flag set)
//! ```
//!
//! Over every interleaving of one writer and one reader it asserts:
//!
//! - **a counter never goes back**: two reads in program order never see a
//!   smaller total;
//! - **closed is final**: a reader that sees the cell closed reads every value
//!   the writer wrote (the last gauge value, the whole count);
//! - **a gauge is never torn**: a read returns the initial value or one store;
//! - **a reopened writer starts from the last value** the dropped one wrote;
//! - **of two concurrent reopens exactly one wins**, and none wins while a
//!   writer is live.

#![cfg(loom)]

use loom::thread;
use tutti_rt::{Reading, counter, gauge};

/// Two halves of one word: a torn read would pair halves of two stores.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pair([u32; 2]);

impl Reading for Pair {
    fn to_bits(self) -> u64 {
        u64::from(self.0[0]) << 32 | u64::from(self.0[1])
    }
    fn from_bits(bits: u64) -> Self {
        Self([(bits >> 32) as u32, bits as u32])
    }
}

#[test]
fn a_counter_never_goes_back_and_closed_is_final() {
    loom::model(|| {
        let (mut writer, total) = counter::<u64>();
        let observer = thread::spawn(move || {
            let closed = total.is_closed();
            let first = total.get();
            let second = total.get();
            assert!(
                second >= first,
                "a count went back from {first} to {second}"
            );
            if closed {
                assert_eq!(first, 3, "a closed counter read short");
            }
        });
        writer.add(1);
        writer.add(2);
        drop(writer);
        observer.join().unwrap();
    });
}

#[test]
fn a_gauge_reads_one_store_and_closed_is_final() {
    loom::model(|| {
        let (mut writer, level) = gauge(Pair([0, 0]));
        let observer = thread::spawn(move || {
            let closed = level.is_closed();
            let seen = level.get();
            assert!(
                matches!(seen, Pair([0, 0] | [1, 1] | [2, 2])),
                "a torn gauge: {seen:?}"
            );
            if closed {
                assert_eq!(
                    seen,
                    Pair([2, 2]),
                    "a closed gauge read before its last write"
                );
            }
        });
        writer.set(Pair([1, 1]));
        writer.set(Pair([2, 2]));
        drop(writer);
        observer.join().unwrap();
    });
}

#[test]
fn a_reopened_writer_sees_the_last_value_of_the_dropped_one() {
    loom::model(|| {
        let (mut writer, total) = counter::<u64>();
        let reopener = total.clone();
        let handle = thread::spawn(move || {
            // Whenever the reopen wins, it follows the drop: the count read is
            // whole, and adding carries on from it.
            if let Some(mut next) = reopener.reopen() {
                assert_eq!(reopener.get(), 3, "a reopened counter read short");
                next.add(1);
                assert_eq!(reopener.get(), 4);
                true
            } else {
                false
            }
        });
        writer.add(1);
        writer.add(2);
        drop(writer);
        let won = handle.join().unwrap();
        if !won {
            // The reopen ran while the writer was live; it is closed now.
            assert!(total.reopen().is_some());
        }
    });
}

#[test]
fn a_reopened_gauge_holds_the_last_value() {
    loom::model(|| {
        let (mut writer, level) = gauge(Pair([0, 0]));
        let reopener = level.clone();
        let handle = thread::spawn(move || {
            if let Some(_next) = reopener.reopen() {
                assert_eq!(reopener.get(), Pair([2, 2]));
            }
        });
        writer.set(Pair([1, 1]));
        writer.set(Pair([2, 2]));
        drop(writer);
        handle.join().unwrap();
    });
}

#[test]
fn of_two_concurrent_reopens_exactly_one_wins() {
    loom::model(|| {
        let (writer, total) = counter::<u64>();
        drop(writer);
        let other = total.clone();
        let handle = thread::spawn(move || other.reopen());
        let mine = total.reopen();
        // Both writers are held until the count: a dropped one would close the
        // cell again for the other to win.
        let theirs = handle.join().unwrap();
        assert_eq!(
            u32::from(mine.is_some()) + u32::from(theirs.is_some()),
            1,
            "two writers or none"
        );
    });
}
