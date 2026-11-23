//! A node's published values: the cells a session writes at `update()` and the
//! node reads once per block through a [`Reader`].

use std::any::Any;
use std::sync::Arc;

use tutti_rt::{Loan, Publisher, Reader, Reclaim};

/// Which published value of a node a cell is: the vocabulary the session keys
/// a node's values by ([`HasPublished`]). Each kind's value type is fixed by
/// the session's table of kinds, so a node declares a kind with that type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum PublishedKind {
    /// The events a MIDI clip node plays.
    ClipEvents,
    /// The chord and scale changes a harmony node sends.
    Harmony,
    /// Where a sampler voice sits on the transport.
    Window,
    /// A sampler voice's loop.
    Looping,
    /// Which way a sampler voice plays.
    Direction,
    /// A sampler voice's varispeed.
    Speed,
}

impl PublishedKind {
    /// The kind a [`HasPublished`] constant names, or `None` for a number no
    /// kind has.
    #[must_use]
    pub const fn new(kind: u16) -> Option<Self> {
        Some(match kind {
            0 => Self::ClipEvents,
            1 => Self::Harmony,
            2 => Self::Window,
            3 => Self::Looping,
            4 => Self::Direction,
            5 => Self::Speed,
            _ => return None,
        })
    }
}

/// That the node `Self` publishes a value of the kind `K` (a
/// [`PublishedKind`] as `u16`), whose value is `Value`. What a typed key's
/// method for that kind needs, and what [`declare_params!`](crate::declare_params)
/// writes from a `published` entry.
///
/// A kind's `Value` is fixed by the session's table of kinds: a node that
/// declares another type for a kind gets no key method for it, and its
/// insertion into a session panics, naming the kind and both types.
pub trait HasPublished<const K: u16> {
    /// The value the key's target reads and writes.
    type Value: Clone + PartialEq + Send + Sync + 'static;
}

/// A cell the control side reads and replaces one published value through.
/// Control thread only: the session calls it under its value lock.
pub trait Published: Send + Sync + 'static {
    /// The value in force.
    type Value: Clone + PartialEq + Send + Sync + 'static;

    /// The value now.
    fn current(&self) -> Self::Value;

    /// Publishes `value`; an equal one may change nothing. Whether it was
    /// taken: a refusal leaves the value pending for the next update.
    fn replace(&self, value: &Self::Value) -> bool;
}

/// A published `T`, shared as an `Arc`: an equal value changes nothing.
impl<T: PartialEq + Send + Sync + 'static> Published for Publisher<T> {
    type Value = Arc<T>;

    fn current(&self) -> Arc<T> {
        Self::current(self)
    }

    fn replace(&self, value: &Arc<T>) -> bool {
        let current = Self::current(self);
        if !Arc::ptr_eq(&current, value) && *current != **value {
            self.publish(Arc::clone(value));
        }
        true
    }
}

/// A published `Copy` value, read and written as the value itself.
#[derive(Clone, Debug)]
pub struct Copied<T>(pub Publisher<T>);

impl<T: Copy + PartialEq + Send + Sync + 'static> Published for Copied<T> {
    type Value = T;

    fn current(&self) -> T {
        *self.0.current()
    }

    fn replace(&self, value: &T) -> bool {
        if *self.0.current() != *value {
            self.0.publish(*value);
        }
        true
    }
}

/// One published cell of a node as `NodeParts` carries it: the kind, the type
/// the node declared for it, and the cell, an `Arc<dyn Published<Value = V>>`
/// behind `Any`.
pub struct ErasedPublished {
    /// Which value the cell is.
    pub kind: PublishedKind,
    /// The name of the value type the node declared (for a refusal's text).
    pub declared: &'static str,
    /// The `Arc<dyn Published<Value = V>>` itself.
    pub cell: Arc<dyn Any + Send + Sync>,
}

impl ErasedPublished {
    pub(crate) fn new<N: HasPublished<K>, const K: u16>(
        cell: Arc<dyn Published<Value = N::Value>>,
    ) -> Self {
        let kind = const {
            match PublishedKind::new(K) {
                Some(kind) => kind,
                None => panic!("`HasPublished<K>` names no `PublishedKind`"),
            }
        };
        Self {
            kind,
            declared: std::any::type_name::<N::Value>(),
            cell: Arc::new(cell),
        }
    }
}

impl std::fmt::Debug for ErasedPublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedPublished")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// What `declare_params!`'s `published` entry holds: the publisher and the
/// node's reader of it. A clone shares the publisher and registers a reader of
/// its own, on the control side where it is made.
pub struct PublishedCell<T> {
    publisher: Publisher<T>,
    reader: Reader<T>,
}

impl<T> PublishedCell<T> {
    /// A cell holding `value`.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::over(Publisher::new(value))
    }

    fn over(publisher: Publisher<T>) -> Self {
        let reader = publisher.reader();
        Self { publisher, reader }
    }

    /// A cell that shares nothing with this one, holding the value this holds
    /// now.
    #[must_use]
    pub fn detached(&self) -> Self {
        Self::over(Publisher::from_arc(self.publisher.current()))
    }

    /// The value for this block. Audio thread: one loan per block, never kept
    /// across blocks.
    pub fn lend(&mut self) -> Loan<'_, T> {
        self.reader.lend()
    }

    /// The publisher, for the session's cell and the reclaimer.
    #[must_use]
    pub fn publisher(&self) -> &Publisher<T> {
        &self.publisher
    }
}

impl<T> Clone for PublishedCell<T> {
    fn clone(&self) -> Self {
        Self::over(self.publisher.clone())
    }
}

impl<T> std::fmt::Debug for PublishedCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedCell").finish_non_exhaustive()
    }
}

/// The reclaimer of a node's published cells: each one's publisher, swept on
/// the owner's tick.
#[derive(Default)]
pub struct Sweep(Vec<Box<dyn Reclaim + Send>>);

impl Sweep {
    /// A sweep of nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// This sweep and `cell`'s publisher.
    #[must_use]
    pub fn with<T: Send + Sync + 'static>(mut self, cell: &PublishedCell<T>) -> Self {
        self.0.push(Box::new(cell.publisher.clone()));
        self
    }

    /// Whether it sweeps nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Reclaim for Sweep {
    fn reclaim(&self) -> usize {
        self.0.iter().map(|each| each.reclaim()).sum()
    }
}

impl std::fmt::Debug for Sweep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sweep").finish_non_exhaustive()
    }
}
