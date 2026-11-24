//! [`SeqSlotsWriter`] and [`SeqSlotsReader`]: an array of sequence cells, keyed
//! by slot index, that one thread overwrites in place and one thread drains,
//! with an epoch that makes every write before it stale.
//!
//! A slot holds the latest `T` written to it, so a newer write replaces an
//! older one nobody has drained: nothing queues, nothing fills. The reader
//! yields each slot written since its last drain once, with the value of its
//! latest write. Neither side allocates, locks or waits after construction.
//!
//! # The epoch
//!
//! [`SeqSlotsWriter::new_epoch`] ends the current epoch. Each write stores the
//! epoch it was made in beside its value, in the same cell, so a copy is one
//! write's value with that write's epoch. A drain loads the epoch once, after
//! it has read every slot, and drops a copy from an earlier epoch. Epochs are
//! 32 bits and compare as serial numbers, so they wrap.
//!
//! # The two fences
//!
//! A slot is a [`SeqWriter`] and [`SeqReader`] pair, so the copy protocol is
//! theirs (loom-checked in `tests/seq_cell_loom.rs`); this module adds the
//! epoch and the generation around it.
//!
//! The reader's `SeqCst` fence, between its slot reads and its epoch load,
//! synchronises with the `SeqCst` fence that ends
//! [`new_epoch`](SeqSlotsWriter::new_epoch), after its epoch store. This is
//! fence-to-fence ordering for a relaxed observation, not a store-then-load
//! pair: the reader's side is a load then a load. The pair orders the epoch store before
//! whatever the caller does after `new_epoch` returns (a plugin's `set_state`
//! the stale writes must not overwrite): a drain that observes that work
//! loads the new epoch, so a copy it read before the work is dropped, never
//! yielded after it. The model in `tests/seq_slots_loom.rs` fails without the
//! writer's fence; loom does not reorder loads, so it cannot show the
//! reader's.
//!
//! # The generation
//!
//! The writer bumps a generation (wrapping, 32 bits) after every cell write.
//! The reader loads it before the reads and sweeps only when it differs from
//! the one it last recorded, so a drain over untouched slots is one load. It
//! records the generation only after a sweep that no write overlapped, so a
//! copy a write tore is read again by the next drain and never missed.
//!
//! Both ends say where they are as a [`Generation`]:
//! [`SeqSlotsWriter::written`] and [`SeqSlotsReader::swept`]. Once `swept` has
//! reached a `written`, every slot's latest write up to it has been collected
//! by a drain, and so handed to the drain's callback or dropped as stale by a
//! new epoch. A sweep a write overlapped does not move `swept`, so it lags
//! and never runs ahead.
//!
//! The `loom` model in `tests/seq_slots_loom.rs` runs this code.

use std::sync::Arc;

use crate::sync::{AtomicU32, Ordering, fence};
use bytemuck::{Pod, Zeroable};

use crate::{Attempt, SeqReader, SeqWriter};

/// A slot's cell: the value and the epoch of the write that stored it. Packed,
/// so the cell has no padding for any `T` and is plain bytes (`Pod`).
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C, packed)]
struct Stamped<T> {
    value: T,
    epoch: u32,
}

/// Whether serial `a` is before serial `b`, as 32-bit serial numbers.
const fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// A count of a slot array's writes, as a wrapping 32-bit serial.
///
/// The writer's is [`SeqSlotsWriter::written`], the reader's
/// [`SeqSlotsReader::swept`]. [`get`](Self::get) and [`new`](Self::new) carry one
/// through an atomic word to another thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(u32);

impl Generation {
    /// The generation a raw serial names: for the side that receives one
    /// another thread published with [`get`](Self::get). The two ends make
    /// their own.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw serial.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Whether this generation is `other` or later, as serial numbers: exact
    /// while the two are less than `2^31` writes apart.
    #[must_use]
    pub const fn has_reached(self, other: Self) -> bool {
        !before(self.0, other.0)
    }
}

/// `len` empty slots holding a `T` each, as the writer and the reader.
#[must_use]
pub fn seq_slots<T: Pod>(len: usize) -> (SeqSlotsWriter<T>, SeqSlotsReader<T>) {
    let cells: Vec<SeqWriter<Stamped<T>>> = (0..len).map(|_| SeqWriter::new()).collect();
    let readers = cells.iter().map(SeqWriter::reader).collect();
    let epoch = Arc::new(AtomicU32::new(0));
    let dirty = Arc::new(AtomicU32::new(0));
    (
        SeqSlotsWriter {
            cells,
            epoch: 0,
            epoch_word: Arc::clone(&epoch),
            generation: 0,
            dirty: Arc::clone(&dirty),
        },
        SeqSlotsReader {
            cells: readers,
            seen: vec![0; len],
            scratch: Vec::with_capacity(len),
            epoch,
            generation: 0,
            dirty,
        },
    )
}

/// The one writer of an array of sequence slots. Not `Clone`, and its methods
/// take `&mut self`, so two writes never overlap.
pub struct SeqSlotsWriter<T: Pod> {
    cells: Vec<SeqWriter<Stamped<T>>>,
    /// The epoch of the writes made now.
    epoch: u32,
    /// The epoch of the last [`new_epoch`](Self::new_epoch), which the reader loads.
    epoch_word: Arc<AtomicU32>,
    /// Counts the writes (wrapping), stored after each cell write.
    generation: u32,
    dirty: Arc<AtomicU32>,
}

impl<T: Pod> SeqSlotsWriter<T> {
    /// Overwrites slot `index` with `value`, in the current epoch. Wait-free;
    /// allocates nothing.
    ///
    /// # Panics
    ///
    /// If `index` is not below the slot count.
    pub fn write(&mut self, index: usize, value: T) {
        self.cells[index].write(Stamped {
            value,
            epoch: self.epoch,
        });
        // After the cell write: a reader that loads this generation and then
        // reads the cells sees the write.
        self.generation = self.generation.wrapping_add(1);
        self.dirty.store(self.generation, Ordering::Release);
    }

    /// The generation of the last [`write`](Self::write): how many writes
    /// there have been, wrapping.
    #[must_use]
    pub const fn written(&self) -> Generation {
        Generation(self.generation)
    }

    /// Ends the epoch: every write made so far is stale, and no drain that
    /// observes this yields it.
    ///
    /// The `SeqCst` fence at the end orders the epoch store before anything the
    /// caller does after the call, and pairs with the reader's.
    pub fn new_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch_word.store(self.epoch, Ordering::Release);
        // Synchronises with the reader's fence before its epoch load.
        fence(Ordering::SeqCst);
    }
}

/// Reads the epoch of a slot array from any thread ([`SeqSlotsReader::epoch_watch`]).
#[derive(Clone)]
pub struct EpochWatch(Arc<AtomicU32>);

impl EpochWatch {
    /// The epoch now, as [`SeqSlotsReader::epoch`] reads it. Wait-free.
    #[must_use]
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for EpochWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochWatch").finish_non_exhaustive()
    }
}

/// The one reader of an array of sequence slots.
pub struct SeqSlotsReader<T: Pod> {
    cells: Vec<SeqReader<Stamped<T>>>,
    /// The sequence of the last write each slot gave up, per slot.
    seen: Vec<u64>,
    /// The cells [`collect`](Self::collect) read, reserved for every slot.
    scratch: Vec<(usize, Stamped<T>)>,
    epoch: Arc<AtomicU32>,
    /// The generation of the last sweep that no write overlapped.
    generation: u32,
    dirty: Arc<AtomicU32>,
}

impl<T: Pod> SeqSlotsReader<T> {
    /// Hands `apply` each slot written since the last drain, by index, with the
    /// value of its latest write, skipping what [`new_epoch`](SeqSlotsWriter::new_epoch)
    /// made stale. Reads each slot once and never retries: a slot a write
    /// overlaps is read on the next drain. Never allocates, locks or waits.
    pub fn drain(&mut self, mut apply: impl FnMut(usize, T)) {
        self.drain_in_epoch(|slot, value, _| apply(slot, value));
    }

    /// [`drain`](Self::drain), handing `apply` the epoch the drain observed
    /// beside each value: the one every value it yields was checked against, so
    /// a caller that keeps a value past the drain knows which epoch it is
    /// valid in ([`epoch`](Self::epoch) says which is current later).
    pub fn drain_in_epoch(&mut self, apply: impl FnMut(usize, T, u32)) {
        self.collect();
        self.finish(apply);
    }

    /// The generation of the last drain no write overlapped. Once it
    /// [has reached](Generation::has_reached) a [`SeqSlotsWriter::written`],
    /// every slot's latest write up to that one has been handed to a drain's
    /// callback, or dropped as stale by a new epoch.
    #[must_use]
    pub const fn swept(&self) -> Generation {
        Generation(self.generation)
    }

    /// The epoch now: the number of [`new_epoch`](SeqSlotsWriter::new_epoch)
    /// calls so far, as a wrapping serial.
    #[must_use]
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }

    /// A handle that reads the epoch without the reader: the one word the writer's
    /// `new_epoch` bumps, so a thread that does not hold the reader (the reader
    /// is claimed elsewhere) still learns that a reset happened.
    #[must_use]
    pub fn epoch_watch(&self) -> EpochWatch {
        EpochWatch(Arc::clone(&self.epoch))
    }

    /// Phase one: reads every slot a write has changed into the scratch buffer.
    fn collect(&mut self) {
        // Before the reads, and recorded only when no slot was contended.
        let generation = self.dirty.load(Ordering::Acquire);
        if generation == self.generation {
            return;
        }
        let mut contended = false;
        for (slot, cell) in self.cells.iter().enumerate() {
            match cell.try_read() {
                Attempt::Stable(sequence, stamped) => {
                    if sequence != self.seen[slot] {
                        self.seen[slot] = sequence;
                        // At most one per slot, within the reserved room.
                        self.scratch.push((slot, stamped));
                    }
                }
                Attempt::Never => {}
                Attempt::Torn => contended = true,
            }
        }
        if !contended {
            self.generation = generation;
        }
    }

    /// Phases two and three: loads the epoch once, after every read, and applies
    /// the collected cells it does not make stale.
    fn finish(&mut self, mut apply: impl FnMut(usize, T, u32)) {
        if self.scratch.is_empty() {
            return;
        }
        // Synchronises with the fence that ends `new_epoch`: the reads are
        // ordered before the epoch load.
        fence(Ordering::SeqCst);
        let epoch = self.epoch.load(Ordering::Acquire);
        for (slot, stamped) in self.scratch.drain(..) {
            let (value, written) = (stamped.value, stamped.epoch);
            if !before(written, epoch) {
                apply(slot, value, epoch);
            }
        }
    }
}

impl<T: Pod> std::fmt::Debug for SeqSlotsWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeqSlotsWriter")
            .field("slots", &self.cells.len())
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl<T: Pod> std::fmt::Debug for SeqSlotsReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeqSlotsReader")
            .field("slots", &self.cells.len())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
