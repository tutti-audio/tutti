//! Typed indices: one newtype per kind of index a plan and its compiler pass
//! around, and [`Table`], a `Vec` indexed only by its own id type.
//!
//! An id is a `u32` underneath, read with `get` (the number) or `index` (the
//! position). Two ids of different kinds do not convert, so an op index cannot
//! index the values and an audio slot cannot name an event slot. An id is not
//! plain data: it has no constructor, no `Pod` or `Zeroable` impl and no
//! public conversion from a number.
//!
//! **An id is minted only by a table of its kind:** [`Table::push`] returns
//! the id of the row it appends, [`TableSlice::ids`] and
//! [`TableSlice::iter_enumerated`] walk the ids of the rows, and
//! [`TableSlice::id`] and [`TableSlice::search_by_key`] return the id of a row
//! that exists, or `None`. The one trait method that builds an id takes a
//! token only this module can make, so no other code mints one, generic or
//! not. So an id is the id of a row of *some* table of its kind, which is a
//! row of the plan's table when it came from that plan; verify does not check
//! a plan's ids against its tables, and a plan assembled from another plan's
//! ids is not covered. Runs of a list are [`Span`](crate::Span)s, made the
//! same way by the list they index.

use bytemuck::TransparentWrapper;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut, Index, IndexMut};

/// The proof that a table is minting an id: only this module can build one,
/// so no code outside it makes an id, generic or not.
pub(crate) struct Mint(usize);

/// A typed `u32` index.
pub(crate) trait Id: Copy + Eq + Ord + std::fmt::Debug {
    /// The id of the row a table is minting.
    fn mint(at: Mint) -> Self;
    /// The position this id names, for indexing a [`Table`] or a slice.
    fn index(self) -> usize;
}

macro_rules! ids {
    ($($(#[$meta:meta])* $name:ident;)*) => {$(
        $(#[$meta])*
        #[derive(
            Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug,
        )]
        pub struct $name(u32);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl $name {
            /// The raw value.
            #[must_use]
            #[allow(dead_code, reason = "the ids only the compiler names read no raw value")]
            pub const fn get(self) -> u32 {
                self.0
            }

            /// The position this id names in its table.
            #[must_use]
            pub const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl Id for $name {
            fn mint(at: Mint) -> Self {
                Self(u32::try_from(at.0).expect("a plan table past u32"))
            }

            fn index(self) -> usize {
                Self::index(self)
            }
        }
    )*};
}

ids! {
    /// An op of a plan: a position in [`Plan::ops`](crate::Plan::ops).
    OpId;
    /// An audio value under construction: a position in the audio values.
    AudioValueId;
    /// An event value under construction: a position in the event values.
    EventValueId;
    /// An audio slot: a position in the audio arena.
    AudioSlotId;
    /// An event slot: a position in the event arena.
    EventSlotId;
    /// A unit of a plan: a position in [`Plan::units`](crate::Plan::units),
    /// which is not its place in the unit store ([`UnitIdx`](crate::UnitIdx)).
    PlanUnitId;
    /// A task: a position in the plan's coarsened task list.
    TaskId;
    /// A PDC delay: a position in [`Plan::delays`](crate::Plan::delays).
    DelayId;
    /// An audio feedback: a position in
    /// [`Plan::audio_feedback`](crate::Plan::audio_feedback).
    AudioFeedbackId;
    /// An event feedback: a position in
    /// [`Plan::event_feedback`](crate::Plan::event_feedback).
    EventFeedbackId;
    /// A tap of a plan: a position in its tap table.
    PlanTapId;
    /// An event tap of a plan: a position in its event tap table.
    PlanEventTapId;
    /// A meter of a plan: a position in its meter table.
    PlanMeterId;
}

/// A `Vec<T>` that only an `I` indexes; its rows are read and written through
/// the [`TableSlice`] it derefs to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Table<I, T> {
    items: Vec<T>,
    ids: PhantomData<fn(I)>,
}

/// The id at `index`, for a test that needs one without a plan: the id of that
/// row of a table with that many rows.
#[cfg(test)]
pub(crate) fn id_at<I: Id>(index: usize) -> I {
    Table::<I, ()>::filled(index + 1, ())
        .id(index)
        .expect("a row of the table")
}

/// A slice of rows that only an `I` indexes: what a hot loop holds as a local,
/// as it held a plain slice before the ids.
#[derive(PartialEq, Eq, Debug, bytemuck::TransparentWrapper)]
#[repr(transparent)]
#[transparent(Rows<T>)]
pub(crate) struct TableSlice<I, T> {
    ids: PhantomData<fn(I)>,
    items: Rows<T>,
}

/// The rows of a [`TableSlice`]; an alias because the derive reads a bracketed
/// type wrongly.
type Rows<T> = [T];

impl<I, T> Default for Table<I, T> {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl<I, T> From<Vec<T>> for Table<I, T> {
    fn from(items: Vec<T>) -> Self {
        Self {
            items,
            ids: PhantomData,
        }
    }
}

impl<I, T> FromIterator<T> for Table<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(iter: It) -> Self {
        Self::from(iter.into_iter().collect::<Vec<T>>())
    }
}

impl<I, T> Deref for Table<I, T> {
    type Target = TableSlice<I, T>;

    fn deref(&self) -> &TableSlice<I, T> {
        TableSlice::wrap_ref(&self.items)
    }
}

impl<I, T> DerefMut for Table<I, T> {
    fn deref_mut(&mut self) -> &mut TableSlice<I, T> {
        TableSlice::wrap_mut(&mut self.items)
    }
}

impl<I: Id, T> Table<I, T> {
    /// A table of `len` copies of `row`.
    pub(crate) fn filled(len: usize, row: T) -> Self
    where
        T: Clone,
    {
        Self::from(vec![row; len])
    }

    /// Appends a row and returns its id.
    pub(crate) fn push(&mut self, row: T) -> I {
        let id = I::mint(Mint(self.items.len()));
        self.items.push(row);
        id
    }
}

impl<I, T> TableSlice<I, T> {
    /// The rows, in id order.
    pub(crate) fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// The rows, mutably, in id order.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.items
    }

    /// The row count.
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// The rows, in id order.
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, T> {
        self.items.iter()
    }

    /// The rows, mutably, in id order.
    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.items.iter_mut()
    }
}

impl<I: Id, T> TableSlice<I, T> {
    /// The row at `id`, if there is one.
    pub(crate) fn get(&self, id: I) -> Option<&T> {
        self.items.get(id.index())
    }

    /// The id of row `index`, if there is one: the one way an arithmetic
    /// position becomes an id.
    pub(crate) fn id(&self, index: usize) -> Option<I> {
        (index < self.items.len()).then(|| I::mint(Mint(index)))
    }

    /// The id of the row whose key is `key`, in a table sorted by `by`.
    pub(crate) fn search_by_key<K: Ord>(&self, key: &K, by: impl FnMut(&T) -> K) -> Option<I> {
        self.items
            .binary_search_by_key(key, by)
            .ok()
            .map(|i| I::mint(Mint(i)))
    }

    /// Every id, in order.
    pub(crate) fn ids(&self) -> impl DoubleEndedIterator<Item = I> + use<I, T> {
        (0..self.items.len()).map(|i| I::mint(Mint(i)))
    }

    /// Every row with its id, in order.
    pub(crate) fn iter_enumerated(&self) -> impl Iterator<Item = (I, &T)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, row)| (I::mint(Mint(i)), row))
    }
}

impl<I: Id, T> Index<I> for TableSlice<I, T> {
    type Output = T;

    fn index(&self, id: I) -> &T {
        &self.items[id.index()]
    }
}

impl<I: Id, T> IndexMut<I> for TableSlice<I, T> {
    fn index_mut(&mut self, id: I) -> &mut T {
        &mut self.items[id.index()]
    }
}

impl<I, T> IntoIterator for Table<I, T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'t, I, T> IntoIterator for &'t mut Table<I, T> {
    type Item = &'t mut T;
    type IntoIter = std::slice::IterMut<'t, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'t, I, T> IntoIterator for &'t Table<I, T> {
    type Item = &'t T;
    type IntoIter = std::slice::Iter<'t, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'t, I, T> IntoIterator for &'t mut TableSlice<I, T> {
    type Item = &'t mut T;
    type IntoIter = std::slice::IterMut<'t, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter_mut()
    }
}

impl<'t, I, T> IntoIterator for &'t TableSlice<I, T> {
    type Item = &'t T;
    type IntoIter = std::slice::Iter<'t, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

/// How many times a store slot has been replaced: a plan expects the unit of
/// one generation there.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct Generation(u32);

impl Generation {
    /// The generation with this raw value.
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }
}

impl AudioSlotId {
    /// The slot every unconnected audio input reads; never written.
    pub const ZERO: Self = Self(0);
}

impl EventSlotId {
    /// The slot every unconnected event input reads; never written.
    pub const EMPTY: Self = Self(0);
}
