//! The plain ring: a head/tail queue whose only taker is the reader.
//!
//! `head` counts values pushed (stored only by the writer) and `tail` values
//! popped (stored only by the reader); a slot is `count % capacity`.
//!
//! - **Push:** if `head - tail` (the tail the writer last saw, refreshed with
//!   an `Acquire` load only when that says full) is the capacity, the ring is
//!   full. Otherwise write the slot, then store `head + n` with `Release`.
//! - **Pop:** if `tail` equals the head the reader last saw (refreshed with
//!   an `Acquire` load only when that says empty), the ring is empty.
//!   Otherwise read the slot, then store `tail + n` with `Release`.
//!
//! The `Release` store of `head` publishes the slots to a reader that
//! `Acquire`-loads it; the `Release` store of `tail` frees them to a writer
//! that `Acquire`-loads it, after the reader finished reading. Each side reads
//! the other's index only to bound itself, so a stale read can only make the
//! ring look fuller or emptier than it is: there is no store-then-load pair
//! that needs a `SeqCst` fence.

use std::sync::Arc;

use super::{Cell, NotSync, Overflow, Padded, PushError, Returned, WriterLoss, check_free_outside};
use crate::gauge::Counter;
use crate::sync::{AtomicBool, AtomicU64, Ordering};

struct Shared<T> {
    slots: Box<[Cell<T>]>,
    /// Values pushed.
    head: Padded<AtomicU64>,
    /// Values popped.
    tail: Padded<AtomicU64>,
    writer_gone: AtomicBool,
    reader_gone: AtomicBool,
}

// SAFETY: the slot cells are the only non-`Sync` field. The one writer writes
// a slot only while `head - tail < capacity` (the reader finished with it
// before its `Release` store of `tail`, which the writer `Acquire`-loaded);
// the one reader reads a slot only while `tail < head` (the writer finished
// before its `Release` store of `head`, which the reader `Acquire`-loaded).
// The ends are `!Sync`, so each is used by one thread at a time. Values cross
// threads, hence `T: Send`.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Shared<T> {
    fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    /// The slot index of position `count`; the caller keeps a run in order
    /// with [`next`](Self::next) so a run divides once. Zero capacity has no
    /// index to name, and no run reaches here (its room is zero).
    fn start(&self, count: u64) -> usize {
        match self.capacity() {
            0 => 0,
            capacity => (count % capacity) as usize,
        }
    }

    fn next(&self, at: usize) -> usize {
        let at = at + 1;
        if at == self.slots.len() { 0 } else { at }
    }

    fn slot(&self, count: u64) -> &Cell<T> {
        &self.slots[(count % self.capacity()) as usize]
    }

    /// Takes every queued value out, freeing it here.
    fn clear(&mut self) {
        let head = self.head.0.load(Ordering::Acquire);
        let mut tail = self.tail.0.load(Ordering::Acquire);
        if tail < head {
            check_free_outside::<T>();
        }
        while tail < head {
            // SAFETY: `&mut self`: no other end exists, and `tail < head`
            // names a published, untaken slot.
            drop(unsafe { self.slot(tail).take() });
            tail += 1;
        }
        self.tail.0.store(tail, Ordering::Release);
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// A ring holding up to `capacity` values (zero holds none: every push is
/// refused), as its two ends. Allocates; control side.
///
/// ```
/// use tutti_rt::{Overflow, PushError, ring};
///
/// let (mut writer, mut reader) = ring::<u32>(2, Overflow::DropNewest);
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
///
/// An end is `Send` and not `Sync`: a reader that lends out `&T` must not be
/// shared by reference with another thread.
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<tutti_rt::RingReader<std::cell::Cell<u8>>>();
/// ```
///
/// ```compile_fail,E0277
/// fn is_sync<T: Sync>() {}
/// is_sync::<tutti_rt::RingWriter<u8>>();
/// ```
///
/// ```
/// fn is_send<T: Send>() {}
/// is_send::<tutti_rt::RingReader<std::cell::Cell<u8>>>();
/// is_send::<tutti_rt::RingWriter<std::cell::Cell<u8>>>();
/// ```
#[must_use]
pub fn ring<T: Send>(capacity: usize, overflow: Overflow) -> (RingWriter<T>, RingReader<T>) {
    let slots = (0..capacity).map(|_| Cell::new()).collect();
    let shared = Arc::new(Shared {
        slots,
        head: Padded(AtomicU64::new(0)),
        tail: Padded(AtomicU64::new(0)),
        writer_gone: AtomicBool::new(false),
        reader_gone: AtomicBool::new(false),
    });
    let (loss, view) = WriterLoss::new();
    (
        RingWriter {
            shared: Arc::clone(&shared),
            head: 0,
            tail_seen: 0,
            overflow,
            loss,
            _not_sync: NotSync::default(),
        },
        RingReader {
            shared,
            tail: 0,
            head_seen: 0,
            view,
            taken: 0,
            _not_sync: NotSync::default(),
        },
    )
}

/// The producing end of a [`ring()`]. `Send`, not `Clone`, not `Sync`.
pub struct RingWriter<T> {
    shared: Arc<Shared<T>>,
    head: u64,
    /// The reader's `tail` as last loaded.
    tail_seen: u64,
    overflow: Overflow,
    loss: WriterLoss,
    _not_sync: NotSync,
}

/// The consuming end of a [`ring()`]. `Send`, not `Clone`, not `Sync`.
pub struct RingReader<T> {
    shared: Arc<Shared<T>>,
    tail: u64,
    /// The writer's `head` as last loaded.
    head_seen: u64,
    view: Counter<u64>,
    taken: u64,
    _not_sync: NotSync,
}

impl<T> std::fmt::Debug for RingWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingWriter")
            .field("capacity", &self.shared.slots.len())
            .field("overflow", &self.overflow)
            .finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for RingReader<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingReader")
            .field("capacity", &self.shared.slots.len())
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
        let head = self.shared.head.0.load(Ordering::Acquire);
        if self.tail < head {
            // In a debug build inside an audio block this panics before any
            // value is taken, so the unwind leaves them queued for `Shared`'s
            // drop; that one runs while unwinding, where the check stands
            // down (`RtScope::check_outside`), so the panic is not doubled
            // into an abort.
            check_free_outside::<T>();
        }
        while self.tail < head {
            // SAFETY: `tail < head`: the slot is published, and this is the
            // only reader.
            let value = unsafe { self.shared.slot(self.tail).take() };
            // Published before the value drops: if its drop panics, the
            // slot is already given up and `Shared`'s drop does not take it
            // again.
            self.tail += 1;
            self.shared.tail.0.store(self.tail, Ordering::Release);
            drop(value);
        }
    }
}

impl<T: Send> RingWriter<T> {
    /// Room for `want` more values, refreshing the cached `tail` only when the
    /// cache says there is not.
    fn room(&mut self, want: u64) -> u64 {
        let capacity = self.shared.capacity();
        if capacity - (self.head - self.tail_seen) < want {
            self.tail_seen = self.shared.tail.0.load(Ordering::Acquire);
        }
        capacity - (self.head - self.tail_seen)
    }

    /// Queues `value`, or by the ring's [`Overflow`] hands it back when the
    /// ring is full. Wait-free; never allocates, locks or waits.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] (the value is handed back) when the ring is full;
    /// [`PushError::Disconnected`] once the reader is dropped.
    pub fn push(&mut self, value: T) -> Result<(), Returned<T>> {
        if self.shared.reader_gone.load(Ordering::Acquire) {
            return Err(Returned {
                error: PushError::Disconnected,
                value,
            });
        }
        if self.room(1) == 0 {
            if self.overflow == Overflow::DropNewest {
                self.loss.add(1);
            }
            return Err(Returned {
                error: PushError::Full,
                value,
            });
        }
        // SAFETY: `head - tail < capacity`: the slot is free (see `Shared`'s
        // `Sync`), and this is the only writer.
        unsafe { self.shared.slot(self.head).write(value) };
        self.head += 1;
        self.shared.head.0.store(self.head, Ordering::Release);
        Ok(())
    }

    /// Queues as many of `values` as fit, in order, with one publish, and
    /// returns how many were taken. The ones that did not fit are counted lost
    /// on a `DropNewest` ring, one each.
    pub fn push_slice(&mut self, values: &[T]) -> usize
    where
        T: Copy,
    {
        if self.shared.reader_gone.load(Ordering::Acquire) {
            return 0;
        }
        let take = self.room(values.len() as u64).min(values.len() as u64);
        let mut at = self.shared.start(self.head);
        for value in &values[..take as usize] {
            // SAFETY: `take` slots are free (see `push`), and this is the
            // only writer; the publish below follows every write.
            unsafe { self.shared.slots[at].write(*value) };
            at = self.shared.next(at);
        }
        self.head += take;
        if take > 0 {
            self.shared.head.0.store(self.head, Ordering::Release);
        }
        if self.overflow == Overflow::DropNewest {
            self.loss.add(values.len() as u64 - take);
        }
        take as usize
    }

    /// Queues values from `values`, in order, while there is room, with one
    /// publish, and returns how many it took. Stops at the first value that
    /// does not fit and leaves the rest of the iterator unread: nothing is
    /// counted lost here, so a caller that wants a loss counted checks
    /// [`vacant`](Self::vacant) first. A panic in the iterator publishes the
    /// values already written.
    pub fn push_iter(&mut self, values: impl IntoIterator<Item = T>) -> usize {
        /// Counts the values written in a local, and on drop (a panic in the
        /// iterator included) advances the writer's head past them and
        /// publishes it.
        struct Publish<'a, T> {
            writer: &'a mut RingWriter<T>,
            done: u64,
        }
        impl<T> Drop for Publish<'_, T> {
            fn drop(&mut self) {
                self.writer.head += self.done;
                self.writer
                    .shared
                    .head
                    .0
                    .store(self.writer.head, Ordering::Release);
            }
        }
        if self.shared.reader_gone.load(Ordering::Acquire) {
            return 0;
        }
        // `u64::MAX` always refreshes the cached tail: one `Acquire` load
        // for the whole run.
        let room = self.room(u64::MAX);
        let mut at = self.shared.start(self.head);
        let mut run = Publish {
            writer: self,
            done: 0,
        };
        for value in values.into_iter().take(room as usize) {
            // SAFETY: `done < room` slots are free (see `push`), and this is
            // the only writer; the head is published on drop, after every
            // write.
            unsafe { run.writer.shared.slots[at].write(value) };
            at = run.writer.shared.next(at);
            run.done += 1;
        }
        run.done as usize
    }

    /// How many more values fit now: at least this many, more if the reader
    /// takes meanwhile.
    #[must_use]
    pub fn vacant(&self) -> usize {
        let held = self.head - self.shared.tail.0.load(Ordering::Acquire);
        (self.shared.capacity() - held) as usize
    }

    /// How many values are queued now: at most this many, fewer if the reader
    /// takes meanwhile.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.slots.len() - self.vacant()
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

impl<T: Send> RingReader<T> {
    /// Values published, refreshing the cached `head` only when the cache says
    /// fewer than `want`.
    fn available(&mut self, want: u64) -> u64 {
        if self.head_seen - self.tail < want {
            self.head_seen = self.shared.head.0.load(Ordering::Acquire);
        }
        self.head_seen - self.tail
    }

    /// The oldest queued value, or `None` when the ring is empty. Wait-free;
    /// never allocates, locks or waits; the value is moved out, never dropped
    /// here.
    pub fn pop(&mut self) -> Option<T> {
        if self.available(1) == 0 {
            return None;
        }
        // SAFETY: `tail < head`: the slot is published, and this is the only
        // reader.
        let value = unsafe { self.shared.slot(self.tail).take() };
        self.tail += 1;
        self.shared.tail.0.store(self.tail, Ordering::Release);
        Some(value)
    }

    /// Takes the values queued now (those a later push adds are for the next
    /// call), oldest first, one per step; one left unread stays queued.
    pub fn drain(&mut self) -> Drain<'_, T> {
        Drain {
            left: self.available(u64::MAX),
            reader: self,
        }
    }

    /// Takes values into `out`, oldest first, as many as fit, with one
    /// publish, and returns the count; the rest of `out` is as it was.
    pub fn pop_slice(&mut self, out: &mut [T]) -> usize
    where
        T: Copy,
    {
        let take = self.available(out.len() as u64).min(out.len() as u64) as usize;
        let mut at = self.shared.start(self.tail);
        for slot in &mut out[..take] {
            // SAFETY: `tail < head` for each of the `take` slots (published),
            // and this is the only reader; the publish follows every read.
            *slot = unsafe { *self.shared.slots[at].borrow() };
            at = self.shared.next(at);
        }
        self.tail += take as u64;
        if take > 0 {
            self.shared.tail.0.store(self.tail, Ordering::Release);
        }
        take
    }

    /// Hands up to `max` queued values, oldest first, to `f`, with one
    /// publish, and returns how many it took. `f` may panic: the values it was
    /// handed stay taken.
    pub fn pop_each(&mut self, max: usize, mut f: impl FnMut(T)) -> usize {
        /// Counts the values handed out in a local, and on drop (a panic in
        /// `f` included) advances the reader's tail past them and publishes it.
        struct Publish<'a, T> {
            reader: &'a mut RingReader<T>,
            done: u64,
        }
        impl<T> Drop for Publish<'_, T> {
            fn drop(&mut self) {
                self.reader.tail += self.done;
                self.reader
                    .shared
                    .tail
                    .0
                    .store(self.reader.tail, Ordering::Release);
            }
        }
        let take = self.available(max as u64).min(max as u64);
        let mut at = self.shared.start(self.tail);
        let mut run = Publish {
            reader: self,
            done: 0,
        };
        while run.done < take {
            // SAFETY: `tail + done < head` (published), and this is the only
            // reader; the count is advanced before `f` runs and published on
            // drop.
            let value = unsafe { run.reader.shared.slots[at].take() };
            at = run.reader.shared.next(at);
            run.done += 1;
            f(value);
        }
        take as usize
    }

    /// Takes and drops up to `n` values; returns how many it took.
    pub fn skip(&mut self, n: usize) -> usize {
        (0..n).take_while(|_| self.pop().is_some()).count()
    }

    /// How many values are queued now: at least what a pop sees, more if the
    /// writer pushes meanwhile.
    #[must_use]
    pub fn len(&self) -> usize {
        (self.shared.head.0.load(Ordering::Acquire) - self.tail) as usize
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
    #[must_use]
    pub fn peek(&self) -> Option<&T> {
        self.iter().next()
    }

    /// The values queued now, oldest first, left queued.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            shared: &self.shared,
            pos: self.tail,
            end: self.shared.head.0.load(Ordering::Acquire),
            _not_send: std::marker::PhantomData,
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
        self.view.get()
    }

    /// Values lost since this end's last `take_lost`, and resets that count;
    /// [`lost`](Self::lost) is unaffected.
    pub fn take_lost(&mut self) -> u64 {
        self.view.take_since(&mut self.taken)
    }

    /// A view of the loss count that outlives the end.
    #[must_use]
    pub fn losses(&self) -> Counter<u64> {
        self.view.clone()
    }
}

/// The values a [`RingReader::drain`] takes.
pub struct Drain<'a, T> {
    reader: &'a mut RingReader<T>,
    left: u64,
}

impl<T> std::fmt::Debug for Drain<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drain")
            .field("left", &self.left)
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

/// The values a [`RingReader::iter`] borrows. Neither `Send` nor `Sync`: it
/// hands out `&T`, which another thread may only hold if `T: Sync`.
pub struct Iter<'a, T> {
    shared: &'a Shared<T>,
    pos: u64,
    end: u64,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl<T> std::fmt::Debug for Iter<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Iter")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        if self.pos >= self.end {
            return None;
        }
        let slot = self.shared.slot(self.pos);
        self.pos += 1;
        // SAFETY: `pos < end <= head` (published), and the reader's `tail`,
        // which the borrow of the reader holds fixed, is at most `pos`, so
        // the writer does not touch the slot and the reader does not take it.
        Some(unsafe { slot.borrow() })
    }
}
