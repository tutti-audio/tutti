//! A `loom` model of [`PackedAtomic`]: the shipped code, whose atomic is
//! loom's under the flag (see `src/packed_atomic.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test packed_atomic_loom
//! ```
//!
//! The value is a range, `[min, max]` as two `f32`s. It asserts, over every
//! interleaving of one writer and one reader:
//!
//! - **no torn pair**: every load returns the `(min, max)` of the initial
//!   value or of one store, never one store's `min` with another's `max`;
//! - **no going back**: two loads in program order never see an older store
//!   after a newer one (the word is a single location, so coherence holds).
//!
//! And, for `swap` as a taker: a raiser raises two requests in order while a
//! taker swaps in "none"; the last request is taken exactly once (a request
//! stored between a taker's read and its write would be lost) and none is
//! taken twice.

#![cfg(loom)]

use std::sync::Arc;

use loom::thread;
use tutti_rt::PackedAtomic;

const STORES: [[f32; 2]; 2] = [[1.0, 2.0], [3.0, 4.0]];
const INITIAL: [f32; 2] = [-1.0, -2.0];

/// The index of `pair` in the history (0: the initial value), or a panic.
fn which(pair: [f32; 2]) -> usize {
    std::iter::once(INITIAL)
        .chain(STORES)
        .position(|p| p == pair)
        .unwrap_or_else(|| panic!("a torn range: {pair:?}"))
}

#[test]
fn a_reader_never_sees_a_torn_or_older_range() {
    loom::model(|| {
        let cell = Arc::new(PackedAtomic::new(INITIAL));
        let writer = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || {
                for range in STORES {
                    cell.store(range);
                }
            })
        };
        let first = which(cell.load());
        let second = which(cell.load());
        assert!(second >= first, "a load went back from {first} to {second}");
        writer.join().unwrap();
        assert_eq!(cell.load(), STORES[1]);
    });
}

#[test]
fn a_swapping_taker_loses_no_request() {
    const NONE: [u32; 2] = [0, 0];
    const FIRST: [u32; 2] = [1, 1];
    const LAST: [u32; 2] = [2, 2];
    loom::model(|| {
        let cell = Arc::new(PackedAtomic::new(NONE));
        let raiser = {
            let cell = Arc::clone(&cell);
            // The raiser swaps rather than stores: loom 0.7.3 lets a plain
            // store racing a read-modify-write produce an outcome coherence
            // forbids (the RMW reads the old value and the store is lost
            // even after a join), which would fail this model with no fault
            // in the code. The taker, the side under test, is the shipped
            // `swap` either way.
            thread::spawn(move || {
                let _ = cell.swap(FIRST);
                let _ = cell.swap(LAST);
            })
        };
        let mut taken = vec![cell.swap(NONE), cell.swap(NONE)];
        raiser.join().unwrap();
        taken.push(cell.swap(NONE));
        let count = |req| taken.iter().filter(|&&v| v == req).count();
        assert_eq!(
            count(LAST),
            1,
            "the last request is taken exactly once: {taken:?}"
        );
        assert!(count(FIRST) <= 1, "a request taken twice: {taken:?}");
    });
}
