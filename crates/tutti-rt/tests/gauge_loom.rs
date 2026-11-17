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
//! - **a multi-word counter's tallies each never go back**, and a closed one
//!   reads whole (every tally of every add);
//! - **a tally with two writers**: each word never goes back and is a prefix
//!   total mid-run, and the total is exact once both writers have joined;
//! - **a gauge is never torn**: a read returns the initial value or one store;
//! - **a reopened writer starts from the last value** the dropped one wrote;
//! - **of two concurrent reopens exactly one wins**, and none wins while a
//!   writer is live.

#![cfg(loom)]

use loom::thread;
use tutti_rt::{Count, Reading, counter, gauge, tally};

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

/// Two tallies, each a word of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Two([u64; 2]);

impl Count for Two {
    type Words = [u64; 2];
    fn to_words(self) -> [u64; 2] {
        self.0
    }
    fn from_words(words: [u64; 2]) -> Self {
        Self(words)
    }
}

#[test]
fn each_tally_of_a_counter_never_goes_back_and_closed_is_whole() {
    loom::model(|| {
        let (mut writer, total) = counter::<Two>();
        let observer = thread::spawn(move || {
            let closed = total.is_closed();
            let Two([a1, b1]) = total.get();
            let Two([a2, b2]) = total.get();
            assert!(a2 >= a1 && b2 >= b1, "a tally went back");
            for (a, b) in [(a1, b1), (a2, b2)] {
                assert!(
                    matches!(a, 0 | 1 | 3) && matches!(b, 0 | 10 | 30),
                    "a tally is not a prefix total: ({a}, {b})"
                );
            }
            if closed {
                assert_eq!((a1, b1), (3, 30), "a closed counter read short");
            }
        });
        writer.add(Two([1, 10]));
        writer.add(Two([2, 20]));
        drop(writer);
        observer.join().unwrap();
    });
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
fn a_reopened_two_tally_writer_sees_every_tally_of_the_dropped_one() {
    loom::model(|| {
        let (mut writer, total) = counter::<Two>();
        let reopener = total.clone();
        let handle = thread::spawn(move || {
            if let Some(mut next) = reopener.reopen() {
                assert_eq!(
                    reopener.get(),
                    Two([3, 30]),
                    "a reopened counter read short"
                );
                next.add(Two([1, 10]));
                assert_eq!(reopener.get(), Two([4, 40]));
                true
            } else {
                false
            }
        });
        writer.add(Two([1, 10]));
        writer.add(Two([2, 20]));
        drop(writer);
        let won = handle.join().unwrap();
        if !won {
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

#[test]
fn a_two_writer_tally_never_goes_back_and_is_exact_once_joined() {
    loom::model(|| {
        let total = tally::<u64>();
        let (a, b) = (total.writer(), total.writer());
        let observer = {
            let total = total.clone();
            thread::spawn(move || {
                let first = total.get();
                let second = total.get();
                assert!(second >= first, "a tally went back");
                assert!(second <= 2, "a tally is not a sum of adds");
            })
        };
        let first = thread::spawn(move || a.add(1));
        let second = thread::spawn(move || b.add(1));
        first.join().unwrap();
        second.join().unwrap();
        observer.join().unwrap();
        assert_eq!(total.get(), 2, "an add was lost");
    });
}

#[test]
fn a_two_word_tally_reads_prefix_totals_and_is_exact_once_joined() {
    loom::model(|| {
        let total = tally::<Two>();
        let (a, b) = (total.writer(), total.writer());
        let observer = {
            let total = total.clone();
            thread::spawn(move || {
                let Two([x1, y1]) = total.get();
                let Two([x2, y2]) = total.get();
                assert!(x2 >= x1 && y2 >= y1, "a tally went back");
                for (x, y) in [(x1, y1), (x2, y2)] {
                    assert!(
                        matches!(x, 0..=2) && matches!(y, 0 | 10 | 20),
                        "a tally is not a sum of adds: ({x}, {y})"
                    );
                }
            })
        };
        let first = thread::spawn(move || a.add(Two([1, 10])));
        let second = thread::spawn(move || b.add(Two([1, 10])));
        first.join().unwrap();
        second.join().unwrap();
        observer.join().unwrap();
        assert_eq!(total.get(), Two([2, 20]), "an add was lost");
    });
}
