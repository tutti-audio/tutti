//! [`Gauge`] and [`Counter`]: a reading the audio thread writes and any thread
//! reads, as a level that moves both ways or a count that only grows.
//!
//! A node measures something (a gain reduction, a lock, a count of dropped
//! events) and the app reads it. The two shapes share one cell: the value in
//! one word ([`PackedAtomic`]) and a flag the writer raises when it is gone.
//!
//! - A [`Gauge<U>`] is a latest value. [`GaugeWriter::set`] replaces it,
//!   [`Gauge::get`] reads it; any number of `Clone`s read the same cell.
//! - A [`Counter<U>`] is a cumulative count. [`CounterWriter::add`] adds to it
//!   and saturates; [`Counter::get`] never returns less than an earlier `get`.
//!
//! Nothing resets on read, and a reader never blocks: one load of one word.
//! The writer is not `Clone`, so the cell has one writer, and it is
//! audio-thread safe: no allocation, no lock, no wait. The cell is allocated
//! once, on the control side, by [`gauge()`] or [`counter()`].
//!
//! The cell knows no reading: `U` is whatever the owner of the measurement
//! defines. A gauge's `U` is a plain value of at most eight bytes ([`NoUninit`]
//! and [`AnyBitPattern`], as [`PackedAtomic`]'s); a counter's is a [`Count`].
//!
//! **Closed.** When the [`GaugeWriter`] or [`CounterWriter`] drops, the cell
//! holds its last value and `is_closed` answers `true`: the subject is gone,
//! and the reading stays what it was. A reader that sees the cell closed sees
//! every value the writer wrote.
//!
//! The model in `tests/gauge_loom.rs` runs this code.

use std::marker::PhantomData;

use bytemuck::{AnyBitPattern, NoUninit};
use sync::{Arc, AtomicBool, Ordering};

use crate::PackedAtomic;

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::Arc;
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::Arc;
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, Ordering};
}

/// A quantity a [`Counter`] counts: a whole number of something.
///
/// `count` and `from_count` are inverse: `U::from_count(u.count()) == u`. The
/// owner of a measurement implements it for the type that names what is
/// counted (a count of events, of frames), so a `Counter<U>` reads back a `U`
/// and not a bare number.
pub trait Count: Copy + Send + 'static {
    /// The number of whole units.
    fn count(self) -> u64;
    /// `count` units.
    fn from_count(count: u64) -> Self;
}

impl Count for u64 {
    #[inline]
    fn count(self) -> u64 {
        self
    }
    #[inline]
    fn from_count(count: u64) -> Self {
        count
    }
}

/// The cell both shapes read: the value and the writer's gone flag.
#[derive(Debug)]
struct Shared<W: NoUninit + AnyBitPattern> {
    value: PackedAtomic<W>,
    /// Raised (`Release`) by the writer's drop, after its last store.
    closed: AtomicBool,
}

impl<W: NoUninit + AnyBitPattern> Shared<W> {
    fn new(initial: W) -> Arc<Self> {
        Arc::new(Self {
            value: PackedAtomic::new(initial),
            closed: AtomicBool::new(false),
        })
    }

    /// Whether the writer has dropped. Read before the value, so a closed
    /// cell is read at its final value.
    #[inline]
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// A level that moves both ways, read by any thread.
///
/// `Clone`: every clone reads the same cell. A measurement is a reading the
/// node writes, never a control the app sets.
///
/// ```
/// use tutti_rt::gauge;
///
/// let (writer, reading) = gauge(0.0f32);
/// writer.set(-6.0);
/// assert_eq!(reading.get(), -6.0);
/// writer.set(-3.0); // a level moves both ways
/// assert_eq!(reading.clone().get(), -3.0);
/// ```
#[derive(Debug)]
pub struct Gauge<U: NoUninit + AnyBitPattern> {
    cell: Arc<Shared<U>>,
}

impl<U: NoUninit + AnyBitPattern> Clone for Gauge<U> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
        }
    }
}

impl<U: NoUninit + AnyBitPattern> Gauge<U> {
    /// The latest value written, or the initial one if none was. Wait-free.
    #[inline]
    #[must_use]
    pub fn get(&self) -> U {
        self.cell.value.load()
    }

    /// Whether the writer is gone: the value read is the last it wrote.
    #[inline]
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.cell.is_closed()
    }
}

/// The writing end of a [`Gauge`]. Not `Clone`: one writer.
#[derive(Debug)]
#[must_use]
pub struct GaugeWriter<U: NoUninit + AnyBitPattern> {
    cell: Arc<Shared<U>>,
}

impl<U: NoUninit + AnyBitPattern> GaugeWriter<U> {
    /// Replaces the reading. Wait-free; allocates nothing.
    #[inline]
    pub fn set(&self, value: U) {
        self.cell.value.store(value);
    }
}

impl<U: NoUninit + AnyBitPattern> Drop for GaugeWriter<U> {
    fn drop(&mut self) {
        self.cell.closed.store(true, Ordering::Release);
    }
}

/// A gauge holding `initial`, and its writer. Allocates: control side.
pub fn gauge<U: NoUninit + AnyBitPattern>(initial: U) -> (GaugeWriter<U>, Gauge<U>) {
    let cell = Shared::new(initial);
    (
        GaugeWriter {
            cell: Arc::clone(&cell),
        },
        Gauge { cell },
    )
}

/// A cumulative count, read by any thread. It only grows.
///
/// `Clone`: every clone reads the same cell. Nothing resets on read, so a
/// reader that wants a delta keeps the last value it saw.
///
/// ```
/// use tutti_rt::counter;
///
/// let (writer, lost) = counter::<u64>();
/// writer.add(2);
/// writer.add(3);
/// assert_eq!(lost.get(), 5);
/// assert_eq!(lost.get(), 5); // a read takes nothing
/// ```
#[derive(Debug)]
pub struct Counter<U: Count> {
    cell: Arc<Shared<u64>>,
    _count: PhantomData<fn() -> U>,
}

impl<U: Count> Clone for Counter<U> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
            _count: PhantomData,
        }
    }
}

impl<U: Count> Counter<U> {
    /// The total counted so far. Never less than an earlier `get`. Wait-free.
    #[inline]
    #[must_use]
    pub fn get(&self) -> U {
        U::from_count(self.cell.value.load())
    }

    /// Whether the writer is gone: the total read is final.
    #[inline]
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.cell.is_closed()
    }
}

/// The writing end of a [`Counter`]. Not `Clone`: one writer.
#[derive(Debug)]
#[must_use]
pub struct CounterWriter<U: Count> {
    cell: Arc<Shared<u64>>,
    _count: PhantomData<fn(U)>,
}

impl<U: Count> CounterWriter<U> {
    /// Adds `by` to the total, saturating at `u64::MAX`. Wait-free; allocates
    /// nothing. The one writer reads and stores the word, so no add is lost.
    #[inline]
    pub fn add(&self, by: U) {
        let total = self.cell.value.load().saturating_add(by.count());
        self.cell.value.store(total);
    }
}

impl<U: Count> Drop for CounterWriter<U> {
    fn drop(&mut self) {
        self.cell.closed.store(true, Ordering::Release);
    }
}

/// A counter at zero, and its writer. Allocates: control side.
pub fn counter<U: Count>() -> (CounterWriter<U>, Counter<U>) {
    let cell = Shared::new(0);
    (
        CounterWriter {
            cell: Arc::clone(&cell),
            _count: PhantomData,
        },
        Counter {
            cell,
            _count: PhantomData,
        },
    )
}

#[cfg(all(test, not(loom)))]
mod tests;
