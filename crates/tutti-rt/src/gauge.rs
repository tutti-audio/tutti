//! [`Gauge`] and [`Counter`]: a reading the audio thread writes and any thread
//! reads, as a level that moves both ways or a count that only grows.
//!
//! A node measures something (a gain reduction, a lock, a count of dropped
//! events) and the app reads it. The two shapes share one cell: the value in
//! one atomic word and a flag the writer raises when it is gone. The word is
//! its own `AtomicU64` of bits rather than an `AtomicCell`: the unit blanket
//! reads a value through [`Reading`], and the flag sits beside it.
//!
//! - A [`Gauge<U>`] is a latest value. [`GaugeWriter::set`] replaces it,
//!   [`Gauge::get`] reads it; any number of `Clone`s read the same cell.
//! - A [`Counter<U>`] is a cumulative count of one or several tallies
//!   ([`Count`]). [`CounterWriter::add`] adds to each and saturates it;
//!   no tally of [`Counter::get`] is less than in an earlier `get`.
//!
//! A [`Tally<U>`] is a count with many writers ([`tally()`]): `Clone` writers
//! that `fetch_add`, wrapping rather than saturating, with no closed flag.
//!
//! Nothing resets on read, and a reader never blocks: one load per word (one
//! word for a gauge).
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
//! defines. A gauge's `U` is a [`Reading`], which every `tutti_units` unit over a
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

mod tally;
pub use tally::{Tally, TallyWriter, tally};

use crate::sync::{Arc, AtomicBool, AtomicU64, Ordering};
use tutti_units::{Losses, Unit};

use crate::ControlDrop;

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
/// `from_bits(to_bits(v))` is `v`. Every `tutti_units` unit whose raw number
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

/// A [`Count`]'s tallies: `[u64; N]` with `N >= 1`, and nothing else. Sealed,
/// so a count's width is fixed by its type and the counter's cells always
/// match it.
///
/// A count of no tallies is refused when its counter is built, at compile
/// time.
///
/// ```compile_fail,E0080
/// use tutti_rt::{Count, counter};
///
/// #[derive(Clone, Copy)]
/// struct ZeroCount;
///
/// impl Count for ZeroCount {
///     type Words = [u64; 0];
///     fn to_words(self) -> [u64; 0] {
///         []
///     }
///     fn from_words(_: [u64; 0]) -> Self {
///         ZeroCount
///     }
/// }
///
/// let _ = counter::<ZeroCount>();
/// ```
///
/// A tally refuses it the same way.
///
/// ```compile_fail,E0080
/// use tutti_rt::{Count, tally};
///
/// #[derive(Clone, Copy)]
/// struct ZeroCount;
///
/// impl Count for ZeroCount {
///     type Words = [u64; 0];
///     fn to_words(self) -> [u64; 0] {
///         []
///     }
///     fn from_words(_: [u64; 0]) -> Self {
///         ZeroCount
///     }
/// }
///
/// let _ = tally::<ZeroCount>();
/// ```
pub trait Words: words::Sealed + Copy {
    /// The number of tallies: `N`, a compile error when `N` is 0.
    const LEN: usize;
    /// All tallies zero.
    fn zeroed() -> Self;
    /// The tallies as a slice of `LEN` words.
    fn as_slice(&self) -> &[u64];
    /// The tallies as a mutable slice of `LEN` words.
    fn as_mut_slice(&mut self) -> &mut [u64];
}

mod words {
    pub trait Sealed {}
    impl<const N: usize> Sealed for [u64; N] {}
}

impl<const N: usize> Words for [u64; N] {
    const LEN: usize = {
        assert!(N >= 1, "a count has at least one tally");
        N
    };
    #[inline]
    fn zeroed() -> Self {
        [0; N]
    }
    #[inline]
    fn as_slice(&self) -> &[u64] {
        self
    }
    #[inline]
    fn as_mut_slice(&mut self) -> &mut [u64] {
        self
    }
}

/// A quantity a [`Counter`] counts: one or several whole-number tallies.
///
/// A value is a fixed array of words, one per tally ([`Words`](Self::Words),
/// `[u64; N]`). The counter keeps one atomic per word and adds word by word,
/// each saturating at `u64::MAX` on its own, so a carry never crosses from
/// one tally to the next. `from_words(u.to_words()) == u`.
///
/// The tallies are independent: each only grows, but a reader that races the
/// writer may read word 0 at one instant and word 1 at a later one (see
/// [`Counter::get`]). A quantity whose words must agree at an instant is not
/// a `Count`.
///
/// The owner of a measurement implements it for the type that names what is
/// counted (a count of events, of frames), so a `Counter<U>` reads back a `U`
/// and not a bare number.
pub trait Count: Copy + Send + 'static {
    /// The tallies: `[u64; N]`, one word per tally.
    type Words: Words;
    /// The value as its tallies.
    fn to_words(self) -> Self::Words;
    /// The value whose tallies `to_words` returned.
    fn from_words(words: Self::Words) -> Self;
}

impl Count for u64 {
    type Words = [u64; 1];
    #[inline]
    fn to_words(self) -> [u64; 1] {
        [self]
    }
    #[inline]
    fn from_words([count]: [u64; 1]) -> Self {
        count
    }
}

/// Frames, as a [`Counter`] counts them.
impl Count for tutti_units::Samples {
    type Words = [u64; 1];
    #[inline]
    fn to_words(self) -> [u64; 1] {
        [self.get() as u64]
    }
    #[inline]
    fn from_words([count]: [u64; 1]) -> Self {
        Self::new(usize::try_from(count).unwrap_or(usize::MAX))
    }
}

/// Re-anchors, as a [`Counter`] counts them.
impl Count for tutti_units::Resyncs {
    type Words = [u64; 1];
    #[inline]
    fn to_words(self) -> [u64; 1] {
        [self.get()]
    }
    #[inline]
    fn from_words([count]: [u64; 1]) -> Self {
        Self::new(count)
    }
}

/// Audio blocks, as a [`Counter`] counts them.
impl Count for tutti_units::Blocks {
    type Words = [u64; 1];
    #[inline]
    fn to_words(self) -> [u64; 1] {
        [self.get()]
    }
    #[inline]
    fn from_words([count]: [u64; 1]) -> Self {
        Self::new(count)
    }
}

/// Transfer chunks, as a [`Counter`] counts them.
impl Count for tutti_units::Chunks {
    type Words = [u64; 1];
    #[inline]
    fn to_words(self) -> [u64; 1] {
        [self.get()]
    }
    #[inline]
    fn from_words([count]: [u64; 1]) -> Self {
        Self::new(count)
    }
}

/// Calls, as a [`Counter`] counts them.
impl Count for tutti_units::Calls {
    type Words = [u64; 1];
    #[inline]
    fn to_words(self) -> [u64; 1] {
        [self.get()]
    }
    #[inline]
    fn from_words([count]: [u64; 1]) -> Self {
        Self::new(count)
    }
}

/// One tally per count, in field order: a `Counter<Losses>` keeps `lost`,
/// `evicted`, `refused` and `late` as four words, each saturating on its own,
/// so a port's losses are one counter. A `Tally<Losses>` wraps per word
/// instead (see `Tally`); only the counter saturates.
impl Count for Losses {
    type Words = [u64; 4];
    #[inline]
    fn to_words(self) -> [u64; 4] {
        [self.lost, self.evicted, self.refused, self.late]
    }
    #[inline]
    fn from_words([lost, evicted, refused, late]: [u64; 4]) -> Self {
        // `Losses` is `non_exhaustive`: built one count at a time.
        Losses::lost(lost)
            + Losses::evicted(evicted)
            + Losses::refused(refused)
            + Losses::late(late)
    }
}

/// The cell both shapes read: the value's word and the writer's gone flag.
#[derive(Debug)]
struct Shared {
    /// Word 0 of the value: a gauge's whole value, a counter's first tally.
    value: AtomicU64,
    /// A counter's tallies after the first; empty for a gauge and a one-word
    /// count. Allocated once, with the cell.
    rest: Box<[AtomicU64]>,
    /// Raised (`Release`) by the writer's drop, after its last store.
    closed: AtomicBool,
}

impl Shared {
    fn new(initial: u64, extra_words: usize) -> Arc<Self> {
        Arc::new(Self {
            value: AtomicU64::new(initial),
            rest: (0..extra_words).map(|_| AtomicU64::new(0)).collect(),
            closed: AtomicBool::new(false),
        })
    }

    /// Word `i` of the value: `value` for 0, then `rest`.
    #[inline]
    fn word(&self, i: usize) -> &AtomicU64 {
        match i.checked_sub(1) {
            None => &self.value,
            Some(j) => &self.rest[j],
        }
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
/// use tutti_units::Db;
///
/// let (mut writer, reduction) = gauge(Db::new(0.0));
/// writer.set(Db::new(-6.0));
/// assert_eq!(reduction.get(), Db::new(-6.0));
/// writer.set(Db::new(-3.0)); // a level moves both ways
/// assert_eq!(reduction.clone().get(), Db::new(-3.0));
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
    /// use tutti_units::Db;
    ///
    /// let (mut writer, level) = gauge(Db::new(0.0));
    /// writer.set(Db::new(-6.0));
    /// assert!(level.reopen().is_none()); // the writer is live
    /// drop(writer);
    /// let mut again = level.reopen().expect("the writer dropped");
    /// assert_eq!(level.get(), Db::new(-6.0)); // the last value stays
    /// again.set(Db::new(-1.0));
    /// assert_eq!(level.get(), Db::new(-1.0));
    /// ```
    pub fn reopen(&self) -> Option<GaugeWriter<U>> {
        self.cell.reopen().then(|| GaugeWriter {
            _control: ControlDrop::new(),
            cell: Arc::clone(&self.cell),
            _reading: PhantomData,
        })
    }
}

/// The writing end of a [`Gauge`]. Not `Clone`: one writer.
#[must_use]
pub struct GaugeWriter<U: Reading> {
    _control: ControlDrop<Self>,
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
    let cell = Shared::new(initial.to_bits(), 0);
    (
        GaugeWriter {
            _control: ControlDrop::new(),
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
    /// The total counted so far. No tally is ever less than in an earlier
    /// `get`. Wait-free: one load per word.
    ///
    /// Each word is read on its own, in order, so a read that races the
    /// writer may pair word 0 from before an `add` with a later word from
    /// after it. Every word is a real total of its own tally; none is torn.
    /// `get` does not load the `closed` flag: a reader that wants the final
    /// total reads [`is_closed`](Self::is_closed) first, and once that is
    /// `true` the `get` that follows reads every tally whole (the writer's
    /// last store precedes its `Release` store of `closed`).
    #[inline]
    #[must_use]
    pub fn get(&self) -> U {
        let mut words = U::Words::zeroed();
        for (i, word) in words.as_mut_slice().iter_mut().enumerate() {
            *word = self.cell.word(i).load(Ordering::Acquire);
        }
        U::from_words(words)
    }

    /// What was counted since `seen`, which becomes the total now: the reader
    /// keeps its own baseline, so the count stays cumulative and every clone
    /// reads it whole. Each tally is the difference on its own, `0` where
    /// `seen` is ahead (a baseline from another counter). Wait-free, as
    /// [`get`](Self::get).
    ///
    /// ```
    /// use tutti_rt::counter;
    ///
    /// let (mut writer, lost) = counter::<u64>();
    /// let mut seen = 0;
    /// writer.add(2);
    /// assert_eq!(lost.take_since(&mut seen), 2);
    /// writer.add(3);
    /// assert_eq!(lost.take_since(&mut seen), 3);
    /// assert_eq!(lost.take_since(&mut seen), 0);
    /// assert_eq!(lost.get(), 5); // the total is untouched
    /// ```
    #[inline]
    pub fn take_since(&self, seen: &mut U) -> U {
        let now = self.get().to_words();
        let mut since = now;
        for (d, s) in since
            .as_mut_slice()
            .iter_mut()
            .zip(seen.to_words().as_slice())
        {
            *d = d.saturating_sub(*s);
        }
        *seen = U::from_words(now);
        U::from_words(since)
    }

    /// Whether the writer is gone: the total read is final until a
    /// [`reopen`](Self::reopen). A `get` after this answers `true` reads
    /// every tally the writer wrote; a `get` before it may read mid-run.
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
            _control: ControlDrop::new(),
            cell: Arc::clone(&self.cell),
            _count: PhantomData,
        })
    }
}

/// The writing end of a [`Counter`]. Not `Clone`: one writer.
#[must_use]
pub struct CounterWriter<U: Count> {
    _control: ControlDrop<Self>,
    cell: Arc<Shared>,
    _count: PhantomData<fn(U)>,
}

impl<U: Count> CounterWriter<U> {
    /// Adds `by` to the total, each tally saturating at `u64::MAX` on its own.
    /// Wait-free; allocates nothing. Takes `&mut self`, so the read-then-store
    /// of a word has no second writer to race and no add is lost.
    #[inline]
    pub fn add(&mut self, by: U) {
        let by = by.to_words();
        for (i, add) in by.as_slice().iter().enumerate() {
            let word = self.cell.word(i);
            let total = word.load(Ordering::Relaxed).saturating_add(*add);
            // Relaxed: this word's coherence order keeps each tally monotonic
            // for a reader, and the `Release` store of `closed` in the drop
            // publishes every word to a reader that saw it closed.
            word.store(total, Ordering::Relaxed);
        }
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
    let cell = Shared::new(0, <U::Words as Words>::LEN - 1);
    (
        CounterWriter {
            _control: ControlDrop::new(),
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
