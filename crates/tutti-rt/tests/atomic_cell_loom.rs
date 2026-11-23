//! A `loom` model of [`AtomicCell`]: the shipped code, whose atomic is loom's
//! under the flag (see `src/atomic_cell.rs`'s `sync` module). Run it with
//! `cargo xtask check-loom` (its recipe sets the loom cfg flag).
//!
//! One writer sets two modes in turn, after writing a payload each; one reader
//! gets twice. It asserts, over every interleaving:
//!
//! - **a whole value**: every `get` is the initial mode or one that was set;
//! - **no going back**: a later `get` never sees an older mode than an earlier
//!   one (the word is one location, so coherence holds);
//! - **the `Release`/`Acquire` pair**: a reader that sees a mode also sees the
//!   payload written before that `set`.
//!
//! A second model holds a pair of `u32`s and asserts **no torn pair** (a load
//! returns the pair of the initial value or of one store, never one store's
//! first with another's second) and **no going back**. A third has a taker
//! `swap` in "none" while a raiser raises two requests in order: the last
//! request is taken exactly once (a request stored between a taker's read and
//! its write would be lost) and none is taken twice.

#![cfg(loom)]

use std::sync::Arc;

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use tutti_rt::{AtomicCell, atomic_word};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum Mode {
    Off,
    Preroll,
    Always,
}
atomic_word!(Mode {
    Off,
    Preroll,
    Always
});

const HISTORY: [Mode; 3] = [Mode::Off, Mode::Preroll, Mode::Always];

#[test]
fn a_reader_sees_whole_ordered_modes_and_the_payload_before_each() {
    loom::model(|| {
        let cell = Arc::new(AtomicCell::new(Mode::Off));
        let payload = Arc::new(AtomicUsize::new(0));
        let writer = {
            let (cell, payload) = (Arc::clone(&cell), Arc::clone(&payload));
            thread::spawn(move || {
                for (n, mode) in HISTORY.iter().enumerate().skip(1) {
                    payload.store(n, Ordering::Relaxed);
                    cell.store(*mode);
                }
            })
        };
        let mut seen = 0;
        for _ in 0..2 {
            let mode = cell.load();
            let at = HISTORY.iter().position(|m| *m == mode).expect("a mode");
            assert!(at >= seen, "a get went back from {seen} to {at}");
            assert!(
                payload.load(Ordering::Relaxed) >= at,
                "mode {at} seen without the payload written before its set"
            );
            seen = at;
        }
        writer.join().unwrap();
        assert_eq!(cell.load(), Mode::Always);
    });
}

const PAIRS: [[u32; 2]; 2] = [[1, 2], [3, 4]];
const INITIAL_PAIR: [u32; 2] = [100, 200];

/// The index of `pair` in the history (0: the initial value), or a panic.
fn which(pair: [u32; 2]) -> usize {
    std::iter::once(INITIAL_PAIR)
        .chain(PAIRS)
        .position(|p| p == pair)
        .unwrap_or_else(|| panic!("a torn pair: {pair:?}"))
}

#[test]
fn a_reader_never_sees_a_torn_or_older_pair() {
    loom::model(|| {
        let cell = Arc::new(AtomicCell::new(INITIAL_PAIR));
        let writer = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || {
                for pair in PAIRS {
                    cell.store(pair);
                }
            })
        };
        let first = which(cell.load());
        let second = which(cell.load());
        assert!(second >= first, "a load went back from {first} to {second}");
        writer.join().unwrap();
        assert_eq!(cell.load(), PAIRS[1]);
    });
}

#[test]
fn a_swapping_taker_loses_no_request() {
    const NONE: [u32; 2] = [0, 0];
    const FIRST: [u32; 2] = [1, 1];
    const LAST: [u32; 2] = [2, 2];
    loom::model(|| {
        let cell = Arc::new(AtomicCell::new(NONE));
        let raiser = {
            let cell = Arc::clone(&cell);
            // The raiser swaps rather than stores: under loom 0.7.3 a raiser
            // of `store(FIRST); store(LAST)` makes this model take
            // `[NONE, NONE, NONE]`, which coherence forbids (each swap reads
            // the value just before it in modification order, and both stores
            // precede the join, so the last swap or an earlier one reads
            // LAST). That is loom's store-vs-RMW modelling, not the cell; swap
            // the two lines back to stores to recheck after a loom bump. The
            // taker, the side under test, is the shipped `swap` either way.
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
