//! [`Tally`] and [`TallyWriter`]: a [`Count`] that many writers add to.
//!
//! A [`Counter`](super::Counter) has one writer, by type, and a `reopen` that
//! fails while the old writer lives. A tally is for a count whose writers
//! overlap (two inlets of one port while a device is replugged): every
//! [`TallyWriter`] is `Clone` and adds through `&self` with one `fetch_add` per
//! word, so one writer may also be shared by reference across threads. Each add
//! is wait-free and none is lost to another writer.
//!
//! - **Not saturating.** A saturating add over many writers is a compare-and-swap
//!   loop, which is not wait-free. A word wraps after 2^64 events: 584 years at
//!   10^9 events a second.
//! - **Relaxed.** A tally publishes no data: nothing a reader may read is
//!   written before an add. Each word is read in its own coherence order, so
//!   no tally of a [`Tally::get`] is less than in an earlier `get`.
//! - **One handle for an owner.** A writer reads its own total
//!   ([`TallyWriter::get`]) and mints a reader ([`TallyWriter::tally`]), so an
//!   owner that counts and reports through `&self` holds the writer alone.
//! - **No closed flag.** The reader mints writers ([`Tally::writer`]), so
//!   "the last writer dropped" is never final. Use a [`Counter`](super::Counter)
//!   for a count with one writer that a reader must see end.
//!
//! **Freeing.** A writer held by an audio-side owner lives in its node, which
//! is retired and dropped on the control side. A writer cloned on the audio
//! thread is dropped there only while other handles to the cell live (the
//! reader, or the owner's writer), so the audio thread never drops the last
//! handle and never frees the cells.
//!
//! The tallies are independent, as a [`Counter`](super::Counter)'s are: a
//! `get` that races an add may read one word before it and the next after it.
//! Once every writer's thread has joined, `get` is exact.

use std::marker::PhantomData;

use super::{Count, Words};
use crate::sync::{Arc, AtomicU64, Ordering};

#[derive(Debug)]
struct Cells {
    words: Box<[AtomicU64]>,
}

/// A cumulative count, added to by any number of [`TallyWriter`]s and read by
/// any thread. `Clone`: every clone reads and mints for the same cell.
///
/// ```
/// use tutti_rt::tally;
///
/// let lost = tally::<u64>();
/// let (a, b) = (lost.writer(), lost.writer());
/// a.add(2);
/// b.add(3);
/// assert_eq!(lost.get(), 5);
/// ```
pub struct Tally<U: Count> {
    cells: Arc<Cells>,
    _count: PhantomData<fn() -> U>,
}

/// A writing end of a [`Tally`]: `Clone`, one per owner, shared by reference
/// among the threads that add through it.
#[must_use]
pub struct TallyWriter<U: Count> {
    cells: Arc<Cells>,
    _count: PhantomData<fn(U)>,
}

/// A tally at zero. Allocates: control side.
#[must_use]
pub fn tally<U: Count>() -> Tally<U> {
    let len = <U::Words as Words>::LEN;
    Tally {
        cells: Arc::new(Cells {
            words: (0..len).map(|_| AtomicU64::new(0)).collect(),
        }),
        _count: PhantomData,
    }
}

impl<U: Count> Clone for Tally<U> {
    fn clone(&self) -> Self {
        Self {
            cells: Arc::clone(&self.cells),
            _count: PhantomData,
        }
    }
}

impl<U: Count> Clone for TallyWriter<U> {
    fn clone(&self) -> Self {
        Self {
            cells: Arc::clone(&self.cells),
            _count: PhantomData,
        }
    }
}

impl<U: Count> Tally<U> {
    /// The total counted so far. No tally is less than in an earlier `get`
    /// until it wraps.
    /// Wait-free: one load per word; exact once every writer has finished.
    #[inline]
    #[must_use]
    pub fn get(&self) -> U {
        read(&self.cells)
    }

    /// A new writer for the cell. It clones one `Arc`, so mint writers on the
    /// control side and add through them from any thread.
    pub fn writer(&self) -> TallyWriter<U> {
        TallyWriter {
            cells: Arc::clone(&self.cells),
            _count: PhantomData,
        }
    }
}

impl<U: Count> TallyWriter<U> {
    /// Adds `by` to the total, one `fetch_add` per word, wrapping at 2^64.
    /// Wait-free; allocates nothing. `&self`: writers on any number of threads
    /// add at once, through one shared writer or several.
    #[inline]
    pub fn add(&self, by: U) {
        let by = by.to_words();
        for (add, cell) in by.as_slice().iter().zip(&*self.cells.words) {
            cell.fetch_add(*add, Ordering::Relaxed);
        }
    }

    /// The total counted so far, as [`Tally::get`] reads it: an owner that
    /// counts and reports through `&self` holds the writer alone.
    #[inline]
    #[must_use]
    pub fn get(&self) -> U {
        read(&self.cells)
    }

    /// A reader for the same cell, to hand to whoever reports the count. It
    /// clones one `Arc`: control side.
    pub fn tally(&self) -> Tally<U> {
        Tally {
            cells: Arc::clone(&self.cells),
            _count: PhantomData,
        }
    }
}

/// The total in `cells`: one relaxed load per word.
#[inline]
fn read<U: Count>(cells: &Cells) -> U {
    let mut words = U::Words::zeroed();
    for (word, cell) in words.as_mut_slice().iter_mut().zip(&*cells.words) {
        *word = cell.load(Ordering::Relaxed);
    }
    U::from_words(words)
}

impl<U: Count> std::fmt::Debug for Tally<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tally").finish_non_exhaustive()
    }
}

impl<U: Count> std::fmt::Debug for TallyWriter<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TallyWriter").finish_non_exhaustive()
    }
}
