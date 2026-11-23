//! [`DepthTarget`]: how deep a modulation route moves its target.

use tutti_rt::Param;
use tutti_units::Bipolar;

use super::error::SetRefusal;
use super::slot::{Slot, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{
    Effective, Modulatable, ScalarTarget, Target, TargetId, sealed, target_builders,
};
use crate::{ModSourceId, ModTarget};

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
    (ModSourceId, ModTarget),
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
        Self::new((source, target.into()))
    }

    /// The value the audio thread uses.
    pub fn effective(self) -> Effective<Self> {
        Effective { target: self }
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
