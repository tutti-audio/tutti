//! [`Ring`]: a bounded single-producer single-consumer queue of any `Send`
//! value, with the full-ring policy chosen at construction and the losses
//! counted by the ring itself.
//!
//! # The policy
//!
//! [`Overflow`] says what a push into a full ring does:
//!
//! - **`DropNewest`**: the pushed value is refused and counted lost; what is
//!   queued stays. The value comes back in the error, so a caller whose value
//!   must not be freed on this thread (a `Box`) can send it elsewhere.
//! - **`Evict`**: the oldest queued value is dropped (on the pushing thread)
//!   and counted lost, and the push succeeds.
//! - **`Refuse`**: the value is refused and *not* counted: the caller owns the
//!   decision (retry later, count it in its own unit, or treat it as
//!   impossible by a credit count).
//!
//! # The protocol
//!
//! A bounded queue of per-slot sequence numbers. Position `p` is slot
//! `p % capacity`; a slot's `seq` is `2p` while it waits for the value at `p`,
//! `2p + 1` once that value is published, and `2(p + capacity)` once it has
//! been taken (the next lap's `2p`; the doubling keeps a one-slot ring's
//! published and free states apart).
//!
//! - **Push** (one writer): the slot at `head` is free when its `seq` is
//!   `2 * head`; write the value, store `seq = 2 * head + 1` with `Release`,
//!   then publish `head + 1`.
//! - **Take** (the reader, and the writer when it evicts): load `tail`; when
//!   the slot's `seq` is `2 * tail + 1`, claim the position with a compare and
//!   exchange on `tail`, read the value, and store
//!   `seq = 2 * (tail + capacity)` with `Release`. A lost claim reloads
//!   `tail`.
//!
//! The claim makes a position's value belong to exactly one taker, and the
//! writer reuses a slot only after the `Release` store that ends its taker's
//! read, so no two accesses to a slot overlap. Without eviction only the
//! reader takes, so the claim never fails. Nothing waits: a push or a take is
//! a handful of atomic operations.
//!
//! The model in `tests/ring_loom.rs` runs this code.

use std::mem::MaybeUninit;
use std::sync::Arc;

use crate::gauge::{Counter, CounterWriter, counter};
use sync::{AtomicBool, AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
}

/// What a push into a full [`Ring`] does. Chosen when the ring is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Overflow {
    /// Refuse the pushed value and count it lost.
    DropNewest,
    /// Drop the oldest queued value, count it lost, and take the pushed one.
    Evict,
    /// Refuse the pushed value without counting it: the caller decides what a
    /// full ring means.
    Refuse,
}

/// Why a push did not queue its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum PushError {
    /// The ring was full (and did not evict).
    #[error("the ring is full")]
    Full,
    /// The reading end was dropped: nothing will take the value, now or later.
    #[error("the ring's reader is gone")]
    Disconnected,
}

/// A value a push did not take, handed back with the reason, so the caller
/// chooses where it is freed.
#[derive(thiserror::Error)]
#[error("{error}")]
pub struct Returned<T> {
    /// Why it was not queued.
    pub error: PushError,
    /// The value, unqueued.
    pub value: T,
}

impl<T> std::fmt::Debug for Returned<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Returned")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// A slot's value cell: loom's under `--cfg loom`, so the model sees every
/// hand-over of a slot.
#[cfg(loom)]
struct Cell<T>(loom::cell::UnsafeCell<MaybeUninit<T>>);
#[cfg(not(loom))]
struct Cell<T>(std::cell::UnsafeCell<MaybeUninit<T>>);

impl<T> Cell<T> {
    fn new() -> Self {
        #[cfg(loom)]
        return Self(loom::cell::UnsafeCell::new(MaybeUninit::uninit()));
        #[cfg(not(loom))]
        Self(std::cell::UnsafeCell::new(MaybeUninit::uninit()))
    }

    /// # Safety
    ///
    /// The slot is free and the caller is the only writer: its `seq` is
    /// twice the position being written.
    unsafe fn write(&self, value: T) {
        #[cfg(loom)]
        // SAFETY: the caller's.
        self.0.with_mut(|slot| unsafe { (*slot).write(value) });
        #[cfg(not(loom))]
        // SAFETY: the caller's.
        unsafe {
            (*self.0.get()).write(value);
        }
    }

    /// # Safety
    ///
    /// The slot holds a published value and the caller has claimed its
    /// position, so no other access runs until the slot's `seq` is advanced.
    unsafe fn take(&self) -> T {
        #[cfg(loom)]
        // SAFETY: the caller's.
        return self
            .0
            .with_mut(|slot| unsafe { (*slot).assume_init_read() });
        #[cfg(not(loom))]
        // SAFETY: the caller's.
        unsafe {
            (*self.0.get()).assume_init_read()
        }
    }

    /// # Safety
    ///
    /// The slot holds a published value that nothing takes while the borrow
    /// lives: the ring does not evict and the caller is its only reader.
    unsafe fn borrow(&self) -> &T {
        #[cfg(loom)]
        // SAFETY: the caller's.
        return self.0.with(|slot| unsafe { (*slot).assume_init_ref() });
        #[cfg(not(loom))]
        // SAFETY: the caller's.
        unsafe {
            (*self.0.get()).assume_init_ref()
        }
    }
}

struct Slot<T> {
    seq: AtomicU64,
    value: Cell<T>,
}

struct Shared<T> {
    slots: Box<[Slot<T>]>,
    /// Values pushed; the writer is its only storer.
    head: AtomicU64,
    /// Values taken, by the reader or by the writer evicting.
    tail: AtomicU64,
    overflow: Overflow,
    writer_gone: AtomicBool,
    reader_gone: AtomicBool,
}

// SAFETY: the slot cells are the only non-`Sync` field. The writer writes a
// slot only when its `seq` equals the position it is pushing, which a taker
// stores (with `Release`, after its read) and the writer `Acquire`-loads; a
// taker reads a slot only after claiming its position on `tail`, having
// `Acquire`-loaded the `seq` the writer published after its write. So no two
// accesses to a cell overlap, and the loom model checks it. Values cross
// threads, hence `T: Send`.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Shared<T> {
    fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    /// Claims and reads the oldest published value, or `None` when there is
    /// none. Lock-free: a lost claim means another taker made progress.
    fn take(&self) -> Option<T> {
        let capacity = self.capacity();
        if capacity == 0 {
            return None;
        }
        let mut pos = self.tail.load(Ordering::Acquire);
        loop {
            let slot = &self.slots[(pos % capacity) as usize];
            let seq = slot.seq.load(Ordering::Acquire);
            let published = 2 * pos + 1;
            if seq == published {
                match self
                    .tail
                    .compare_exchange(pos, pos + 1, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => {
                        // SAFETY: `seq == 2 * pos + 1` says the value at `pos` is
                        // published, and the exchange made this call its only
                        // taker; the writer waits for the store below.
                        let value = unsafe { slot.value.take() };
                        slot.seq.store(2 * (pos + capacity), Ordering::Release);
                        return Some(value);
                    }
                    Err(now) => pos = now,
                }
            } else if seq < published {
                return None;
            } else {
                // A taker ahead of this one took `pos` and the slot is on its
                // next lap.
                pos = self.tail.load(Ordering::Acquire);
            }
        }
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        while self.take().is_some() {}
    }
}

/// The namespace of the bounded single-producer single-consumer queue: build
/// one with [`Ring::pair`].
///
/// ```
/// use tutti_rt::{Overflow, PushError, Ring};
///
/// let (mut writer, mut reader) = Ring::<u32>::pair(2, Overflow::DropNewest);
/// writer.push(1).unwrap();
/// writer.push(2).unwrap();
/// let refused = writer.push(3).unwrap_err();
/// assert_eq!((refused.error, refused.value), (PushError::Full, 3));
/// assert_eq!(writer.lost(), 1);
/// assert_eq!(reader.pop(), Some(1));
/// assert_eq!(reader.pop(), Some(2));
/// assert_eq!(reader.pop(), None);
/// assert_eq!(reader.take_lost(), 1);
/// assert_eq!(reader.take_lost(), 0, "taking resets");
/// assert_eq!(reader.lost(), 1, "the total stands");
/// ```
#[derive(Debug)]
pub struct Ring<T>(std::marker::PhantomData<fn(T)>);

impl<T: Send> Ring<T> {
    /// A ring holding up to `capacity` values (zero holds none: every push is
    /// refused), as its two ends. Allocates; control side.
    #[must_use]
    pub fn pair(capacity: usize, overflow: Overflow) -> (RingWriter<T>, RingReader<T>) {
        let slots = (0..capacity as u64)
            .map(|pos| Slot {
                seq: AtomicU64::new(2 * pos),
                value: Cell::new(),
            })
            .collect();
        let (lost_writer, lost) = counter::<u64>();
        let shared = Arc::new(Shared {
            slots,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            overflow,
            writer_gone: AtomicBool::new(false),
            reader_gone: AtomicBool::new(false),
        });
        (
            RingWriter {
                shared: Arc::clone(&shared),
                head: 0,
                lost_writer,
                lost: lost.clone(),
                taken_lost: 0,
            },
            RingReader {
                shared,
                lost,
                taken_lost: 0,
            },
        )
    }
}

/// The producing end of a [`Ring`]. `Send`, not `Clone`: one thread pushes.
pub struct RingWriter<T> {
    shared: Arc<Shared<T>>,
    head: u64,
    /// The one writer of the loss count: only a push loses a value.
    lost_writer: CounterWriter<u64>,
    lost: Counter<u64>,
    taken_lost: u64,
}

/// The consuming end of a [`Ring`]. `Send`, not `Clone`: one thread takes.
pub struct RingReader<T> {
    shared: Arc<Shared<T>>,
    lost: Counter<u64>,
    taken_lost: u64,
}

impl<T> std::fmt::Debug for RingWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingWriter")
            .field("capacity", &self.shared.slots.len())
            .field("overflow", &self.shared.overflow)
            .finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for RingReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingReader")
            .field("capacity", &self.shared.slots.len())
            .field("overflow", &self.shared.overflow)
            .finish_non_exhaustive()
    }
}

impl<T> Drop for RingWriter<T> {
    fn drop(&mut self) {
        self.shared.writer_gone.store(true, Ordering::Release);
    }
}

impl<T> Drop for RingReader<T> {
    fn drop(&mut self) {
        self.shared.reader_gone.store(true, Ordering::Release);
    }
}

impl<T: Send> RingWriter<T> {
    /// Queues `value`, by the ring's [`Overflow`] when it is full. Wait-free
    /// without eviction; never allocates, locks or waits. An evicted value is
    /// dropped here, on the pushing thread.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] (the value is handed back) when the ring is full
    /// and does not evict, or when an evicting ring found no room because a
    /// take of its oldest value was still in flight;
    /// [`PushError::Disconnected`] once the reader is dropped.
    pub fn push(&mut self, value: T) -> Result<(), Returned<T>> {
        let shared = &*self.shared;
        if shared.reader_gone.load(Ordering::Acquire) {
            return Err(Returned {
                error: PushError::Disconnected,
                value,
            });
        }
        let capacity = shared.capacity();
        if capacity == 0 {
            return Err(Self::refuse(&mut self.lost_writer, shared.overflow, value));
        }
        let pos = self.head;
        let slot = &shared.slots[(pos % capacity) as usize];
        loop {
            if slot.seq.load(Ordering::Acquire) == 2 * pos {
                // SAFETY: `seq == 2 * pos`: the slot's last value was taken and
                // its taker finished (its `Release` store, loaded above), and
                // this is the only writer.
                unsafe { slot.value.write(value) };
                slot.seq.store(2 * pos + 1, Ordering::Release);
                self.head = pos + 1;
                shared.head.store(pos + 1, Ordering::Release);
                return Ok(());
            }
            // Not free. Evict only when the ring is logically full: if a take
            // already claimed the oldest value, its read is what is in flight
            // and evicting more would lose a value the ring has room for.
            let full = pos - shared.tail.load(Ordering::Acquire) >= capacity;
            if shared.overflow != Overflow::Evict || !full {
                return Err(Self::refuse(&mut self.lost_writer, shared.overflow, value));
            }
            match shared.take() {
                Some(oldest) => {
                    self.lost_writer.add(1);
                    drop(oldest);
                }
                None => return Err(Self::refuse(&mut self.lost_writer, shared.overflow, value)),
            }
        }
    }

    fn refuse(lost: &mut CounterWriter<u64>, overflow: Overflow, value: T) -> Returned<T> {
        if overflow != Overflow::Refuse {
            lost.add(1);
        }
        Returned {
            error: PushError::Full,
            value,
        }
    }

    /// Pushes each of `values` as [`push`](Self::push) does, and returns how
    /// many were queued. A refused value is not retried and the rest are still
    /// tried, so a `DropNewest` ring counts one loss per refused value.
    pub fn push_slice(&mut self, values: &[T]) -> usize
    where
        T: Copy,
    {
        values.iter().filter(|v| self.push(**v).is_ok()).count()
    }

    /// How many more values fit now: at least this many, more if the reader
    /// takes meanwhile.
    #[must_use]
    pub fn vacant(&self) -> usize {
        let held = self.head - self.shared.tail.load(Ordering::Acquire);
        (self.shared.capacity() - held.min(self.shared.capacity())) as usize
    }

    /// How many values are queued now: at most this many, fewer if the reader
    /// takes meanwhile.
    #[must_use]
    pub fn len(&self) -> usize {
        (self.shared.capacity() as usize) - self.vacant()
    }

    /// Whether nothing is queued now.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether [`vacant`](Self::vacant) is zero.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.vacant() == 0
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

    /// Values the ring has lost since it was made, cumulative (see
    /// [`Overflow`] for what counts).
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost.get()
    }

    /// Values lost since this end's last `take_lost`, and resets that count;
    /// [`lost`](Self::lost) is unaffected.
    pub fn take_lost(&mut self) -> u64 {
        let now = self.lost();
        now - std::mem::replace(&mut self.taken_lost, now)
    }

    /// A view of the loss count that outlives the end.
    #[must_use]
    pub fn losses(&self) -> Counter<u64> {
        self.lost.clone()
    }
}

impl<T: Send> RingReader<T> {
    /// The oldest queued value, or `None` when the ring is empty. Never
    /// allocates, locks or waits; the value is moved out, never dropped here.
    pub fn pop(&mut self) -> Option<T> {
        self.shared.take()
    }

    /// Takes the values queued now (those a later push adds are for the next
    /// call), oldest first, one per step; one left unread stays queued.
    pub fn drain(&mut self) -> Drain<'_, T> {
        Drain {
            left: self.len(),
            reader: self,
        }
    }

    /// Takes values into `out`, oldest first, as many as fit, and returns the
    /// count; the rest of `out` is as it was.
    pub fn pop_slice(&mut self, out: &mut [T]) -> usize
    where
        T: Copy,
    {
        let mut n = 0;
        for slot in out {
            let Some(value) = self.pop() else { break };
            *slot = value;
            n += 1;
        }
        n
    }

    /// Takes and drops up to `n` values; returns how many it took.
    pub fn skip(&mut self, n: usize) -> usize {
        (0..n).take_while(|_| self.pop().is_some()).count()
    }

    /// How many values are queued now: at least what a pop sees, more if the
    /// writer pushes meanwhile.
    #[must_use]
    pub fn len(&self) -> usize {
        let tail = self.shared.tail.load(Ordering::Acquire);
        let head = self.shared.head.load(Ordering::Acquire);
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

    /// The oldest queued value, left queued.
    ///
    /// # Panics
    ///
    /// On an [`Overflow::Evict`] ring, whose writer may take the value out
    /// from under a borrow.
    #[must_use]
    pub fn peek(&self) -> Option<&T> {
        self.iter().next()
    }

    /// The values queued now, oldest first, left queued.
    ///
    /// # Panics
    ///
    /// On an [`Overflow::Evict`] ring (see [`peek`](Self::peek)).
    pub fn iter(&self) -> Iter<'_, T> {
        assert!(
            self.shared.overflow != Overflow::Evict,
            "an evicting ring's values cannot be borrowed: its writer may take them"
        );
        Iter {
            shared: &self.shared,
            pos: self.shared.tail.load(Ordering::Acquire),
        }
    }

    /// Whether the writer was dropped: no value will be pushed again.
    ///
    /// **Ask before taking** ([`FaultLatch`](crate::FaultLatch)'s order): a
    /// `true` answer, then a pop that finds nothing, is the end; pop first and
    /// ask after, and a value pushed between the two is missed.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.shared.writer_gone.load(Ordering::Acquire)
    }

    /// Values the ring has lost since it was made, cumulative.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost.get()
    }

    /// Values lost since this end's last `take_lost`, and resets that count;
    /// [`lost`](Self::lost) is unaffected.
    pub fn take_lost(&mut self) -> u64 {
        let now = self.lost();
        now - std::mem::replace(&mut self.taken_lost, now)
    }

    /// A view of the loss count that outlives the end.
    #[must_use]
    pub fn losses(&self) -> Counter<u64> {
        self.lost.clone()
    }
}

/// The values a [`RingReader::drain`] takes.
pub struct Drain<'a, T> {
    reader: &'a mut RingReader<T>,
    left: usize,
}

impl<T> std::fmt::Debug for Drain<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drain")
            .field("left", &self.left)
            .finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for Iter<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Iter")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl<T: Send> Iterator for Drain<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        self.reader.pop()
    }
}

/// The values a [`RingReader::iter`] borrows.
pub struct Iter<'a, T> {
    shared: &'a Shared<T>,
    pos: u64,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        let capacity = self.shared.capacity();
        if capacity == 0 {
            return None;
        }
        let slot = &self.shared.slots[(self.pos % capacity) as usize];
        if slot.seq.load(Ordering::Acquire) != 2 * self.pos + 1 {
            return None;
        }
        self.pos += 1;
        // SAFETY: `seq == 2 * pos + 1`: the value is published, and an iterator
        // exists only on a non-evicting ring, whose one taker is the reader
        // this borrows (`pop` needs `&mut`), so nothing takes it while the
        // borrow lives; the writer does not touch an untaken slot.
        Some(unsafe { slot.value.borrow() })
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
