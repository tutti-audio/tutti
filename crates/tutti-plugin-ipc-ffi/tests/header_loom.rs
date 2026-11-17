//! Loom models of the shipped [`SlabHeader`] protocols: the block ring's
//! Release/Acquire pair and the setup magic's. The header's atomics are loom's
//! under `--cfg loom`, so these run `publish`, `sequence`, `initialize` and
//! `validate` themselves.
//!
//! ```text
//! cargo test -p tutti-plugin-ipc-ffi --release --test header_loom   (with the loom cfg flag set)
//! ```

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering};
use loom::thread;
use tutti_plugin_ipc_ffi::{Direction, RING_SLOTS, SlabHeader, slot_for};

/// A header and the words standing for the samples it guards: one per slot,
/// written `Relaxed` before the publish and read `Relaxed` after the sequence
/// check, so all the ordering is the header's.
struct Region {
    header: SlabHeader,
    data: [AtomicU64; RING_SLOTS],
}

impl Region {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            header: SlabHeader::zeroed(),
            data: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }

    fn publish(&self, direction: Direction, seq: u64) {
        let slot = slot_for(seq);
        self.data[slot].store(seq, Ordering::Relaxed);
        self.header.publish(direction, slot, seq);
    }

    /// The slot's data when its sequence is `want`, `None` when it is not yet.
    fn read(&self, direction: Direction, want: u64) -> Option<u64> {
        let slot = slot_for(want);
        (self.header.sequence(direction, slot) == want)
            .then(|| self.data[slot].load(Ordering::Relaxed))
    }
}

/// A reader that observes a published sequence number observes the samples
/// published before it, in both directions.
#[test]
fn a_matched_sequence_implies_its_data_is_visible() {
    for direction in [Direction::Input, Direction::Output] {
        loom::model(move || {
            let region = Region::new();
            let writer = {
                let region = Arc::clone(&region);
                thread::spawn(move || region.publish(direction, 1))
            };
            if let Some(data) = region.read(direction, 1) {
                assert_eq!(data, 1, "the sequence was visible without its samples");
            }
            writer.join().expect("writer thread");
        });
    }
}

/// Two blocks in the two slots: a reader asking for block N is never satisfied
/// by another block's samples, and a block is never seen before the previous
/// one's publish landed in its own slot.
#[test]
fn a_slot_reused_by_a_later_block_is_not_mistaken_for_an_earlier_one() {
    loom::model(|| {
        let region = Region::new();
        let writer = {
            let region = Arc::clone(&region);
            thread::spawn(move || {
                region.publish(Direction::Output, 1);
                region.publish(Direction::Output, 2);
            })
        };
        for want in 1..=2u64 {
            if let Some(data) = region.read(Direction::Output, want) {
                assert_eq!(
                    data, want,
                    "a read of block {want} got block {data}'s samples"
                );
            }
        }
        writer.join().expect("writer thread");
    });
}

/// A peer that acquires the magic sees the rest of the header: `validate` on
/// a header being initialized fails only on the magic, never on the version or
/// the slot count that `initialize` wrote before it.
#[test]
fn a_validated_magic_implies_the_rest_of_the_header_is_visible() {
    loom::model(|| {
        let header = Arc::new(SlabHeader::zeroed());
        let creator = {
            let header = Arc::clone(&header);
            thread::spawn(move || header.initialize())
        };
        if let Err(error) = header.validate() {
            let error = error.to_string();
            assert!(
                error.contains("not a tutti audio slab"),
                "the magic was visible without the header it publishes: {error}"
            );
        }
        creator.join().expect("creator thread");
        header.validate().expect("an initialized header validates");
    });
}
