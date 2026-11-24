//! [`ring()`]: a bounded single-producer single-consumer queue of any `Send`
//! value, its full-ring policy chosen at construction and its losses counted
//! by the ring.
//!
//! # The protocol
//!
//! The reader is the only taker, so the queue is a classic head/tail ring over
//! packed slots: a push or a pop is a few plain loads and one `Release` store,
//! wait-free, and `push_slice`/`pop_slice`/`push_iter`/`pop_each` move a run
//! with one publish. Values can be borrowed in place ([`RingReader::peek`],
//! [`RingReader::iter`]).
//!
//! # The policy
//!
//! [`Overflow`] says what a push into a full ring does:
//!
//! - **`DropNewest`**: the pushed value is refused and counted lost; what is
//!   queued stays. The value comes back in the error, so a caller whose value
//!   must not be freed on this thread (a `Box`) can send it elsewhere.
//! - **`Refuse`**: the value is refused and *not* counted: the caller owns the
//!   decision (retry later, count it in its own unit, or treat it as
//!   impossible by a credit count).
//!
//! # Where values are freed
//!
//! A value is freed by whoever pops it, or, if it is still queued when the
//! ring is dropped, by the reader's drop. The writer's drop frees nothing; the
//! last end to drop frees a value only when a push raced the reader's drop and
//! landed after its drain.
//!
//! **Contract:** an end that can hold queued boxes is dropped off the audio
//! thread. Dropping a reader with values queued, or the last end after such a
//! race, frees them on the dropping thread, so the audio-side end of a ring of
//! `Box`es goes with its owner (the executor, the pool, the engine) on the
//! control side. A drop that frees a value inside an audio block panics in
//! debug builds ([`RtScope::check_outside`](crate::RtScope::check_outside)).
//!
//! # Counting
//!
//! Positions are `u64` counts that only grow; at a billion pushes a second
//! they last centuries, so they are never wrapped or compared modulo. The loss
//! count is a [`Counter`] with the writer as its one writer: only a push
//! loses a value.
//!
//! The models in `tests/ring_loom.rs` run this code.

use std::cell::Cell as StdCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;

use crate::gauge::{Counter, CounterWriter, counter};
use crate::scope::RtScope;

mod plain;

pub use plain::{Drain, Iter, RingReader, RingWriter, ring};

/// What a push into a full [`ring()`] does. Chosen when the ring is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Overflow {
    /// Refuse the pushed value and count it lost.
    DropNewest,
    /// Refuse the pushed value without counting it: the caller decides what a
    /// full ring means.
    Refuse,
}

/// Why a push did not queue its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum PushError {
    /// The ring was full.
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

/// Marks an end `Send` but not `Sync`: one thread at a time uses it, which is
/// what lets it borrow a queued value without `T: Sync`.
type NotSync = PhantomData<StdCell<()>>;

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
    /// The slot is free and the caller is the only writer.
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
    /// The slot holds a published value and the caller is its only taker: no
    /// other access runs until the slot is released.
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
    /// lives.
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

/// An atomic on its own cache line, so the writer's and the reader's indices
/// do not share one.
#[repr(align(64))]
struct Padded<A>(A);

/// The writer's side of the loss count: the one `CounterWriter` and a reading
/// handle.
struct WriterLoss {
    writer: CounterWriter<u64>,
    view: Counter<u64>,
}

impl WriterLoss {
    fn new() -> (Self, Counter<u64>) {
        let (writer, view) = counter::<u64>();
        (
            Self {
                writer,
                view: view.clone(),
            },
            view,
        )
    }

    fn add(&mut self, by: u64) {
        if by > 0 {
            self.writer.add(by);
        }
    }

    fn lost(&self) -> u64 {
        self.view.get()
    }
}

/// Whether dropping a queued value would free: debug builds check the thread
/// is outside the audio callback before a drop that does.
fn check_free_outside<T>() {
    if std::mem::needs_drop::<T>() {
        RtScope::check_outside("a ring's queued values");
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
