//! The evicting ring: a bounded queue whose writer, when full, takes the
//! oldest value itself.
//!
//! Because the writer is a second taker, a position's value is claimed by a
//! compare and exchange on `tail`, and a slot carries a sequence number that
//! says whose turn it is. Position `p` is slot `p % capacity`; a slot's `seq`
//! is `2p` while it waits for the value at `p`, `2p + 1` once that value is
//! published, and `2(p + capacity)` once it has been taken (the next lap's
//! `2p`; the doubling keeps a one-slot ring's published and free states
//! apart).
//!
//! - **Push** (one writer): the slot at `head` is free when its `seq` is
//!   `2 * head`; write the value, store `seq = 2 * head + 1` with `Release`,
//!   then publish `head + 1`. If the slot is not free and the oldest value
//!   (position `head - capacity`) is still queued, the writer claims exactly
//!   that position, drops the value, frees the slot and writes. If the reader
//!   has claimed it already, its read is in flight and the push is refused
//!   (and counted): the writer never takes a value it did not see full.
//! - **Take** (the reader): load `tail`; when the slot's `seq` is
//!   `2 * tail + 1`, claim the position with a compare and exchange on `tail`,
//!   read the value, and store `seq = 2 * (tail + capacity)` with `Release`.
//!   A lost claim reloads `tail`.
//!
//! A queued value can be taken by the writer at any moment, so values cannot
//! be borrowed: there is no `peek` or `iter`.

use std::sync::Arc;

use super::sync::{AtomicBool, AtomicU64, Ordering};
use super::{
    Cell, NotSync, Padded, PushError, Returned, WriterLoss, check_free_outside, take_since,
};
use crate::gauge::Counter;

struct Slot<T> {
    seq: AtomicU64,
    value: Cell<T>,
}

struct Shared<T> {
    slots: Box<[Slot<T>]>,
    /// Values pushed; the writer is its only storer.
    head: Padded<AtomicU64>,
    /// Values claimed, by the reader or by the writer evicting.
    tail: Padded<AtomicU64>,
    writer_gone: AtomicBool,
    reader_gone: AtomicBool,
}

// SAFETY: the slot cells are the only non-`Sync` field. The writer writes a
// slot only when its `seq` equals twice the position it is pushing, which a
// taker stores (with `Release`, after its read) and the writer
// `Acquire`-loads; a taker reads a slot only after claiming its position on
// `tail`, having `Acquire`-loaded the `seq` the writer published after its
// write. So no two accesses to a cell overlap. The ends are `!Sync`. Values
// cross threads, hence `T: Send`.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Shared<T> {
    fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    /// Claims and reads the oldest published value, or `None` when there is
    /// none. Lock-free: a lost claim means the other taker made progress.
    fn take(&self) -> Option<T> {
        let capacity = self.capacity();
        if capacity == 0 {
            return None;
        }
        let mut pos = self.tail.0.load(Ordering::Acquire);
        loop {
            let slot = &self.slots[(pos % capacity) as usize];
            let seq = slot.seq.load(Ordering::Acquire);
            let published = 2 * pos + 1;
            if seq == published {
                match self.tail.0.compare_exchange(
                    pos,
                    pos + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(self.read(slot, pos)),
                    Err(now) => pos = now,
                }
            } else if seq < published {
                return None;
            } else {
                // The other taker took `pos` and the slot is on its next lap.
                pos = self.tail.0.load(Ordering::Acquire);
            }
        }
    }

    /// Reads the value at the claimed position `pos` and frees its slot.
    fn read(&self, slot: &Slot<T>, pos: u64) -> T {
        // SAFETY: the caller claimed `pos` on `tail` after seeing its `seq`
        // published, so this call is the position's only taker; the writer
        // waits for the store below.
        let value = unsafe { slot.value.take() };
        slot.seq
            .store(2 * (pos + self.capacity()), Ordering::Release);
        value
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        if self.tail.0.load(Ordering::Acquire) < self.head.0.load(Ordering::Acquire) {
            check_free_outside::<T>();
        }
        while self.take().is_some() {}
    }
}

/// A ring holding the newest `capacity` values: a push into a full ring drops
/// the oldest queued value on the pushing thread and counts it lost. Allocates;
/// control side.
///
/// ```
/// use tutti_rt::evicting_ring;
///
/// let (mut writer, mut reader) = evicting_ring::<u32>(3);
/// for n in 0..10 {
///     writer.push(n).unwrap();
/// }
/// assert_eq!(writer.lost(), 7);
/// assert_eq!(reader.drain().collect::<Vec<_>>(), [7, 8, 9]);
/// ```
///
/// An end is `Send` and not `Sync`.
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<tutti_rt::EvictReader<u8>>();
/// ```
#[must_use]
pub fn evicting_ring<T: Send>(capacity: usize) -> (EvictWriter<T>, EvictReader<T>) {
    let slots = (0..capacity as u64)
        .map(|pos| Slot {
            seq: AtomicU64::new(2 * pos),
            value: Cell::new(),
        })
        .collect();
    let shared = Arc::new(Shared {
        slots,
        head: Padded(AtomicU64::new(0)),
        tail: Padded(AtomicU64::new(0)),
        writer_gone: AtomicBool::new(false),
        reader_gone: AtomicBool::new(false),
    });
    let (loss, view) = WriterLoss::new();
    (
        EvictWriter {
            shared: Arc::clone(&shared),
            head: 0,
            loss,
            _not_sync: NotSync::default(),
        },
        EvictReader {
            shared,
            view,
            taken: 0,
            _not_sync: NotSync::default(),
        },
    )
}

/// The producing end of an [`evicting_ring()`]. `Send`, not `Clone`, not
/// `Sync`.
pub struct EvictWriter<T> {
    shared: Arc<Shared<T>>,
    head: u64,
    loss: WriterLoss,
    _not_sync: NotSync,
}

/// The consuming end of an [`evicting_ring()`]. `Send`, not `Clone`, not
/// `Sync`.
pub struct EvictReader<T> {
    shared: Arc<Shared<T>>,
    view: Counter<u64>,
    taken: u64,
    _not_sync: NotSync,
}

impl<T> std::fmt::Debug for EvictWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvictWriter")
            .field("capacity", &self.shared.slots.len())
            .finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for EvictReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvictReader")
            .field("capacity", &self.shared.slots.len())
            .finish_non_exhaustive()
    }
}

impl<T> Drop for EvictWriter<T> {
    fn drop(&mut self) {
        self.shared.writer_gone.store(true, Ordering::Release);
    }
}

impl<T> Drop for EvictReader<T> {
    fn drop(&mut self) {
        self.shared.reader_gone.store(true, Ordering::Release);
        let queued =
            self.shared.tail.0.load(Ordering::Acquire) < self.shared.head.0.load(Ordering::Acquire);
        if queued {
            check_free_outside::<T>();
        }
        while self.shared.take().is_some() {}
    }
}

impl<T: Send> EvictWriter<T> {
    /// Queues `value`, dropping (and counting) the oldest queued value when
    /// the ring is full. Lock-free; never allocates, locks or waits.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] (the value is handed back, and counted lost) only
    /// when the reader was in the middle of taking the oldest value, so
    /// nothing could be evicted; [`PushError::Disconnected`] once the reader
    /// is dropped.
    pub fn push(&mut self, value: T) -> Result<(), Returned<T>> {
        let shared = &*self.shared;
        if shared.reader_gone.load(Ordering::Acquire) {
            return Err(Returned {
                error: PushError::Disconnected,
                value,
            });
        }
        let capacity = shared.capacity();
        let pos = self.head;
        if capacity == 0 {
            self.loss.add(1);
            return Err(Returned {
                error: PushError::Full,
                value,
            });
        }
        let slot = &shared.slots[(pos % capacity) as usize];
        if slot.seq.load(Ordering::Acquire) != 2 * pos {
            // Not free: the oldest value (`pos - capacity`) is queued or being
            // read. Take it only if it is still queued.
            let oldest = pos - capacity;
            let claimed = slot.seq.load(Ordering::Acquire) == 2 * oldest + 1
                && shared
                    .tail
                    .0
                    .compare_exchange(oldest, oldest + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok();
            if !claimed {
                self.loss.add(1);
                return Err(Returned {
                    error: PushError::Full,
                    value,
                });
            }
            drop(shared.read(slot, oldest));
            self.loss.add(1);
        }
        // SAFETY: `seq == 2 * pos`: the slot's last value was taken and its
        // taker finished (its `Release` store, loaded above or stored by
        // `read` here), and this is the only writer.
        unsafe { slot.value.write(value) };
        slot.seq.store(2 * pos + 1, Ordering::Release);
        self.head = pos + 1;
        shared.head.0.store(pos + 1, Ordering::Release);
        Ok(())
    }

    /// The most values the ring holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Whether the reader was dropped: every push is refused for good.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.shared.reader_gone.load(Ordering::Acquire)
    }

    /// Values the ring has lost since it was made, cumulative.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.loss.lost()
    }

    /// Values lost since this end's last `take_lost`, and resets that count;
    /// [`lost`](Self::lost) is unaffected.
    pub fn take_lost(&mut self) -> u64 {
        self.loss.take()
    }

    /// A view of the loss count that outlives the end.
    #[must_use]
    pub fn losses(&self) -> Counter<u64> {
        self.loss.view.clone()
    }
}

impl<T: Send> EvictReader<T> {
    /// The oldest queued value, or `None` when the ring is empty. Lock-free;
    /// never allocates, locks or waits.
    pub fn pop(&mut self) -> Option<T> {
        self.shared.take()
    }

    /// Takes the values queued now (those a later push adds are for the next
    /// call), oldest first, one per step; one left unread stays queued.
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        let mut left = self.len();
        std::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            left -= 1;
            self.pop()
        })
    }

    /// How many values are queued now: at least what a pop sees, more if the
    /// writer pushes meanwhile.
    #[must_use]
    pub fn len(&self) -> usize {
        let tail = self.shared.tail.0.load(Ordering::Acquire);
        let head = self.shared.head.0.load(Ordering::Acquire);
        head.saturating_sub(tail) as usize
    }

    /// Whether nothing is queued now.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most values the ring holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Whether the writer was dropped: no value will be pushed again. Ask
    /// before taking, as [`RingReader::is_disconnected`](super::RingReader::is_disconnected) says.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.shared.writer_gone.load(Ordering::Acquire)
    }

    /// Values the ring has lost since it was made, cumulative.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.view.get()
    }

    /// Values lost since this end's last `take_lost`, and resets that count;
    /// [`lost`](Self::lost) is unaffected.
    pub fn take_lost(&mut self) -> u64 {
        take_since(&self.view, &mut self.taken)
    }

    /// A view of the loss count that outlives the end.
    #[must_use]
    pub fn losses(&self) -> Counter<u64> {
        self.view.clone()
    }
}
