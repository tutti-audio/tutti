//! [`DepthTarget`]: how deep a modulation route moves its target.

use tutti_rt::Param;
use tutti_units::Depth;

use super::error::SetRefusal;
use super::slot::{Slot, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{Modulatable, ScalarTarget, Target, TargetId, sealed, target_builders};
use crate::{ModSourceId, ModTarget};

/// A route's depth: the [`Slot`] of its [`DepthTarget`], over the cell the
/// route's lane reads.
pub(crate) struct DepthSlot(pub(crate) Param<Depth>);

impl Slot<Depth> for DepthSlot {
    fn load(&self) -> Depth {
        self.0.load()
    }

    fn check(&self, _: &Depth) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &Depth) {
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
    Depth,
    (ModSourceId, ModTarget),
    RouteDepth
);

impl Modulatable for DepthTarget {}
impl ScalarTarget for DepthTarget {
    type Unit = Depth;
}

impl DepthTarget {
    /// The depth of the route from `source` into `target`.
    #[must_use]
    pub fn of(source: ModSourceId, target: impl Into<ModTarget>) -> Self {
        Self::new((source, target.into()))
    }

    /// The source of the route.
    #[must_use]
    pub const fn source(self) -> ModSourceId {
        self.0.0
    }

    /// What the route modulates.
    #[must_use]
    pub const fn target(self) -> ModTarget {
        self.0.1
    }
}
