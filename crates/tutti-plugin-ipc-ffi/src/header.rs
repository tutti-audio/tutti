//! The slab's synchronization header: what makes a read *meaningful*.
//!
//! The audio regions below this header carry no evidence of who wrote them: a
//! freshly-mapped region of zeros and one the peer just filled are byte-identical
//! to the reader. The evidence goes *in the shared region*, beside the data it
//! describes: a per-slot sequence number the writer stamps after the last sample
//! and the reader checks before the first. That Release/Acquire pair is what
//! justifies [`SharedMapping`]'s `unsafe impl Sync`.
//!
//! [`SharedMapping`]: crate::SharedMapping
//!
//! # The ordering
//!
//! A slot is a seqlock. The writer [`begin`](SlabHeader::begin)s it (stores
//! `BUSY`, then a [`Release`](Ordering::Release) fence), `memcpy`s every
//! channel, then [`publish`](SlabHeader::publish)es `seq` with
//! [`Release`](Ordering::Release). The reader loads `seq` with
//! [`Acquire`](Ordering::Acquire), `memcpy`s out only if it matches, then
//! [`recheck`](SlabHeader::recheck)s: an [`Acquire`](Ordering::Acquire) fence
//! and a second load, which must still name `seq`. Without the busy mark a
//! reader that started on block N accepts half of N and half of N+2 when the
//! writer reuses the slot; without the fence the copy's loads may be satisfied
//! after the recheck on a weakly ordered CPU. The header's atomics are `loom`'s
//! under `--cfg loom`, and the `loom_model` module below runs `begin`,
//! `publish`, `sequence`, `recheck`, `initialize` and `validate` under every
//! interleaving and reordering the memory model permits between threads; it
//! does not cover a reader in another process or a creator that crashed
//! mid-initialize.
//!
//! # Sequence numbers, not a "newest" index
//!
//! Per-slot, because the reader wants a *specific* block, the one it submitted a
//! fixed number of blocks ago, not the newest. A single newest-index would let a
//! skipped block hand back N-2 as though it were N-1: wrong audio, not silence.
//! Sequences start at **1**: a fresh slab is zeroed, so 0 means "nothing was ever
//! published here".

use crate::SlabError;
use std::mem::{align_of, offset_of, size_of};
use std::ops::{Deref, DerefMut};

/// A value on its own cache line: aligned to 128 bytes, the larger of the line
/// sizes the targets use (64 on x86-64, whose adjacent-line prefetcher pulls
/// pairs, and 128 on aarch64). The alignment pads the size to a multiple of it.
#[derive(Debug)]
#[repr(C, align(128))]
struct CachePadded<T>(T);

impl<T> Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// The header's atomics: `std`'s, and `loom`'s under `--cfg loom`, so the model
/// in `loom_model` runs this code. The mapped layout is the `std` one.
mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
}

use sync::{AtomicU32, AtomicU64, Ordering, fence};

/// Identifies a tutti audio slab. Checked by the opening side before it trusts
/// a single byte of the mapping.
pub(crate) const SLAB_MAGIC: u64 = u64::from_le_bytes(*b"TTI_SLAB");

/// Bumped when this header's layout or the meaning of a word in it changes. Distinct from the wire
/// `PROTOCOL_VERSION`: that one gates the control-socket message shapes, this
/// one gates the bytes at offset 0 of the mapping. Both sides are the same
/// binary in practice, so a mismatch means something is badly wrong — but the
/// check is nearly free and the failure it prevents (mapping a wrong-shaped
/// region and reading it as audio) is not.
pub(crate) const HEADER_VERSION: u32 = 2;

/// The sequence a slot carries while its writer is rewriting it. Real
/// sequences start at 1 and never reach it, so no reader asks for it.
pub(crate) const BUSY: u64 = u64::MAX;

/// The number of blocks the shared-memory ring holds.
///
/// Two slots is the minimum that lets one block be published while another is
/// read, which is all the pipelined protocol needs: the bridge pumps serially,
/// so at most one block is ever in flight.
///
/// Fixed rather than negotiated because the reasoning above is not a tunable —
/// raising it is only meaningful alongside a bridge that pipelines its own
/// socket traffic, and that change would have to revisit `MAX_BEHIND` in
/// `dispatch.rs` at the same time (they are one constraint; see its docs).
pub const RING_SLOTS: usize = 2;

/// Setup-time identification. Separated from the sequence arrays so it shares no
/// cache line with them: it is written once and then read never on the hot path,
/// whereas the arrays are stored to every block by both processes.
#[derive(Debug)]
#[repr(C)]
struct Control {
    magic: AtomicU64,
    header_version: AtomicU32,
    slots: AtomicU32,
}

/// The fixed-size prologue at offset 0 of every slab mapping.
///
/// `repr(C)` is load-bearing, not stylistic. Two independently-compiled
/// mappings of the same bytes must compute identical field offsets, and Rust's
/// default layout is explicitly unspecified — a compiler upgrade is permitted to
/// reorder fields, which would silently point one process's `output_seq` at the
/// other's `input_seq`. The `const` assertions below pin the properties that
/// matter; `repr(C)` is what makes them meaningful across builds.
#[derive(Debug)]
#[repr(C)]
pub struct SlabHeader {
    control: CachePadded<Control>,
    /// Written by the host, read by the server.
    input_seq: CachePadded<[AtomicU64; RING_SLOTS]>,
    /// Written by the server, read by the host.
    output_seq: CachePadded<[AtomicU64; RING_SLOTS]>,
}

// Each writer's sequence array must sit on its own cache line.
//
// This is a *performance* property, not a correctness one — false sharing makes
// the atomics slower, never wrong; correctness rests entirely on the
// Release/Acquire pair. But the cost is real and lands in the worst possible
// place: the host stores `input_seq` and the server stores `output_seq` every
// block, every block, from different processes on different cores.
// Sharing a line would ping-pong it between caches on the audio thread's
// critical path, paying the full price of contention over data that is not
// actually shared.
//
// `CachePadded` aligns to 128, not 64: the line size is 64 on x86-64 — but its
// L2 prefetcher pulls line *pairs*, so the effective granularity is 128 — and
// 128 on aarch64. A hardcoded 64 would be mistuned on both.
//
// The assertions check *properties* (each region is line-aligned) rather than an
// absolute `size_of`: they state the invariant instead of a number that happens
// to satisfy it today.
const _: () = assert!(align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>() >= 64);
const _: () = assert!(
    offset_of!(SlabHeader, input_seq) % align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>() == 0
);
const _: () = assert!(
    offset_of!(SlabHeader, output_seq) % align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>() == 0
);
// The two arrays must not land on the same line even if the padding were
// misconfigured — this is the invariant the whole block above exists to secure.
const _: () = assert!(
    offset_of!(SlabHeader, output_seq) - offset_of!(SlabHeader, input_seq)
        >= align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>()
);

/// Bytes reserved for the header at the start of every slab. The audio regions
/// begin here.
pub const SLAB_HEADER_BYTES: usize = size_of::<SlabHeader>();

/// Which direction a sequence belongs to. The two arrays are structurally
/// identical, so an untyped index would make "host writes the server's array" a
/// silent bug rather than a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Host → server. The host publishes, the server checks.
    Input,
    /// Server → host. The server publishes, the host checks.
    Output,
}

impl SlabHeader {
    /// A header of zeros in ordinary memory: the state a fresh mapping is in. A
    /// `loom` atomic is not a bit pattern, so a model builds its header here.
    #[cfg(loom)]
    pub fn zeroed() -> Self {
        Self {
            control: CachePadded(Control {
                magic: AtomicU64::new(0),
                header_version: AtomicU32::new(0),
                slots: AtomicU32::new(0),
            }),
            input_seq: CachePadded(std::array::from_fn(|_| AtomicU64::new(0))),
            output_seq: CachePadded(std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }

    /// Stamp a newly created slab. Called only by the creating side, before the
    /// peer has been told the slab exists.
    ///
    /// The magic is stored **last**, with `Release`, so it doubles as the
    /// publication point for the rest of the header: a peer that observes the
    /// magic with `Acquire` (see [`validate`](Self::validate)) is guaranteed to
    /// see the version and slot count that were written before it.
    pub(crate) fn initialize(&self) {
        self.control
            .header_version
            .store(HEADER_VERSION, Ordering::Relaxed);
        self.control
            .slots
            .store(RING_SLOTS as u32, Ordering::Relaxed);
        for seq in self.input_seq.iter().chain(self.output_seq.iter()) {
            seq.store(0, Ordering::Relaxed);
        }
        self.control.magic.store(SLAB_MAGIC, Ordering::Release);
    }

    /// Check that this mapping is a tutti slab of a recognized shape.
    ///
    /// `Acquire` on the magic pairs with the `Release` in
    /// [`initialize`](Self::initialize); the two `Relaxed` loads after it are
    /// correct precisely *because* of that pairing — the acquire already
    /// establishes happens-before with everything the creator wrote first, so
    /// re-synchronizing on each field would be redundant.
    ///
    /// # Errors
    ///
    /// [`SlabError::BadMagic`], [`SlabError::BadVersion`] or
    /// [`SlabError::BadSlots`] when the magic, the header version or the slot
    /// count is not this build's.
    pub(crate) fn validate(&self) -> Result<(), SlabError> {
        let magic = self.control.magic.load(Ordering::Acquire);
        if magic != SLAB_MAGIC {
            return Err(SlabError::BadMagic {
                found: magic,
                expected: SLAB_MAGIC,
            });
        }
        let version = self.control.header_version.load(Ordering::Relaxed);
        if version != HEADER_VERSION {
            return Err(SlabError::BadVersion {
                found: version,
                expected: HEADER_VERSION,
            });
        }
        let slots = self.control.slots.load(Ordering::Relaxed);
        if slots as usize != RING_SLOTS {
            return Err(SlabError::BadSlots {
                found: slots,
                expected: RING_SLOTS,
            });
        }
        Ok(())
    }

    fn array(&self, direction: Direction) -> &[AtomicU64; RING_SLOTS] {
        match direction {
            Direction::Input => &self.input_seq,
            Direction::Output => &self.output_seq,
        }
    }

    /// Marks `slot` as being rewritten, in `direction`: until the next
    /// [`publish`](Self::publish) no reader accepts it.
    ///
    /// Call before the first sample write of a block. A block is published once
    /// per slot, ever: republishing the same sequence into a slot is an ABA,
    /// and a reader that copied the first would accept the second's samples. The `Release` fence
    /// keeps the sample writes after the mark, so a reader that copies any of
    /// them sees `BUSY` (or a later block) at its [`recheck`](Self::recheck).
    #[inline]
    pub fn begin(&self, direction: Direction, slot: usize) {
        self.array(direction)[slot].store(BUSY, Ordering::Relaxed);
        fence(Ordering::Release);
    }

    /// Announce that `slot` now holds block `seq`, in `direction`.
    ///
    /// **Call this exactly once, after [`begin`](Self::begin) and the last
    /// channel's samples are in place.** Publishing per-channel would let a reader observe a slot marked
    /// valid while later channels are still being copied — the block would be
    /// half this block and half the previous one, which is worse than silence
    /// because it sounds almost right.
    ///
    /// `Release` is what makes the preceding sample writes visible to a reader
    /// that acquires this same location. It is the entire safety argument for
    /// the region below; see the module docs.
    #[inline]
    pub fn publish(&self, direction: Direction, slot: usize, seq: u64) {
        self.array(direction)[slot].store(seq, Ordering::Release);
    }

    /// The block currently published in `slot`, or 0 if nothing is: nothing
    /// ever was, or the writer is rewriting it (`BUSY` reads as 0, so a
    /// peer-supplied sequence of `u64::MAX` cannot match a slot mid-write).
    ///
    /// `Acquire` pairs with [`publish`](Self::publish)'s `Release`: once this
    /// returns the expected sequence, every sample write that preceded that
    /// publish is visible to this thread. Reading the samples before this load,
    /// or loading `Relaxed`, would defeat the ordering entirely.
    #[inline]
    pub fn sequence(&self, direction: Direction, slot: usize) -> u64 {
        match self.array(direction)[slot].load(Ordering::Acquire) {
            BUSY => 0,
            seq => seq,
        }
    }

    /// Whether `slot` still holds block `seq` after the reader copied its
    /// samples: an `Acquire` fence, so the copy's loads are ordered before the
    /// second load of the sequence, then that load.
    #[inline]
    pub fn recheck(&self, direction: Direction, slot: usize, seq: u64) -> bool {
        fence(Ordering::Acquire);
        self.array(direction)[slot].load(Ordering::Relaxed) == seq
    }
}

/// The ring slot block `seq` lives in.
///
/// A free function rather than a method because both processes compute it from
/// a sequence number alone, and it must give the same answer on both sides —
/// making it a method on the header would invite one side to derive it from
/// something else.
#[inline]
pub fn slot_for(seq: u64) -> usize {
    (seq % RING_SLOTS as u64) as usize
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Build a zeroed header in ordinary memory. Real slabs live in an mmap, but
    /// the header's logic is address-agnostic and a `Box` exercises it without a
    /// filesystem.
    fn header() -> Box<SlabHeader> {
        // SAFETY: `SlabHeader` is all-atomic `repr(C)` with no padding
        // invariants and no niches, so the all-zero bit pattern is a valid
        // instance — that is precisely the state a freshly-created mmap is in.
        unsafe { Box::new(std::mem::zeroed()) }
    }

    /// The state a fresh mapping is in: zeros mean "nothing published", which is
    /// why sequences start at 1.
    #[test]
    fn a_zeroed_slab_has_published_nothing() {
        let h = header();
        for slot in 0..RING_SLOTS {
            assert_eq!(h.sequence(Direction::Input, slot), 0);
            assert_eq!(h.sequence(Direction::Output, slot), 0);
        }
        assert!(
            h.validate().is_err(),
            "zeros are not a valid slab — the magic must be stamped"
        );
    }

    #[test]
    fn initialize_then_validate_round_trips() {
        let h = header();
        h.initialize();
        assert!(h.validate().is_ok());
    }

    /// A creator that crashed after the version and slot count but before the
    /// magic leaves a header `validate` refuses on the magic.
    #[test]
    fn a_header_without_its_magic_is_refused_on_the_magic() {
        let h = header();
        h.control
            .header_version
            .store(HEADER_VERSION, Ordering::Relaxed);
        h.control.slots.store(RING_SLOTS as u32, Ordering::Relaxed);
        let err = h.validate().unwrap_err();
        assert!(matches!(err, SlabError::BadMagic { .. }), "{err}");
    }

    /// Corrupt magic is rejected. This is the first line of defence against
    /// mapping something that is not ours and reading it as audio.
    #[test]
    fn corrupt_magic_is_rejected() {
        let h = header();
        h.initialize();
        h.control.magic.store(0xDEAD_BEEF, Ordering::Release);
        let err = h.validate().unwrap_err();
        assert!(
            matches!(
                err,
                SlabError::BadMagic {
                    found: 0xDEAD_BEEF,
                    expected: SLAB_MAGIC
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_future_header_version_is_rejected() {
        let h = header();
        h.initialize();
        h.control
            .header_version
            .store(HEADER_VERSION + 1, Ordering::Relaxed);
        let err = h.validate().unwrap_err();
        assert!(
            matches!(err, SlabError::BadVersion { found, expected: HEADER_VERSION } if found == HEADER_VERSION + 1),
            "{err}"
        );
    }

    /// A peer built with a different ring depth must be refused rather than
    /// silently reading the wrong slot.
    #[test]
    fn a_mismatched_slot_count_is_rejected() {
        let h = header();
        h.initialize();
        h.control
            .slots
            .store(RING_SLOTS as u32 + 2, Ordering::Relaxed);
        let err = h.validate().unwrap_err();
        assert!(
            matches!(err, SlabError::BadSlots { found, expected: RING_SLOTS } if found as usize == RING_SLOTS + 2),
            "{err}"
        );
    }

    /// The two directions are independent storage. If they aliased, the host's
    /// own input publish would satisfy its own output check — which is the
    /// bypass bug, reconstructed one level up.
    #[test]
    fn the_two_directions_do_not_alias() {
        let h = header();
        h.initialize();
        h.publish(Direction::Input, 0, 7);
        assert_eq!(h.sequence(Direction::Input, 0), 7);
        assert_eq!(
            h.sequence(Direction::Output, 0),
            0,
            "publishing an input must not mark an output as ready"
        );
    }

    /// Slots are independent, so a stale slot cannot masquerade as a fresh one.
    #[test]
    fn slots_are_independent() {
        let h = header();
        h.initialize();
        h.publish(Direction::Output, 0, 10);
        assert_eq!(h.sequence(Direction::Output, 1), 0);
    }

    /// `slot_for` must agree with the ring depth and be stable across the u64
    /// range — both processes derive the slot from the sequence alone.
    #[test]
    fn slot_for_wraps_with_the_ring() {
        for seq in 0u64..8 {
            assert_eq!(slot_for(seq), (seq as usize) % RING_SLOTS);
        }
        // Consecutive blocks always land in different slots at depth 2 — this is
        // what lets one be read while the next is written.
        for seq in 0u64..8 {
            assert_ne!(slot_for(seq), slot_for(seq + 1));
        }
    }

    /// A busy slot names no block: `begin` hides the previous occupant, and a
    /// peer-supplied `u64::MAX` cannot match it.
    #[test]
    fn a_busy_slot_names_no_block() {
        let h = header();
        h.initialize();
        h.publish(Direction::Output, 1, 5);
        h.begin(Direction::Output, 1);
        assert_eq!(h.sequence(Direction::Output, 1), 0);
        assert_ne!(h.sequence(Direction::Output, 1), u64::MAX);
        assert!(!h.recheck(Direction::Output, 1, 5));
        h.publish(Direction::Output, 1, 7);
        assert_eq!(h.sequence(Direction::Output, 1), 7);
        assert!(h.recheck(Direction::Output, 1, 7));
    }

    /// Two threads run the real seqlock against a real header: the writer laps
    /// the two slots freely, the reader takes whatever block a slot holds, and
    /// every accepted read has both of its words from one block. The ordering
    /// itself is `loom_model`'s; this catches a protocol that tears under real
    /// preemption.
    #[test]
    fn a_free_running_writer_never_shows_a_reader_a_torn_block() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64 as StdAtomicU64;

        const BLOCKS: u64 = 1_000_000;
        struct Region {
            header: Box<SlabHeader>,
            data: [[StdAtomicU64; 2]; RING_SLOTS],
        }
        let region = Arc::new(Region {
            header: header(),
            data: [const { [const { StdAtomicU64::new(0) }; 2] }; RING_SLOTS],
        });
        region.header.initialize();

        let writer = {
            let region = Arc::clone(&region);
            std::thread::spawn(move || {
                for seq in 1..=BLOCKS {
                    let slot = slot_for(seq);
                    region.header.begin(Direction::Output, slot);
                    region.data[slot][0].store(seq, Ordering::Relaxed);
                    region.data[slot][1].store(seq, Ordering::Relaxed);
                    region.header.publish(Direction::Output, slot, seq);
                }
            })
        };

        let (mut accepted, mut torn) = (0u64, 0u64);
        while !writer.is_finished() {
            for slot in 0..RING_SLOTS {
                let seq = region.header.sequence(Direction::Output, slot);
                if seq == 0 {
                    continue;
                }
                let a = region.data[slot][0].load(Ordering::Relaxed);
                let b = region.data[slot][1].load(Ordering::Relaxed);
                if region.header.recheck(Direction::Output, slot, seq) {
                    accepted += 1;
                    torn += u64::from(a != seq || b != seq);
                }
            }
        }
        writer.join().expect("writer thread");
        assert!(accepted > 0, "the reader never caught a block");
        assert_eq!(torn, 0, "{torn} of {accepted} accepted reads were torn");
    }

    /// The header must not grow into the audio regions' space unnoticed, and the
    /// regions must start on an 8-byte boundary so `f64` samples are aligned.
    #[test]
    fn header_size_is_sane_and_sample_aligned() {
        assert!(SLAB_HEADER_BYTES >= size_of::<u64>() * 4);
        assert_eq!(
            SLAB_HEADER_BYTES % align_of::<f64>(),
            0,
            "audio regions start right after the header and must stay aligned"
        );
    }
}

/// Loom models of the block ring's Release/Acquire pair and the setup magic's,
/// run against this file's code under `--cfg loom`.
#[cfg(all(test, loom))]
mod loom_model {
    use super::{Direction, RING_SLOTS, SlabHeader, slot_for};
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::thread;

    /// A header and the words standing for the samples it guards: two per slot,
    /// written `Relaxed` between `begin` and `publish` and read `Relaxed`
    /// between the sequence check and the recheck, so all the ordering is the
    /// header's. Two words, so a read that spans two blocks shows two values.
    struct Region {
        header: SlabHeader,
        data: [[AtomicU64; 2]; RING_SLOTS],
    }

    impl Region {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                header: SlabHeader::zeroed(),
                data: std::array::from_fn(|_| [AtomicU64::new(0), AtomicU64::new(0)]),
            })
        }

        fn publish(&self, direction: Direction, seq: u64) {
            let slot = slot_for(seq);
            self.header.begin(direction, slot);
            self.data[slot][0].store(seq, Ordering::Relaxed);
            self.data[slot][1].store(seq, Ordering::Relaxed);
            self.header.publish(direction, slot, seq);
        }

        /// The slot's two words when its sequence is `want` before and after
        /// the copy, `None` when it is not.
        fn read(&self, direction: Direction, want: u64) -> Option<(u64, u64)> {
            let slot = slot_for(want);
            if self.header.sequence(direction, slot) != want {
                return None;
            }
            let words = (
                self.data[slot][0].load(Ordering::Relaxed),
                self.data[slot][1].load(Ordering::Relaxed),
            );
            self.header.recheck(direction, slot, want).then_some(words)
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
                    assert_eq!(data, (1, 1), "the sequence was visible without its samples");
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
                    assert_eq!(data, (want, want), "a read of block {want} got {data:?}");
                }
            }
            writer.join().expect("writer thread");
        });
    }

    /// The writer laps slot 1 (blocks 1 then 3) while a reader copies block 1:
    /// an accepted read is block 1's two words, never one word of each. Without
    /// `begin`'s mark or `recheck`'s fence the model finds the torn pair.
    #[test]
    fn a_reader_overtaken_by_a_lapping_writer_never_accepts_a_torn_block() {
        loom::model(|| {
            let region = Region::new();
            let writer = {
                let region = Arc::clone(&region);
                thread::spawn(move || {
                    for seq in 1..=3u64 {
                        region.publish(Direction::Output, seq);
                    }
                })
            };
            if let Some(words) = region.read(Direction::Output, 1) {
                assert_eq!(words, (1, 1), "an accepted read of block 1 was torn");
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
}
