//! [`ParamTarget`]: a native param of a node, what `key.cutoff()` returns.

use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;

use tutti_graph::{NodeKey, ParamUnit};
use tutti_types::ParamCell;
use tutti_units::{ParamAddr, UnitAddr};

use super::batch::Describe;
use super::error::{SetRefusal, ValueErrorKind};
use super::state::Params;
use super::target::{Info, Moved, ScalarTarget, Target, TargetId, sealed, target_builders};

/// One native param of a node in the session's graph, with the unit its value
/// is in: what `key.cutoff()` returns. The typed sibling of
/// [`ParamRef`](crate::ParamRef), which it converts into, so
/// `mods.route(src, lp.cutoff(), depth)` takes it as it takes the untyped
/// one; a plugin's param stays a `ParamRef`.
///
/// It is written and read with the session's verbs:
/// `session.set(lp.cutoff().to(Hz::new(800.0)))`. The write goes to the
/// param's cell at the cell's own width, so a [`Seconds`](tutti_units::Seconds)
/// stays an `f64`.
///
/// `Copy`, and equal when node and address are.
pub struct ParamTarget<U> {
    node: NodeKey,
    addr: UnitAddr,
    /// The node type the key named, which declares the unit `U`.
    owner: TypeId,
    unit: PhantomData<fn() -> U>,
}

impl<U> ParamTarget<U> {
    /// `addr` on `node` of the type `owner`, whose value is in `U`. Only a
    /// [`Key`](crate::Key)'s param methods make one, so the unit is always the
    /// one the node type declares; the target is `NoSuchTarget` once the node
    /// is another type (`Graph::replace`).
    #[must_use]
    pub(crate) fn new(node: impl Into<NodeKey>, addr: UnitAddr, owner: TypeId) -> Self {
        Self {
            node: node.into(),
            addr,
            owner,
            unit: PhantomData,
        }
    }

    /// The node with the param.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.node
    }

    /// The param's address on that node.
    #[must_use]
    pub const fn addr(self) -> UnitAddr {
        self.addr
    }
}

// Written out: the derives would demand `U: Clone`/`Eq`/..., and the float
// units cannot be `Eq`. The target's identity is its node and address.
impl<U> Clone for ParamTarget<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for ParamTarget<U> {}
impl<U> PartialEq for ParamTarget<U> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.addr == other.addr
    }
}
impl<U> Eq for ParamTarget<U> {}
impl<U> std::hash::Hash for ParamTarget<U> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.node.hash(state);
        self.addr.hash(state);
    }
}
impl<U> fmt::Debug for ParamTarget<U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ParamTarget<{}>({}, {:?})",
            std::any::type_name::<U>(),
            self.node.0,
            self.addr
        )
    }
}

impl<U> From<ParamTarget<U>> for ParamAddr {
    fn from(param: ParamTarget<U>) -> Self {
        ParamAddr::unit(param.addr)
    }
}

impl<U> From<ParamTarget<U>> for crate::ParamRef {
    fn from(param: ParamTarget<U>) -> Self {
        Self::new(param.node, ParamAddr::unit(param.addr))
    }
}

impl<U: ParamUnit> sealed::Sealed for ParamTarget<U> {
    fn id(self) -> TargetId {
        TargetId::Param(self.node, self.addr, self.owner)
    }
}
impl<U: ParamUnit> Target for ParamTarget<U> {
    type Value = U;
}
impl<U: ParamUnit> Moved for ParamTarget<U> {}
impl<U: crate::Scalar> ScalarTarget for ParamTarget<U> {
    type Unit = U;
}

impl<U: ParamUnit> sealed::Land for ParamTarget<U> {
    type Cell = ParamCell;

    fn resolve(self, params: &Params) -> Option<(ParamCell, U)> {
        let cell = params.cell(self.node, self.addr, self.owner)?;
        let base = U::from_wide(cell.load_wide(Ordering::Acquire));
        Some((cell, base))
    }

    fn check(cell: &ParamCell, value: &U) -> Result<(), SetRefusal> {
        let wide = value.to_wide();
        if !wide.is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        // An `f32` cell holds what an `f32` does: a finite figure that rounds
        // to infinity there is not stored.
        if matches!(cell, ParamCell::F32(_)) && !(wide as f32).is_finite() {
            return Err(SetRefusal::OutOfRange);
        }
        Ok(())
    }

    fn land(cell: &ParamCell, value: &U) {
        cell.store_wide(value.to_wide(), Ordering::Release);
    }
}

target_builders!(moved [U: ParamUnit] ParamTarget<U>);

/// What a control shows of a native param ([`ParamTarget::info`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetInfo<V> {
    /// The param's address on its node.
    pub param: UnitAddr,
    /// The value the param had when its node was last addressed (added, or
    /// replaced): the declaration's seed.
    pub default: V,
}

impl<U: ParamUnit> Describe for Info<ParamTarget<U>> {
    type Output = TargetInfo<U>;

    fn describe(self, params: &Params) -> Result<TargetInfo<U>, (TargetId, ValueErrorKind)> {
        let target = self.target;
        let id = sealed::Sealed::id(target);
        params
            .seed(target.node, target.addr, target.owner)
            .map(|seed| TargetInfo {
                param: target.addr,
                default: U::from_wide(seed),
            })
            .ok_or((id, ValueErrorKind::NoSuchTarget))
    }
}
