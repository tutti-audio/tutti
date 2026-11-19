//! [`Target`], what holds a value, and the builders every target gets:
//! [`Assign`], [`Modify`], [`Effective`] and [`Info`].

use std::any::{Any, TypeId};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use tutti_graph::NodeKey;
use tutti_units::UnitAddr;

use super::batch::{Batch, Failure, Stageable};
use super::error::SetRefusal;
use super::slot::SlotId;
use super::state::Params;

/// The crate-private half of [`Target`]: how a target finds its cell, checks
/// a value and lands it. Nothing outside the crate can name it, so nothing
/// outside the crate implements [`Target`].
pub(crate) mod sealed {
    use super::{Params, SetRefusal, Target, TargetId};

    /// Marks a type tutti-session made a [`Target`], and names it.
    pub trait Sealed: Copy {
        /// The target's identity: what two assignments to one target share.
        fn id(self) -> TargetId;
    }

    /// What the verbs do with a target.
    pub trait Store: Target {
        /// What a store writes through, resolved once under the value lock.
        type Cell: Send + Sync + 'static;

        /// The cell and the base value now, or `None` when the target is gone.
        fn resolve(self, params: &Params) -> Option<(Self::Cell, Self::Value)>;

        /// Why `resolve` found no cell, when the target is there but its
        /// declaration no longer makes it one (a plugin parameter whose unit
        /// a rescan removed); `None` is a target that is gone.
        fn unresolved(self, _: &Params) -> Option<SetRefusal> {
            None
        }

        /// Whether `value` may be written through `cell`.
        fn check(cell: &Self::Cell, value: &Self::Value) -> Result<(), SetRefusal>;

        /// Stores `value` through `cell`. Infallible and panic-free.
        fn store(cell: &Self::Cell, value: &Self::Value);
    }
}

/// Something that holds a value. Sealed: only tutti-session implements it.
///
/// A target is a `Copy` reference taken from a key (`filter.cutoff()`); the
/// verbs [`Session::set`](crate::Session::set) and
/// [`Session::get`](crate::Session::get) take items built from targets. Its
/// builders are inherent methods every target type has, so a caller imports
/// no trait to use them: `to`, `modify`, `info`, and `effective` on a
/// [`Modulatable`] one.
pub trait Target: Copy + Send + Sync + 'static + sealed::Sealed {
    /// What the target holds.
    type Value: Clone + PartialEq + Send + Sync + 'static;
}

/// A target a modulation lane, an automation lane, a mapping or a follower
/// can move, so it has an [effective](Effective) value beside its base.
pub trait Modulatable: Target {}

/// A [`Modulatable`] target whose value is a number in a unit.
///
/// Named `ScalarTarget` because [`Scalar`](crate::Scalar) is the graph's
/// trait for a unit a param can hold.
pub trait ScalarTarget: Modulatable {
    /// The unit of the value.
    type Unit;
}

/// A target's identity, whatever its type: what an [`AnyTarget`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetId {
    /// A native param of a node, at an address, as the node type its key
    /// names declares it: a key of another node type than the one the node
    /// is now names no such param.
    Param(NodeKey, UnitAddr, TypeId),
    /// A parameter of a hosted plugin, whatever the value type its target names
    /// (its position, or a unit).
    #[cfg(feature = "plugin")]
    PluginParam(NodeKey, tutti_units::ParamId),
    /// One of the session's own values (its tempo, meter or click), or a value
    /// of a node that is not a param (a clip's events).
    Slot(SlotId),
}

/// A target with its type erased, in the error of the call that named it.
///
/// `K` is the session's key type. Until nodes are named by it, the target
/// holds a [`NodeKey`].
pub struct AnyTarget<K> {
    id: TargetId,
    key: PhantomData<fn() -> K>,
}

impl<K> AnyTarget<K> {
    pub(crate) fn of(id: TargetId) -> Self {
        Self {
            id,
            key: PhantomData,
        }
    }

    /// Whether this is `target`.
    #[must_use]
    pub fn is<T: Target>(&self, target: T) -> bool {
        sealed::Sealed::id(target) == self.id
    }

    /// The node the target is on, or `None` for one of the session's own
    /// values (its tempo, meter or click).
    #[must_use]
    pub fn node(&self) -> Option<NodeKey> {
        match self.id {
            TargetId::Param(node, ..) => Some(node),
            #[cfg(feature = "plugin")]
            TargetId::PluginParam(node, _) => Some(node),
            TargetId::Slot(slot) => slot.node(),
        }
    }
}

impl<K> Clone for AnyTarget<K> {
    fn clone(&self) -> Self {
        Self::of(self.id)
    }
}
impl<K> PartialEq for AnyTarget<K> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<K> Eq for AnyTarget<K> {}
impl<K> fmt::Debug for AnyTarget<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AnyTarget").field(&self.id).finish()
    }
}

/// `target.to(value)`: an absolute assignment, for [`Session::set`](crate::Session::set).
#[must_use = "an assignment is written by `Session::set`"]
pub struct Assign<T: Target> {
    pub(crate) target: T,
    pub(crate) value: T::Value,
}

impl<T: Target> Clone for Assign<T> {
    fn clone(&self) -> Self {
        Self {
            target: self.target,
            value: self.value.clone(),
        }
    }
}

impl<T: Target> fmt::Debug for Assign<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Assign").finish_non_exhaustive()
    }
}

/// `target.modify(f)`: a new value built from the old, for
/// [`Session::set`](crate::Session::set). `f` runs once, under the value lock,
/// on the caller's thread, so it needs neither `Send` nor `'static`; it sees
/// the value the call has built so far for its target.
#[must_use = "an edit is written by `Session::set`"]
pub struct Modify<T: Target, F> {
    pub(crate) target: T,
    pub(crate) edit: F,
}

impl<T: Target, F> fmt::Debug for Modify<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Modify").finish_non_exhaustive()
    }
}

/// `target.effective()`: the value the audio thread uses, which only a
/// [`Modulatable`] target has. [`Session::get`](crate::Session::get) takes no
/// `Effective` yet, so asking for one does not compile.
#[must_use = "a read is answered by `Session::get`"]
pub struct Effective<T: Modulatable> {
    #[allow(dead_code)]
    pub(crate) target: T,
}

impl<T: Modulatable> fmt::Debug for Effective<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Effective").finish_non_exhaustive()
    }
}

/// `target.info()`: what a control shows of a target, for
/// [`Session::get`](crate::Session::get).
#[must_use = "a read is answered by `Session::get`"]
pub struct Info<T: Target> {
    pub(crate) target: T,
}

impl<T: Target> fmt::Debug for Info<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Info").finish_non_exhaustive()
    }
}

/// An absolute assignment with its target erased: presets, MIDI learn,
/// scripting and generic UIs build a `Vec` of them at run time. A
/// [`Modify`] holds a closure and is not erasable.
#[derive(Clone)]
#[must_use = "an assignment is written by `Session::set`"]
pub struct AnyAssign(pub(crate) Arc<dyn Erased>);

/// An [`Assign`] behind a pointer.
pub trait Erased: Send + Sync {
    fn stage(&self, batch: &mut Batch<'_>) -> Result<(), Failure>;
}

impl<T: sealed::Store> Erased for Assign<T> {
    fn stage(&self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        self.clone().stage(batch)
    }
}

impl fmt::Debug for AnyAssign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyAssign").finish_non_exhaustive()
    }
}

impl<T: sealed::Store> Assign<T> {
    /// The assignment with its target erased.
    pub fn erase(self) -> AnyAssign {
        AnyAssign(Arc::new(self))
    }
}

/// A target's value in a batch: the slot a landing reads.
pub trait Staged {
    /// The target's identity.
    fn id(&self) -> TargetId;
    /// The value the call has built so far.
    fn value_mut(&mut self) -> &mut dyn Any;
    /// Whether the slot's cell takes `value`, which is of the slot's type.
    fn accepts(&self, value: &dyn Any) -> Result<(), SetRefusal>;
    /// Stores it.
    fn store(self: Box<Self>);
}

/// Writes the builders every target type has, from the target's generics.
///
/// `moved` adds `effective`.
macro_rules! target_builders {
    ([$($generics:tt)*] $target:ty $(where $($bounds:tt)+)?) => {
        impl<$($generics)*> $target $(where $($bounds)+)? {
            /// An assignment of `value`: an absolute write, for
            /// [`Session::set`](crate::Session::set).
            pub fn to(
                self,
                value: impl Into<<Self as $crate::Target>::Value>,
            ) -> $crate::Assign<Self> {
                $crate::Assign { target: self, value: value.into() }
            }

            /// An edit: the value `f` builds from the value the call has for
            /// this target, for [`Session::set`](crate::Session::set). `f` runs
            /// under the value lock, so a graph edit (`add`, `remove`,
            /// `replace`) and a `Values` write on another thread wait until it
            /// returns; it must not wait on another thread.
            pub fn modify<F>(self, f: F) -> $crate::Modify<Self, F>
            where
                F: FnOnce(&<Self as $crate::Target>::Value) -> <Self as $crate::Target>::Value,
            {
                $crate::Modify { target: self, edit: f }
            }

            /// What a control shows of the target, read by
            /// [`Session::get`](crate::Session::get).
            pub fn info(self) -> $crate::Info<Self> {
                $crate::Info { target: self }
            }
        }
    };
    (moved [$($generics:tt)*] $target:ty $(where $($bounds:tt)+)?) => {
        $crate::value::target_builders!([$($generics)*] $target $(where $($bounds)+)?);

        impl<$($generics)*> $target $(where $($bounds)+)? {
            /// The value the audio thread uses, which a lane or a mapping may
            /// have moved from the base.
            pub fn effective(self) -> $crate::Effective<Self> {
                $crate::Effective { target: self }
            }
        }
    };
}
pub(crate) use target_builders;
