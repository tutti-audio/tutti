//! A node's published values: the cells a session writes at `update()` and the
//! node reads once per block through a [`Reader`].

use std::any::{Any, TypeId};
use std::sync::Arc;

use tutti_rt::{Loan, Publisher, Reader, Reclaim};

/// Which published value of a node a cell is, and the type it holds: the
/// vocabulary a session addresses a node's values by. A kind is an
/// uninhabited type declared next to its value (`enum Window {}`), so a kind
/// has one value type and a node cannot declare another for it.
pub trait PublishedKind: 'static {
    /// The value the key's target reads and writes.
    type Value: Clone + PartialEq + Send + Sync + 'static;
}

/// That the node `Self` publishes a value of the kind `K`, whose value is
/// `K::Value`. What a typed key's method for that kind needs, and what
/// [`declare_params!`](crate::declare_params) writes from a `published` entry.
///
/// A session addresses the kinds it knows. A node that publishes a kind the
/// session does not know has nothing that can address its value, and the
/// session leaves it out.
pub trait HasPublished<K: PublishedKind> {}

/// A cell the control side reads and replaces one published value through,
/// and sweeps ([`Reclaim`]): a published cell frees what it retired because it
/// is published. Control thread only: the session calls it under its value
/// lock.
pub trait Published: Reclaim + Send + Sync + 'static {
    /// The value in force.
    type Value: Clone + PartialEq + Send + Sync + 'static;

    /// The value now.
    fn current(&self) -> Self::Value;

    /// Publishes `value`; an equal one may change nothing.
    fn replace(&self, value: &Self::Value);
}

/// A published `T`: the value in force is read and replaced as the `T` itself.
impl<T: Clone + PartialEq + Send + Sync + 'static> Published for Publisher<T> {
    type Value = T;

    fn current(&self) -> T {
        (*Self::current(self)).clone()
    }

    fn replace(&self, value: &T) {
        if *Self::current(self) != *value {
            self.publish(value.clone());
        }
    }
}

/// One published cell of a node as `NodeParts` carries it: its kind and the
/// cell, an `Arc<dyn Published<Value = K::Value>>` behind `Any`, and the same
/// cell as a [`Reclaim`], so the entry sweeps without knowing its kind.
pub struct ErasedPublished {
    kind: TypeId,
    kind_name: &'static str,
    cell: Arc<dyn Any + Send + Sync>,
    sweep: Arc<dyn Reclaim + Send + Sync>,
}

impl ErasedPublished {
    pub(crate) fn new<K: PublishedKind>(cell: Arc<dyn Published<Value = K::Value>>) -> Self {
        Self {
            kind: TypeId::of::<K>(),
            kind_name: std::any::type_name::<K>(),
            sweep: cell.clone(),
            cell: Arc::new(cell),
        }
    }

    /// Frees what the cell retired and no reader holds; returns how many
    /// retired values are still pinned. Control thread, from the owner's tick:
    /// the session's reclaim phase sweeps every entry of every node.
    pub fn reclaim(&self) -> usize {
        self.sweep.reclaim()
    }

    /// The name of the kind's type.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        self.kind_name
    }

    /// The cell, when it is of the kind `K`: its value is `K::Value` by the
    /// one way an entry is made.
    #[must_use]
    pub fn cell<K: PublishedKind>(&self) -> Option<Arc<dyn Published<Value = K::Value>>> {
        if self.kind != TypeId::of::<K>() {
            return None;
        }
        self.cell
            .downcast_ref::<Arc<dyn Published<Value = K::Value>>>()
            .cloned()
    }
}

impl std::fmt::Debug for ErasedPublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedPublished")
            .field("kind", &self.kind_name)
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

    /// The publisher, for the session's cell.
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
