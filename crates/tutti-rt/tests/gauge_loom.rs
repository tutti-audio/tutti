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
//! - **a gauge is never torn**: a read returns the initial value or one store.

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
