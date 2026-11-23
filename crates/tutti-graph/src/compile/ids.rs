//! Typed indices: one newtype per kind of index a plan and its compiler pass
//! around, and [`Table`], a `Vec` indexed only by its own id type.
//!
//! An id is a `u32` underneath (`repr(transparent)`, so a slice of ids casts to
//! a slice of `u32` for free), and the one place a `u32` becomes a `usize` is
//! [`Id::index`]. Two ids of different kinds do not convert, so an op index
//! cannot index the values and an audio slot cannot name an event slot.

use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// A typed `u32` index.
pub(crate) trait Id: bytemuck::Pod + Eq + Ord + std::fmt::Debug {
    /// The id with this raw value.
    fn new(raw: u32) -> Self;
    /// The raw value.
    fn get(self) -> u32;
    /// The position this id names, for indexing a [`Table`] or a slice.
    fn index(self) -> usize;
    /// The id at `index`; debug-asserts that it fits.
    fn from_index(index: usize) -> Self;
}

macro_rules! ids {
    ($($(#[$meta:meta])* $name:ident;)*) => {$(
        $(#[$meta])*
        #[derive(
            Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug,
            bytemuck::Pod, bytemuck::Zeroable,
        )]
        #[repr(transparent)]
        pub(crate) struct $name(u32);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl Id for $name {
            fn new(raw: u32) -> Self {
                Self(raw)
            }

            fn get(self) -> u32 {
                self.0
            }

            fn index(self) -> usize {
                self.0 as usize
            }

            fn from_index(index: usize) -> Self {
                Self(u32::try_from(index).expect("a plan table past u32"))
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
    /// An audio feedback: a position in the audio
    /// [`Plan::feedback`](crate::Plan::feedback).
    AudioFeedbackId;
    /// An event feedback: a position in the event
    /// [`Plan::feedback`](crate::Plan::feedback).
    EventFeedbackId;
    /// A tap of a plan: a position in its tap table.
    PlanTapId;
    /// An event tap of a plan: a position in its event tap table.
    PlanEventTapId;
    /// A meter of a plan: a position in its meter table.
    PlanMeterId;
}

impl AudioFeedbackId {
    /// The audio slot this feedback is read from: slot 0 is the null slot,
    /// the feedback slots follow.
    pub(crate) fn slot(self) -> AudioSlotId {
        AudioSlotId::from_index(1 + self.index())
    }
}

impl EventFeedbackId {
    /// The event slot this feedback is read from.
    pub(crate) fn slot(self) -> EventSlotId {
        EventSlotId::from_index(1 + self.index())
    }
}

/// A `Vec<T>` that only an `I` indexes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Table<I, T> {
    items: Vec<T>,
    ids: PhantomData<fn(I)>,
}

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

impl<I, T> Table<I, T> {
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
        let id = I::from_index(self.items.len());
        self.items.push(row);
        id
    }

    /// The row at `id`, if there is one.
    pub(crate) fn get(&self, id: I) -> Option<&T> {
        self.items.get(id.index())
    }

    /// The row at `id`, mutably, if there is one.
    pub(crate) fn get_mut(&mut self, id: I) -> Option<&mut T> {
        self.items.get_mut(id.index())
    }

    /// Every id, in order.
    pub(crate) fn ids(&self) -> impl DoubleEndedIterator<Item = I> + use<I, T> {
        (0..self.items.len()).map(I::from_index)
    }

    /// Every row with its id, in order.
    pub(crate) fn iter_enumerated(&self) -> impl Iterator<Item = (I, &T)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, row)| (I::from_index(i), row))
    }
}

impl<I: Id, T> Index<I> for Table<I, T> {
    type Output = T;

    fn index(&self, id: I) -> &T {
        &self.items[id.index()]
    }
}

impl<I: Id, T> IndexMut<I> for Table<I, T> {
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
        self.items.iter_mut()
    }
}

impl<'t, I, T> IntoIterator for &'t Table<I, T> {
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
    pub(crate) const ZERO: Self = Self(0);
}

impl EventSlotId {
    /// The slot every unconnected event input reads; never written.
    pub(crate) const EMPTY: Self = Self(0);

    /// The feedback this slot is read from: the null slot is none, and the
    /// feedback slots follow it (see [`EventFeedbackId::slot`]).
    pub(crate) fn feedback(self) -> Option<EventFeedbackId> {
        (self != Self::EMPTY).then(|| EventFeedbackId::from_index(self.index() - 1))
    }
}

/// A slice of ids as the raw `u32`s they wrap: what a public accessor returns
/// while the plan's public structs keep raw fields.
pub(crate) fn raw<I: Id>(ids: &[I]) -> &[u32] {
    bytemuck::cast_slice(ids)
}
