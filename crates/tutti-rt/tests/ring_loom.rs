//! `loom` models of [`ring`]: the shipped code, whose atomics and slot cells
//! are loom's under the flag (see `src/ring/`).
//!
//! ```text
//! LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test ring_loom
//! ```
//!
//! The rings hold `[u32; 3]`, each value's elements equal, so a torn slot
//! shows as unequal elements. Over every interleaving within the preemption
//! bound: 3, from the recipe's `LOOM_MAX_PREEMPTIONS`. Every model is two
//! threads and a handful of operations, whose races need at most two
//! preemptions between a store and the load that pairs with it; a third is
//! headroom, and each further one multiplies the executions several-fold for
//! no new interleaving of those few operations.
//!
//! - **no accepted value is lost, none repeated, order kept** (`DropNewest`,
//!   `Refuse`, with `push`/`pop` and with `push_slice`/`pop_slice`): the
//!   values popped equal the values accepted, in order, and the loss count
//!   equals the refusals;
//! - **`push_iter` and `pop_each`** move runs whole, in order, with one
//!   publish: what the reader hands out is what the writer took;
//! - **`peek` and `iter` against a concurrent push** see only published
//!   values, in order, and never a torn slot;
//! - **`len` and `drain` while the writer pushes**: a drain takes consecutive
//!   values from where the last stopped, and `len` never exceeds what was
//!   pushed (the `head` ordering);
//! - **a reader dropped mid-push**: every value is freed exactly once (queued
//!   at the drop, or handed back by a refused push), and a push after
//!   `Disconnected` never succeeds (the `reader_gone` ordering);
//! - **the loss count under a race**: the reader's cumulative `lost` never
//!   goes down, counts no loss not yet made, and ends at the writer's;
//! - **disconnection loses nothing**: a reader that saw "writer gone" before
//!   its pop has seen every value pushed;
//! - **no torn slot, no overlapping access**: the slots are loom
//!   `UnsafeCell`s, so a read not ordered after its write, or a write that
//!   races a taker still reading, is a causality violation.

#![cfg(loom)]

use loom::thread;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_rt::{Overflow, PushError, RingReader, ring};

/// A value that counts its own drops, so a leak or a double free is a count.
struct Tracked(Arc<AtomicUsize>);

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn checked(v: [u32; 3]) -> u32 {
    let [a, b, c] = v;
    assert!(a == b && b == c, "a torn slot: {v:?}");
    a
}

fn take(reader: &mut RingReader<[u32; 3]>, popped: &mut Vec<u32>) -> bool {
    match reader.pop() {
        Some(v) => {
            popped.push(checked(v));
            true
        }
        None => false,
    }
}

fn accepted_run(capacity: usize, overflow: Overflow) {
    loom::model(move || {
        let (mut writer, mut reader) = ring::<[u32; 3]>(capacity, overflow);
        let producer = thread::spawn(move || {
            let mut accepted = Vec::new();
            for n in 1..=3u32 {
                if writer.push([n; 3]).is_ok() {
                    accepted.push(n);
                }
            }
            (accepted, writer.lost())
        });
        let mut popped = Vec::new();
        for _ in 0..3 {
            if !take(&mut reader, &mut popped) {
                thread::yield_now();
            }
        }
        let (accepted, lost) = producer.join().unwrap();
        while take(&mut reader, &mut popped) {}
        assert_eq!(popped, accepted, "popped values differ from accepted");
        let counted = if overflow == Overflow::Refuse {
            0
        } else {
            3 - accepted.len() as u64
        };
        assert_eq!(lost, counted, "refusals miscounted");
    });
}

#[test]
fn every_accepted_value_arrives_once_in_order_when_full_drops_the_newest() {
    accepted_run(2, Overflow::DropNewest);
}

#[test]
fn a_single_slot_is_reused_in_order() {
    accepted_run(1, Overflow::Refuse);
}

#[test]
fn runs_move_whole_and_in_order() {
    loom::model(|| {
        let (mut writer, mut reader) = ring::<u32>(3, Overflow::DropNewest);
        let producer = thread::spawn(move || {
            let mut sent = Vec::new();
            let mut next = 1;
            for _ in 0..2 {
                let run = [next, next + 1];
                let took = writer.push_slice(&run);
                sent.extend_from_slice(&run[..took]);
                next += 2;
            }
            (sent, writer.lost())
        });
        let mut got = Vec::new();
        let mut out = [0u32; 2];
        for _ in 0..2 {
            let n = reader.pop_slice(&mut out);
            got.extend_from_slice(&out[..n]);
            if n == 0 {
                thread::yield_now();
            }
        }
        let (sent, lost) = producer.join().unwrap();
        loop {
            let n = reader.pop_slice(&mut out);
            got.extend_from_slice(&out[..n]);
            if n == 0 {
                break;
            }
        }
        assert_eq!(got, sent, "a run was torn, lost or reordered");
        assert_eq!(
            lost,
            4 - sent.len() as u64,
            "a refused value is counted once"
        );
    });
}

#[test]
fn len_and_drain_see_only_published_values() {
    loom::model(|| {
        let (mut writer, mut reader) = ring::<[u32; 3]>(3, Overflow::Refuse);
        let producer = thread::spawn(move || {
            for n in 1..=3u32 {
                assert!(writer.push([n; 3]).is_ok());
            }
        });
        let mut expect = 1;
        for _ in 0..2 {
            assert!(reader.len() <= 3, "len counts a value not pushed yet");
            for n in reader.drain().map(checked) {
                assert_eq!(n, expect, "drain skipped or repeated a value");
                expect += 1;
            }
            thread::yield_now();
        }
        producer.join().unwrap();
        for n in reader.drain().map(checked) {
            assert_eq!(n, expect);
            expect += 1;
        }
        assert_eq!(expect, 4, "a pushed value never drained");
    });
}

#[test]
fn a_reader_dropped_mid_push_frees_every_value_once() {
    loom::model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let (mut writer, mut reader) = ring::<Tracked>(2, Overflow::DropNewest);
        let values: Vec<Tracked> = (0..3).map(|_| Tracked(Arc::clone(&drops))).collect();
        let producer = thread::spawn(move || {
            let mut disconnected = false;
            for value in values {
                match writer.push(value) {
                    Ok(()) => assert!(!disconnected, "a push after Disconnected succeeded"),
                    Err(refused) => disconnected |= refused.error == PushError::Disconnected,
                }
            }
            // Handed back: a loom thread's captures are not dropped inside the
            // model, so the model drops the last end itself.
            writer
        });
        let first = reader.pop();
        drop(reader);
        drop(producer.join().unwrap());
        drop(first);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            3,
            "a value leaked or freed twice"
        );
    });
}

#[test]
fn the_loss_count_survives_a_race() {
    loom::model(|| {
        let (mut writer, mut reader) = ring::<u32>(1, Overflow::DropNewest);
        let view = writer.losses();
        let producer = thread::spawn(move || {
            for n in 0..3 {
                let _ = writer.push(n);
            }
            writer.lost()
        });
        let first = reader.lost();
        let _ = reader.pop();
        let second = reader.lost();
        let total = producer.join().unwrap();
        assert!(first <= second, "the cumulative count went down");
        assert!(second <= total, "a read counted a loss not yet made");
        assert_eq!(reader.lost(), total, "a loss was dropped or repeated");
        assert_eq!(view.get(), total);
        assert_eq!(reader.lost(), total);
    });
}

#[test]
fn push_iter_and_pop_each_move_runs_whole_and_in_order() {
    loom::model(|| {
        let (mut writer, mut reader) = ring::<u32>(3, Overflow::Refuse);
        let producer = thread::spawn(move || {
            let mut sent = Vec::new();
            let mut next = 1;
            for _ in 0..2 {
                let run = [next, next + 1];
                let took = writer.push_iter(run);
                sent.extend_from_slice(&run[..took]);
                next += 2;
            }
            sent
        });
        let mut got = Vec::new();
        for _ in 0..2 {
            if reader.pop_each(2, |n| got.push(n)) == 0 {
                thread::yield_now();
            }
        }
        let sent = producer.join().unwrap();
        while reader.pop_each(2, |n| got.push(n)) > 0 {}
        assert_eq!(got, sent, "a run was torn, lost or reordered");
    });
}

#[test]
fn peek_and_iter_see_only_published_values() {
    loom::model(|| {
        let (mut writer, mut reader) = ring::<[u32; 3]>(3, Overflow::Refuse);
        let producer = thread::spawn(move || {
            for n in 1..=3u32 {
                assert!(writer.push([n; 3]).is_ok());
            }
        });
        let mut expect = 1;
        for _ in 0..2 {
            let seen: Vec<u32> = reader.iter().map(|v| checked(*v)).collect();
            assert!(
                seen.iter().zip(expect..).all(|(a, b)| *a == b),
                "iter skipped or reordered: {seen:?} from {expect}"
            );
            if let Some(first) = reader.peek() {
                assert_eq!(checked(*first), expect, "peek is not the oldest");
            }
            while let Some(v) = reader.pop() {
                assert_eq!(checked(v), expect);
                expect += 1;
            }
            thread::yield_now();
        }
        producer.join().unwrap();
        while let Some(v) = reader.pop() {
            assert_eq!(checked(v), expect);
            expect += 1;
        }
        assert_eq!(expect, 4);
    });
}

#[test]
fn a_reader_that_saw_the_writer_gone_has_seen_every_push() {
    loom::model(|| {
        let (mut writer, mut reader) = ring::<[u32; 3]>(2, Overflow::Refuse);
        let producer = thread::spawn(move || {
            assert!(writer.push([1; 3]).is_ok());
            assert!(writer.push([2; 3]).is_ok());
        });
        let mut popped = Vec::new();
        loop {
            let gone = reader.is_disconnected();
            if !take(&mut reader, &mut popped) {
                if gone {
                    break;
                }
                thread::yield_now();
            }
        }
        producer.join().unwrap();
        assert_eq!(popped, [1, 2]);
    });
}
