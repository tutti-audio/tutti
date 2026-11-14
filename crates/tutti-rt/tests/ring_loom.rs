//! A `loom` model of [`Ring`]: the shipped code, whose atomics and slot cells
//! are loom's under the flag (see `src/ring.rs`).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test ring_loom
//! ```
//!
//! The ring holds `[u32; 3]`, each value's elements equal, so a torn slot
//! shows as unequal elements. The producer pushes 1, 2, 3 and the consumer
//! pops concurrently, then drains after the producer joins. Over every
//! interleaving:
//!
//! - **no accepted value is lost, none repeated, order kept** (`DropNewest`,
//!   `Refuse`): the values popped equal the values whose push returned `Ok`;
//! - **every value is popped or counted** (`Evict`, where the writer takes
//!   from the reader's end too): the values popped are in increasing order
//!   and `popped + lost == pushed`, so a value the writer evicted while the
//!   reader claimed the next one is neither duplicated nor dropped twice;
//! - **no torn slot, no overlapping access**: the slots are loom
//!   `UnsafeCell`s, so a read not ordered after its write, or a write that
//!   races a taker still reading, is a causality violation;
//! - **disconnection loses nothing**: a reader that asked "is the writer
//!   gone" before its pop and found nothing has seen every value pushed.

#![cfg(loom)]

use loom::thread;
use tutti_rt::{Overflow, Ring, RingReader};

fn take(reader: &mut RingReader<[u32; 3]>, popped: &mut Vec<u32>) -> bool {
    match reader.pop() {
        Some([a, b, c]) => {
            assert!(a == b && b == c, "a torn slot: {:?}", [a, b, c]);
            popped.push(a);
            true
        }
        None => false,
    }
}

fn accepted_run(capacity: usize, overflow: Overflow) {
    loom::model(move || {
        let (mut writer, mut reader) = Ring::<[u32; 3]>::pair(capacity, overflow);
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
fn an_evicting_ring_pops_or_counts_every_value() {
    for capacity in [1, 2] {
        loom::model(move || {
            let (mut writer, mut reader) = Ring::<[u32; 3]>::pair(capacity, Overflow::Evict);
            let producer = thread::spawn(move || {
                for n in 1..=3u32 {
                    let _ = writer.push([n; 3]);
                }
                writer.lost()
            });
            let mut popped = Vec::new();
            for _ in 0..2 {
                if !take(&mut reader, &mut popped) {
                    thread::yield_now();
                }
            }
            let lost = producer.join().unwrap();
            while take(&mut reader, &mut popped) {}
            assert!(
                popped.windows(2).all(|w| w[0] < w[1]),
                "order or repeat: {popped:?}"
            );
            assert_eq!(
                popped.len() as u64 + lost,
                3,
                "a value neither popped nor counted"
            );
        });
    }
}

#[test]
fn a_reader_that_saw_the_writer_gone_has_seen_every_push() {
    loom::model(|| {
        let (mut writer, mut reader) = Ring::<[u32; 3]>::pair(2, Overflow::Refuse);
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
