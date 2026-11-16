//! [`Gauge`] and [`Counter`]: a reading the audio thread writes and any thread
//! reads, as a level that moves both ways or a count that only grows.
//!
//! A node measures something (a gain reduction, a lock, a count of dropped
//! events) and the app reads it. The two shapes share one cell: the value in
//! one atomic word and a flag the writer raises when it is gone. The word is
//! its own `AtomicU64` of bits rather than a `PackedAtomic`: the unit blanket
//! reads a value through [`Reading`], not bytemuck, and the flag sits beside it.
//!
//! - A [`Gauge<U>`] is a latest value. [`GaugeWriter::set`] replaces it,
//!   [`Gauge::get`] reads it; any number of `Clone`s read the same cell.
//! - A [`Counter<U>`] is a cumulative count. [`CounterWriter::add`] adds to it
//!   and saturates; [`Counter::get`] never returns less than an earlier `get`.
//!
//! Nothing resets on read, and a reader never blocks: one load of one word.
//! The writer is not `Clone` and writes through `&mut self`, so the cell has
//! one writer at a time by type (a shared reference cannot write, and two
//! threads cannot race a read-modify-write). Writing is audio-thread safe: no
//! allocation, no lock, no wait. The cell is allocated once, on the control
//! side, by [`gauge()`] or [`counter()`].
//!
//! **Freeing.** A writer lives in its node, and a node is retired and dropped
//! on the control side, so the audio thread never drops the last handle on the
//! cell and never frees it, even when every reader went first.
//!
//! The cell knows no reading: `U` is whatever the owner of the measurement
//! defines. A gauge's `U` is a [`Reading`], which every `tutti_types` unit over a
//! float is (`Db`, `SamplePosition`, ...); an owner's own type (a
//! lock state) implements it. A counter's is a [`Count`].
//!
//! **Closed.** When the [`GaugeWriter`] or [`CounterWriter`] drops, the cell
//! holds its last value and `is_closed` answers `true`: no writer is attached,
//! and the reading stays what it was. A reader that sees the cell closed sees
//! every value the writer wrote. [`Gauge::reopen`] and [`Counter::reopen`]
//! attach a new writer to a closed cell, which starts from that value (a count
//! goes on from its total), so a closed value is final only until a reopen.
//!
//! The model in `tests/gauge_loom.rs` runs this code.

use std::marker::PhantomData;

use sync::{Arc, AtomicBool, AtomicU64, Ordering};
use tutti_types::Unit;

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::Arc;
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::Arc;
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
}

/// Plumbing for the unit blanket of [`Reading`]: a raw number as the bits of
/// one word. Not nameable outside this module; owners never implement it.
mod bits {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}

    pub trait RawBits: Copy + Sealed {
        /// The number as the low bits of a word.
        fn to_word(self) -> u64;
        /// The number whose bits `to_word` returned.
        fn from_word(word: u64) -> Self;
    }
}

use bits::RawBits;

impl RawBits for f32 {
    #[inline]
    fn to_word(self) -> u64 {
        u64::from(self.to_bits())
    }
    #[inline]
    fn from_word(word: u64) -> Self {
        debug_assert!(u32::try_from(word).is_ok(), "an f32 reading is 32 bits");
        f32::from_bits(u32::try_from(word).unwrap_or(0))
    }
}

impl RawBits for f64 {
    #[inline]
    fn to_word(self) -> u64 {
        self.to_bits()
    }
    #[inline]
    fn from_word(word: u64) -> Self {
        f64::from_bits(word)
    }
}

/// A value a [`Gauge`] holds: one that converts to and from one 64-bit word.
///
/// `from_bits(to_bits(v))` is `v`. Every `tutti_types` unit whose raw number
/// is `f32` or `f64` is a `Reading`; the owner of any other
/// measurement implements it for its own type. Do not implement it for a bare
/// number: a quantity has a unit.
pub trait Reading: Copy + Send + Sync + 'static {
    /// The value as a word.
    fn to_bits(self) -> u64;
    /// The value whose word `to_bits` returned.
    fn from_bits(bits: u64) -> Self;
}

impl<T: Unit + Send + Sync + 'static> Reading for T
where
    T::Raw: RawBits,
{
    #[inline]
    fn to_bits(self) -> u64 {
        self.to_raw().to_word()
    }
    #[inline]
    fn from_bits(bits: u64) -> Self {
        T::from_raw(T::Raw::from_word(bits))
    }
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

/// Frames, as a [`Counter`] counts them.
impl Count for tutti_types::Samples {
    #[inline]
    fn count(self) -> u64 {
        self.0 as u64
    }
    #[inline]
    fn from_count(count: u64) -> Self {
        Self(usize::try_from(count).unwrap_or(usize::MAX))
    }
}

/// Re-anchors, as a [`Counter`] counts them.
impl Count for tutti_types::Resyncs {
    #[inline]
    fn count(self) -> u64 {
        self.get()
    }
    #[inline]
    fn from_count(count: u64) -> Self {
        Self::new(count)
    }
}

/// The cell both shapes read: the value's word and the writer's gone flag.
#[derive(Debug)]
struct Shared {
    value: AtomicU64,
    /// Raised (`Release`) by the writer's drop, after its last store.
    closed: AtomicBool,
}

impl Shared {
    fn new(initial: u64) -> Arc<Self> {
        Arc::new(Self {
            value: AtomicU64::new(initial),
            closed: AtomicBool::new(false),
        })
    }

    /// Whether the writer has dropped. Read before the value, so a closed
    /// cell is read at its last value.
    #[inline]
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Takes the writer's place if it has dropped: one exchange from closed to
    /// open. The `Acquire` side pairs with the dropping writer's `Release`
    /// store of `closed`, which follows its last value store, so the new
    /// writer starts from that value; two callers race on one exchange and
    /// one wins. No store-load pair exists to fence: the exchange is the one
    /// access to the flag that decides.
    fn reopen(&self) -> bool {
        self.closed
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// A level that moves both ways, read by any thread.
///
/// `Clone`: every clone reads the same cell. A measurement is a reading the
/// node writes, never a control the app sets.
///
/// ```
/// use tutti_rt::gauge;
/// use tutti_types::Db;
///
/// let (mut writer, reduction) = gauge(Db(0.0));
/// writer.set(Db(-6.0));
/// assert_eq!(reduction.get(), Db(-6.0));
/// writer.set(Db(-3.0)); // a level moves both ways
/// assert_eq!(reduction.clone().get(), Db(-3.0));
/// ```
pub struct Gauge<U: Reading> {
    cell: Arc<Shared>,
    _reading: PhantomData<fn() -> U>,
}

impl<U: Reading> Clone for Gauge<U> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
            _reading: PhantomData,
        }
    }
}

impl<U: Reading> Gauge<U> {
    /// The latest value written, or the initial one if none was. Wait-free.
    #[inline]
    #[must_use]
    pub fn get(&self) -> U {
        U::from_bits(self.cell.value.load(Ordering::Acquire))
    }

    /// Whether no writer is attached: the value read is the last one written,
    /// final until a [`reopen`](Self::reopen).
    #[inline]
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.cell.is_closed()
    }

    /// A new writer for the cell, once the previous one has dropped; `None`
    /// while one is live (the cell is not [closed](Self::is_closed)), so there
    /// is exactly one writer at a time, and of two concurrent calls on a
    /// closed cell exactly one gets `Some`. The reading holds the last value
    /// written: it does not reset. Control side, a cheap atomic exchange.
    ///
    /// A reader that saw the cell closed has not seen it for good: a reopened
    /// cell is open again, and closes again when the new writer drops.
    ///
    /// ```
    /// use tutti_rt::gauge;
    /// use tutti_types::Db;
    ///
    /// let (mut writer, level) = gauge(Db(0.0));
    /// writer.set(Db(-6.0));
    /// assert!(level.reopen().is_none()); // the writer is live
    /// drop(writer);
    /// let mut again = level.reopen().expect("the writer dropped");
    /// assert_eq!(level.get(), Db(-6.0)); // the last value stays
    /// again.set(Db(-1.0));
    /// assert_eq!(level.get(), Db(-1.0));
    /// ```
    pub fn reopen(&self) -> Option<GaugeWriter<U>> {
        self.cell.reopen().then(|| GaugeWriter {
            cell: Arc::clone(&self.cell),
            _reading: PhantomData,
        })
    }
}

/// The writing end of a [`Gauge`]. Not `Clone`: one writer.
#[must_use]
pub struct GaugeWriter<U: Reading> {
    cell: Arc<Shared>,
    _reading: PhantomData<fn(U)>,
}

impl<U: Reading> GaugeWriter<U> {
    /// Replaces the reading. Wait-free; allocates nothing. Takes `&mut self`:
    /// the one writer is not shared.
    #[inline]
    pub fn set(&mut self, value: U) {
        self.cell.value.store(value.to_bits(), Ordering::Release);
    }
}

impl<U: Reading> Drop for GaugeWriter<U> {
    fn drop(&mut self) {
        self.cell.closed.store(true, Ordering::Release);
    }
}

/// A gauge holding `initial`, and its writer. Allocates: control side.
#[must_use = "the writer is the only way to set the reading"]
pub fn gauge<U: Reading>(initial: U) -> (GaugeWriter<U>, Gauge<U>) {
    let cell = Shared::new(initial.to_bits());
    (
        GaugeWriter {
            cell: Arc::clone(&cell),
            _reading: PhantomData,
        },
        Gauge {
            cell,
            _reading: PhantomData,
        },
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
/// let (mut writer, lost) = counter::<u64>();
/// writer.add(2);
/// writer.add(3);
/// assert_eq!(lost.get(), 5);
/// assert_eq!(lost.get(), 5); // a read takes nothing
/// ```
///
/// The writer is one: two threads cannot share it to add.
///
/// ```compile_fail,E0499
/// let (mut writer, _lost) = tutti_rt::counter::<u64>();
/// std::thread::scope(|s| {
///     s.spawn(|| writer.add(1));
///     s.spawn(|| writer.add(1));
/// });
/// ```
pub struct Counter<U: Count> {
    cell: Arc<Shared>,
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
        U::from_count(self.cell.value.load(Ordering::Acquire))
    }

    /// Whether the writer is gone: the total read is final until a
    /// [`reopen`](Self::reopen).
    #[inline]
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.cell.is_closed()
    }

    /// A new writer for the cell, once the previous one has dropped; `None`
    /// while one is live, so there is exactly one writer at a time, and of two
    /// concurrent calls on a closed cell exactly one gets `Some`. The total
    /// carries on from where the last writer left it: it stays cumulative
    /// across writers. Control side, a cheap atomic exchange.
    ///
    /// ```
    /// use tutti_rt::counter;
    ///
    /// let (mut writer, lost) = counter::<u64>();
    /// writer.add(2);
    /// assert!(lost.reopen().is_none()); // the writer is live
    /// drop(writer);
    /// let mut again = lost.reopen().expect("the writer dropped");
    /// again.add(3);
    /// assert_eq!(lost.get(), 5);
    /// ```
    pub fn reopen(&self) -> Option<CounterWriter<U>> {
        self.cell.reopen().then(|| CounterWriter {
            cell: Arc::clone(&self.cell),
            _count: PhantomData,
        })
    }
}

/// The writing end of a [`Counter`]. Not `Clone`: one writer.
#[must_use]
pub struct CounterWriter<U: Count> {
    cell: Arc<Shared>,
    _count: PhantomData<fn(U)>,
}

impl<U: Count> CounterWriter<U> {
    /// Adds `by` to the total, saturating at `u64::MAX`. Wait-free; allocates
    /// nothing. Takes `&mut self`, so the read-then-store of the word has no
    /// second writer to race and no add is lost.
    #[inline]
    pub fn add(&mut self, by: U) {
        let total = self
            .cell
            .value
            .load(Ordering::Relaxed)
            .saturating_add(by.count());
        self.cell.value.store(total, Ordering::Release);
    }
}

impl<U: Count> Drop for CounterWriter<U> {
    fn drop(&mut self) {
        self.cell.closed.store(true, Ordering::Release);
    }
}

/// A counter at zero, and its writer. Allocates: control side.
#[must_use = "the writer is the only way to count"]
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

impl<U: Reading> std::fmt::Debug for Gauge<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gauge")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl<U: Reading> std::fmt::Debug for GaugeWriter<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaugeWriter").finish_non_exhaustive()
    }
}

impl<U: Count> std::fmt::Debug for Counter<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Counter")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl<U: Count> std::fmt::Debug for CounterWriter<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CounterWriter").finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
