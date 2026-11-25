//! [`DepthTarget`]: how deep a modulation route moves its target.

use tutti_rt::Param;
use tutti_units::Bipolar;

use super::error::SetRefusal;
use super::slot::{DepthKey, Slot, SlotCell, SlotId, keyed_slot_target};
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
    /// target's range (negative inverts), as [`DepthTarget::of`] names it; or
    /// of a route's scale, as [`DepthTarget::scale_of`] names it.
    ///
    /// The route's lane reads the depth from a cell at every evaluation, so a
    /// write lands at once: the lane's program is not republished, nothing is
    /// recompiled, and no phase resets. A call that names a source and target
    /// with no route between them (never declared, or removed), or a scale of a
    /// route with none, fails with
    /// `NoSuchTarget`.
    DepthTarget,
    Bipolar,
    DepthKey,
    RouteDepth
);

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

    /// The depth of the scale of `route` ([`Mods::scale`](crate::Mods::scale)).
    #[must_use]
    pub fn scale_of(route: ModRouteId) -> Self {
        Self::new(DepthKey::Scale(route))
    }

    /// The source of the route, for a route into a target; `None` for a
    /// scale.
    #[must_use]
    pub const fn source(self) -> Option<ModSourceId> {
        match self.0 {
            DepthKey::Into(source, _) => Some(source),
            DepthKey::Scale(_) => None,
        }
    }

    /// What the route modulates, for a route into a target; `None` for a
    /// scale.
    #[must_use]
    pub const fn target(self) -> Option<ModTarget> {
        match self.0 {
            DepthKey::Into(_, target) => Some(target),
            DepthKey::Scale(_) => None,
        }
    }
}
