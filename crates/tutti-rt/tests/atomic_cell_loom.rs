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
