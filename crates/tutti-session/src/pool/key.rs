//! The app's key, held without its type.

use std::any::{Any, TypeId};
use std::cmp::Ordering;
use std::fmt;

/// An app key of any type, as the session holds it.
///
/// The session has no key parameter yet, so a pool entry's key is held erased:
/// [`get`](Self::get) reads it back as the type the app declared it with. Two
/// keys are equal when they have the same type and are equal; they order by
/// type, then by value.
pub struct PoolKey(Box<dyn Erased>);

trait Erased: Send {
    fn clone_box(&self) -> Box<dyn Erased>;
    fn as_any(&self) -> &dyn Any;
    fn type_id_of(&self) -> TypeId;
    fn compare(&self, other: &dyn Erased) -> Ordering;
    fn debug(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;
}

impl<K: Ord + Clone + fmt::Debug + Send + 'static> Erased for K {
    fn clone_box(&self) -> Box<dyn Erased> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn type_id_of(&self) -> TypeId {
        TypeId::of::<K>()
    }

    fn compare(&self, other: &dyn Erased) -> Ordering {
        match other.as_any().downcast_ref::<K>() {
            Some(other) => self.cmp(other),
            None => self.type_id_of().cmp(&other.type_id_of()),
        }
    }

    fn debug(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl PoolKey {
    pub(crate) fn new<K: Ord + Clone + fmt::Debug + Send + 'static>(key: &K) -> Self {
        Self(Box::new(key.clone()))
    }

    /// The key as the type it was declared with, `None` for any other type.
    #[must_use]
    pub fn get<K: 'static>(&self) -> Option<&K> {
        self.0.as_any().downcast_ref::<K>()
    }
}

impl Clone for PoolKey {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl fmt::Debug for PoolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.debug(f)
    }
}

impl PartialEq for PoolKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PoolKey {}

impl PartialOrd for PoolKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PoolKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.compare(&*other.0)
    }
}
