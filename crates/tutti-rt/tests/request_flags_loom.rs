//! A `loom` model of [`AtomicRequestFlags`]: the shipped code, whose atomics are
//! loom's under the flag (see `src/request_flags.rs`'s `sync` module).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test request_flags_loom
//! ```
//!
//! Two threads raise one flag each while the main thread takes twice. Each
//! raiser also writes a plain-looking datum (a `Release` store to a second
//! atomic) before raising. Over every interleaving it asserts:
//!
//! - **no flag lost**: after both raisers join and a final take, every raised
//!   flag was returned by some take;
//! - **each flag taken once**: no flag appears in two takes, because a raise
//!   happens once and a take clears;
//! - **raise orders what came before it**: a taker that sees a flag also sees
//!   the datum its raiser wrote before raising;
//! - **an unraised set is never taken** (the second test).

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::thread;
use tutti_rt::{AtomicRequestFlags, RequestFlag};

const A: RequestFlag = RequestFlag::of(0);
const B: RequestFlag = RequestFlag::of(5);

#[test]
fn raises_from_two_threads_are_all_taken_once() {
    loom::model(|| {
        let flags = Arc::new(AtomicRequestFlags::new());
        let data = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let raisers: Vec<_> = [(A, 0), (B, 1)]
            .into_iter()
            .map(|(flag, slot)| {
                let flags = flags.clone();
                let data = data.clone();
                thread::spawn(move || {
                    data[slot].store(7, Ordering::Relaxed);
                    flags.raise(flag);
                })
            })
            .collect();

        let mut seen = [0u8; 2];
        for _ in 0..2 {
            let taken = flags.take();
            for (n, (flag, slot)) in [(A, 0), (B, 1)].into_iter().enumerate() {
                if taken.contains(flag) {
                    seen[n] += 1;
                    assert_eq!(
                        data[slot].load(Ordering::Relaxed),
                        7,
                        "saw the flag without the datum"
                    );
                }
            }
        }
        for raiser in raisers {
            raiser.join().unwrap();
        }
        let last = flags.take();
        for (n, flag) in [A, B].into_iter().enumerate() {
            if last.contains(flag) {
                seen[n] += 1;
            }
        }
        assert_eq!(seen, [1, 1], "a flag lost, or taken twice");
    });
}

/// A set nobody raised is never taken as non-empty, whatever the other
/// thread's takes do.
#[test]
fn an_unraised_word_is_never_taken() {
    loom::model(|| {
        let flags = Arc::new(AtomicRequestFlags::new());
        let other = flags.clone();
        let t = thread::spawn(move || assert!(other.take().is_empty()));
        assert!(flags.take().is_empty());
        t.join().unwrap();
    });
}
