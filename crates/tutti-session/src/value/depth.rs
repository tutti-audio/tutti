//! [`DepthTarget`]: how deep a modulation route moves its target.

use tutti_rt::Param;
use tutti_units::Bipolar;

use super::error::SetRefusal;
use super::slot::{Slot, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{Modulatable, ScalarTarget, Target, TargetId, sealed, target_builders};
use crate::{ModRouteId, ModSourceId, ModTarget};

/// A route's depth: the [`Slot`] of its [`DepthTarget`], over the cell the
/// route's lane reads.
pub(crate) struct DepthSlot(pub(crate) Param<Bipolar>);

impl Slot<Bipolar> for DepthSlot {
    fn load(&self) -> Bipolar {
        self.0.load()
    }

    fn check(&self, _: &Bipolar) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &Bipolar) {
        self.0.store(*value);
    }
}

keyed_slot_target!(
    /// The depth of the route from a source into a target, a fraction of the
    /// target's range (negative inverts), as [`DepthTarget::of`] names it.
    ///
    /// The route's lane reads the depth from a cell at every evaluation, so a
    /// write lands at once: the lane's program is not republished, nothing is
    /// recompiled, and no phase resets. A call that names a source and target
    /// with no route between them (never declared, or removed) fails with
    /// `NoSuchTarget`.
    DepthTarget,
    Bipolar,
    DepthKey,
    RouteDepth
);

/// What names a route's depth: the route from a source into a target, or the
/// one from a source into another route's depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepthKey {
    /// The route from a source into a target.
    Into(ModSourceId, ModTarget),
    /// The route from a source into the depth of a route.
    Scaling(ModSourceId, ModRouteId),
}

impl Modulatable for DepthTarget {}
impl ScalarTarget for DepthTarget {
    type Unit = Bipolar;
}

impl DepthTarget {
    /// The depth of the route from `source` into `target`.
    #[must_use]
    pub fn of(source: ModSourceId, target: impl Into<ModTarget>) -> Self {
        Self::new(DepthKey::Into(source, target.into()))
    }

    /// The depth of the route from `source` into the depth of `route`
    /// ([`RouteSpec::scaling`](crate::RouteSpec::scaling)).
    #[must_use]
    pub fn scaling(source: ModSourceId, route: ModRouteId) -> Self {
        Self::new(DepthKey::Scaling(source, route))
    }

    /// The source of the route.
    #[must_use]
    pub const fn source(self) -> ModSourceId {
        match self.0 {
            DepthKey::Into(source, _) | DepthKey::Scaling(source, _) => source,
        }
    }

    /// What the route modulates, for a route into a target; `None` for one
    /// that scales another route's depth.
    #[must_use]
    pub const fn target(self) -> Option<ModTarget> {
        match self.0 {
            DepthKey::Into(_, target) => Some(target),
            DepthKey::Scaling(..) => None,
        }
    }
}
