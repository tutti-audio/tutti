//! [`RangeTarget`]: the range a modulation target is declared over.

use std::marker::PhantomData;
use std::ops::Range;
use std::sync::Arc;

use tutti_graph::Scalar;

use super::error::SetRefusal;
use super::program::Declared;
use super::slot::{Slot, SlotCell, SlotId};
use super::state::Params;
use super::target::{Target, TargetId, sealed, target_builders};
use crate::{ModTarget, ParamTarget, Ranged};

/// A range declared on a target: the cell the declaration and the
/// [`RangeTarget`] share, as bounds in the cell's raw `f64`.
pub(crate) type Bounds = Declared<(f64, f64)>;

/// The range of a modulation target, in the unit `U` of the target's value:
/// what a route's depth is a fraction of, and what the lane clamps its
/// effective value to (both ends inclusive). As
/// [`ParamTarget::range`](crate::ParamTarget::range) names it for a param,
/// and [`RangeTarget::of`] for a mix or an edge's gain.
///
/// It bounds the **effective** value only: the target's base (a param's
/// own value, a mix, an edge's gain) is never rewritten, so
/// `session.get(param)` reads the same after a range narrows around it, and a
/// range that widens again lets the effective value go back to where the
/// base and the routes have got to.
///
/// The target exists once [`Mods::range`](crate::Mods::range) declared the
/// range: a call that names a target with none fails with `NoSuchTarget`, as
/// one that names it in another unit does. A write lands in the declaration at
/// once (a read answers it) and reaches the lane with the next
/// [`Session::update`](crate::Session::update), as a store into the lane's
/// range with no recompile. A NaN or infinite bound is refused with
/// [`SetRefusal::NotFinite`], a start above the end with
/// [`SetRefusal::Backwards`].
pub struct RangeTarget<U> {
    target: ModTarget,
    unit: PhantomData<fn() -> U>,
}

impl<U: Scalar> RangeTarget<U> {
    /// The range of `target`, a [`ParamTarget`], a [`ChannelEdge`](crate::ChannelEdge) (its
    /// gain) or a [`MixRef`](crate::MixRef).
    #[must_use]
    pub fn of<T: Ranged<Unit = U>>(target: T) -> Self {
        Self::new(target.into())
    }

    pub(crate) const fn new(target: ModTarget) -> Self {
        Self {
            target,
            unit: PhantomData,
        }
    }
}

impl<U: Scalar> ParamTarget<U> {
    /// The param's range, which bounds its effective value.
    #[must_use]
    pub fn range(self) -> RangeTarget<U> {
        RangeTarget::of(self)
    }
}

// Written out, as `ParamTarget`'s are: the identity is the target.
impl<U> Clone for RangeTarget<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for RangeTarget<U> {}
impl<U> PartialEq for RangeTarget<U> {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
    }
}
impl<U> Eq for RangeTarget<U> {}
impl<U> std::hash::Hash for RangeTarget<U> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.target.hash(state);
    }
}
impl<U> std::fmt::Debug for RangeTarget<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RangeTarget<{}>({:?})",
            std::any::type_name::<U>(),
            self.target
        )
    }
}

/// The [`Slot`] of a [`RangeTarget`] over `U`.
pub(crate) struct RangeSlot<U> {
    bounds: Arc<Bounds>,
    unit: PhantomData<fn() -> U>,
}

impl<U: Scalar> RangeSlot<U> {
    pub(crate) fn cell(bounds: Arc<Bounds>) -> SlotCell<Range<U>> {
        Arc::new(Self {
            bounds,
            unit: PhantomData,
        })
    }
}

impl<U: Scalar> Slot<Range<U>> for RangeSlot<U> {
    fn load(&self) -> Range<U> {
        let (min, max) = self.bounds.get();
        U::from_cell(min)..U::from_cell(max)
    }

    fn check(&self, value: &Range<U>) -> Result<(), SetRefusal> {
        let (min, max) = (value.start.to_cell(), value.end.to_cell());
        if !min.is_finite() || !max.is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        if min > max {
            return Err(SetRefusal::Backwards);
        }
        Ok(())
    }

    fn store(&self, value: &Range<U>) {
        self.bounds
            .set((value.start.to_cell(), value.end.to_cell()));
    }
}

impl<U: Scalar> sealed::Sealed for RangeTarget<U> {
    fn id(self) -> TargetId {
        TargetId::Slot(SlotId::Range(self.target))
    }
}
impl<U: Scalar> Target for RangeTarget<U> {
    type Value = Range<U>;
}
impl<U: Scalar> sealed::Store for RangeTarget<U> {
    type Cell = SlotCell<Range<U>>;

    fn resolve(self, params: &Params) -> Option<(Self::Cell, Range<U>)> {
        let cell = params.slot::<Range<U>>(SlotId::Range(self.target))?;
        let base = cell.load();
        Some((cell, base))
    }

    fn check(cell: &Self::Cell, value: &Range<U>) -> Result<(), SetRefusal> {
        cell.check(value)
    }

    fn store(cell: &Self::Cell, value: &Range<U>) {
        cell.store(value);
    }
}
target_builders!([U: Scalar] RangeTarget<U>);
