//! A `loom` model of [`SeqSlotsWriter`] and [`SeqSlotsReader`]: the shipped code, whose atomics
//! and fences are loom's under the flag (see `src/seq_slots.rs`'s `sync` module, and
//! `src/seq_cell.rs`'s for the cells).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test seq_slots_loom
//! ```
//!
//! The writer writes slot 0 (`1`), ends the epoch, does the work a caller
//! does after `new_epoch` (a relaxed store to `loaded`, standing for a
//! plugin's `set_state`), then writes slot 0 again (`2`). The reader drains
//! once, then again after the writer is done. Over every interleaving it
//! asserts:
//!
//! - **no stale value after an observed epoch**: a reader that sees `loaded`
//!   (so the work after `new_epoch`) never yields `1`. Nothing but the fence
//!   that ends `new_epoch` orders the epoch store before that relaxed store,
//!   so the model fails with it removed. The reader's fence is the other half
//!   of the pair; loom does not reorder loads, so the model cannot fail on its
//!   removal;
//! - **no dropped post-epoch write**: the values yielded end with `2`, once;
//! - **an unchanged generation skips the sweep**: a drain after that yields
//!   nothing.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread;
use tutti_rt::{Generation, generation_watch, seq_slots};

#[test]
fn a_value_before_an_observed_epoch_is_never_yielded_and_a_later_write_never_dropped() {
    loom::model(|| {
        let (mut writer, mut reader) = seq_slots::<u32>(1);
        let loaded = Arc::new(AtomicBool::new(false));
        let writer_loaded = Arc::clone(&loaded);
        let handle = thread::spawn(move || {
            writer.write(0, 1);
            writer.new_epoch();
            writer_loaded.store(true, Ordering::Relaxed);
            writer.write(0, 2);
        });

        let mut yielded = Vec::new();
        let observed = loaded.load(Ordering::Relaxed);
        reader.drain_in_epoch(|_, value, _| yielded.push(value));
        if observed {
            assert!(
                !yielded.contains(&1),
                "a stale value after the epoch: {yielded:?}"
            );
        }
        // The writer makes two writes: a sweep that claims the second has
        // yielded it.
        if reader.swept().has_reached(Generation::new(2)) {
            assert!(
                yielded.contains(&2),
                "swept past a write it did not yield: {yielded:?}"
            );
        }
        handle.join().unwrap();

        reader.drain_in_epoch(|_, value, _| yielded.push(value));
        assert_eq!(reader.swept(), Generation::new(2), "a quiet drain");
        assert_eq!(
            yielded.last(),
            Some(&2),
            "the post-epoch write: {yielded:?}"
        );
        assert_eq!(yielded.iter().filter(|&&v| v == 2).count(), 1);

        let mut after = 0;
        reader.drain_in_epoch(|_, _, _| after += 1);
        assert_eq!(after, 0, "nothing was written since");
    });
}

/// **A watch never shows a sweep whose values were not applied.** The reader
/// applies a drained value into `applied` (a plugin's `process`, relaxed), then
/// publishes its sweep; an observer that sees the watch reach the write sees
/// the value applied.
#[test]
fn a_watch_that_reached_a_write_shows_it_applied() {
    loom::model(|| {
        let (mut writer, mut reader) = seq_slots::<u32>(1);
        let (mut publisher, watch) = generation_watch();
        let applied = Arc::new(loom::sync::atomic::AtomicU32::new(0));
        writer.write(0, 7);
        let reader_applied = Arc::clone(&applied);
        let drainer = thread::spawn(move || {
            reader.drain_in_epoch(|_, value, _| reader_applied.store(value, Ordering::Relaxed));
            publisher.publish(reader.swept());
        });
        if watch.get().has_reached(Generation::new(1)) {
            assert_eq!(applied.load(Ordering::Relaxed), 7, "swept, not applied");
        }
        drainer.join().unwrap();
    });
}
