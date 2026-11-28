//! Targets on the session's own modulation declarations: a route's polarity
//! and curve, an LFO's rate and a macro's value.
//!
//! Each is a field of a declaration the apply reads
//! ([`Mods`](crate::Mods)), held in a [`Declared`] cell the declaration and
//! the target share. A write is staged like any published value and lands at
//! the update, ahead of the modulation apply, which then republishes the
//! lanes the declaration reaches: no commit, and no phase reset, since the
//! compile dates an LFO's clock itself.

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_modulation::{Curve, Law, LayerClock, LfoRate, Polarity, Source, TargetKey};
use tutti_nodes::automation::Trajectory;
use tutti_rt::Mutex;
use tutti_units::Bipolar;

use super::error::SetRefusal;
use super::param::ParamTarget;
use super::slot::{Held, ShapeKey, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{Target, TargetId, sealed, target_builders};
use crate::{ModRouteId, ModSourceId};

/// Whether a declaration changed since the apply built its declarations:
/// shared by the session's [`Mods`](crate::Mods) and every [`Declared`] cell.
pub(crate) type Edited = Arc<AtomicBool>;

/// One field of a declaration, shared by the declaration (the apply reads it)
/// and its target (the update writes it). A write marks the declarations
/// edited, so the next apply builds them again.
#[derive(Debug)]
pub(crate) struct Declared<T> {
    value: Mutex<T>,
    edited: Edited,
}

impl<T: Clone> Declared<T> {
    pub(crate) fn new(value: T, edited: &Edited) -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(value),
            edited: Arc::clone(edited),
        })
    }

    /// The value in force.
    pub(crate) fn get(&self) -> T {
        self.value.lock().clone()
    }

    /// Replaces the value and marks the declarations edited.
    pub(crate) fn set(&self, value: T) {
        *self.value.lock() = value;
        self.edited.store(true, Ordering::Release);
    }

    /// Edits the value in place; `edit` answers whether it changed anything,
    /// which marks the declarations edited.
    fn edit(&self, edit: impl FnOnce(&mut T) -> bool) {
        if edit(&mut self.value.lock()) {
            self.edited.store(true, Ordering::Release);
        }
    }
}

/// A route's polarity or law: the whole [`Declared`] value.
pub(crate) struct Field<T>(pub(crate) Arc<Declared<T>>);

impl<T: Copy + PartialEq + Send + Sync + 'static> Held for Field<T> {
    type Value = T;

    fn current(&self) -> T {
        self.0.get()
    }

    fn replace(&self, value: &T) -> bool {
        self.0.edit(|now| std::mem::replace(now, *value) != *value);
        true
    }
}

/// An LFO source's rate, in the source's clock.
pub(crate) struct RateField(pub(crate) Arc<Declared<Source>>);

impl Held for RateField {
    type Value = LfoRate;

    fn current(&self) -> LfoRate {
        match self.0.get() {
            Source::Lfo {
                clock: LayerClock::Beats { per_cycle, .. },
                ..
            } => LfoRate::Beats(per_cycle),
            Source::Lfo {
                clock: LayerClock::Seconds { rate, .. },
                ..
            } => LfoRate::Hz(rate),
            _ => unreachable!("the slot is unregistered before the source stops being an LFO"),
        }
    }

    fn check(&self, value: &LfoRate) -> Result<(), SetRefusal> {
        match value {
            LfoRate::Hz(rate) if !rate.get().is_finite() => Err(SetRefusal::NotFinite),
            _ => Ok(()),
        }
    }

    fn replace(&self, value: &LfoRate) -> bool {
        self.0.edit(|source| {
            let Source::Lfo { clock, .. } = source else {
                return false;
            };
            // The phase offset stays; the compile dates the rate change.
            let (LayerClock::Beats { phase, .. } | LayerClock::Seconds { phase, .. }) = *clock;
            let next = match *value {
                LfoRate::Beats(per_cycle) => LayerClock::Beats { per_cycle, phase },
                LfoRate::Hz(rate) => LayerClock::Seconds {
                    rate,
                    phase,
                    anchor: None,
                },
            };
            std::mem::replace(clock, next) != next
        });
        true
    }
}

/// A macro source's value.
pub(crate) struct MacroField(pub(crate) Arc<Declared<Source>>);

impl Held for MacroField {
    type Value = Bipolar;

    fn current(&self) -> Bipolar {
        match self.0.get() {
            Source::Macro { value } => Bipolar::clamped(value),
            _ => unreachable!("the slot is unregistered before the source stops being a macro"),
        }
    }

    fn replace(&self, value: &Bipolar) -> bool {
        self.0.edit(|source| {
            let next = value.get();
            match source {
                Source::Macro { value } if *value != next => {
                    *value = next;
                    true
                }
                _ => false,
            }
        });
        true
    }
}

keyed_slot_target!(
    /// How a modulation route maps its source's value onto its span
    /// ([`Polarity`]), as [`ModRouteId::polarity`] names it; or the route's
    /// scale does, as [`PolarityTarget::scale_of`] names it.
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update), which republishes the
    /// lanes the route reaches with no commit; a read answers the staged
    /// value, else the one in force. A write to a route removed before the
    /// update is dropped.
    PolarityTarget,
    Polarity,
    ShapeKey,
    RoutePolarity
);

keyed_slot_target!(
    /// The law that shapes a modulation route's value ([`Law`]), as
    /// [`ModRouteId::curve`] names it, or [`CurveTarget::scale_of`] for a scale's.
    /// Staged and published as
    /// [`PolarityTarget`] is.
    CurveTarget,
    Law,
    ShapeKey,
    RouteCurve
);

keyed_slot_target!(
    /// An LFO's rate ([`LfoRate`]: cycles per second, or beats per cycle), as
    /// [`ModSourceId::rate`] names it. Staged and published as
    /// [`PolarityTarget`] is. The phase continues across the update for an Hz
    /// to Hz change, which the lanes take from a dated anchor; a beats clock is
    /// bar-locked, so a change to or from one jumps to where the beat puts it.
    ///
    /// A source that is not an LFO has no rate target: a call that names it
    /// fails with `NoSuchTarget`.
    RateTarget,
    LfoRate,
    ModSourceId,
    SourceRate
);

keyed_slot_target!(
    /// A macro's value ([`Bipolar`], `-1..=1`), as [`ModSourceId::value`]
    /// names it. Staged and published as [`PolarityTarget`] is. A macro stays
    /// a constant, so a lane that only macros reach keeps its steady path.
    ///
    /// A source that is not a macro has no value target: a call that names
    /// it fails with `NoSuchTarget`.
    MacroTarget,
    Bipolar,
    ModSourceId,
    SourceValue
);

impl ModRouteId {
    /// The route's polarity.
    #[must_use]
    pub const fn polarity(self) -> PolarityTarget {
        PolarityTarget::new(ShapeKey::Route(self))
    }

    /// The route's law.
    #[must_use]
    pub const fn curve(self) -> CurveTarget {
        CurveTarget::new(ShapeKey::Route(self))
    }
}

impl PolarityTarget {
    /// The polarity of the scale of `route`
    /// ([`Mods::scale`](crate::Mods::scale)).
    #[must_use]
    pub const fn scale_of(route: ModRouteId) -> Self {
        Self::new(ShapeKey::Scale(route))
    }
}

impl CurveTarget {
    /// The law of the scale of `route`
    /// ([`Mods::scale`](crate::Mods::scale)).
    #[must_use]
    pub const fn scale_of(route: ModRouteId) -> Self {
        Self::new(ShapeKey::Scale(route))
    }
}

impl ModSourceId {
    /// The LFO's rate.
    #[must_use]
    pub const fn rate(self) -> RateTarget {
        RateTarget::new(self)
    }

    /// The macro's value.
    #[must_use]
    pub const fn value(self) -> MacroTarget {
        MacroTarget::new(self)
    }
}

/// A param's automation curve, in the unit `U` its param is in.
pub(crate) type Automation<U> = Declared<Arc<Trajectory<U>>>;

/// A param's automation with its unit erased: what the declarations read.
pub(crate) trait AutomationCell: Send + Sync + 'static {
    /// The curve in force.
    fn curve(&self) -> Arc<dyn Curve>;

    /// The cell as the [`Automation`] of its unit.
    fn as_any(&self) -> &dyn Any;
}

impl<U: crate::Scalar> AutomationCell for Automation<U> {
    fn curve(&self) -> Arc<dyn Curve> {
        self.get()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A param's automation curve, whole.
pub(crate) struct AutomationField<U: crate::Scalar>(pub(crate) Arc<Automation<U>>);

impl<U: crate::Scalar> Held for AutomationField<U> {
    type Value = Arc<Trajectory<U>>;

    fn current(&self) -> Self::Value {
        self.0.get()
    }

    fn replace(&self, value: &Self::Value) -> bool {
        self.0.edit(|now| {
            let changed = **now != **value;
            *now = Arc::clone(value);
            changed
        });
        true
    }
}

/// A param's automation curve ([`Trajectory`], in the unit `U` the param is
/// in), as [`ParamTarget::automation`] names it: the breakpoints the lane
/// plays in place of the param's knob, and where a take lands.
///
/// A write is staged and published by the next
/// [`Session::update`](crate::Session::update) as one republish of the lane;
/// a read answers the staged curve, else the one in force. The param has the
/// target once [`Mods::automate`](crate::Mods::automate) declared its lane: a
/// call that names a param with none fails with `NoSuchTarget`, as one that
/// names it in another unit does. A take that finishes replaces the curve.
pub struct AutomationTarget<U> {
    key: TargetKey,
    unit: PhantomData<fn() -> U>,
}

impl<U> AutomationTarget<U> {
    pub(crate) const fn new(key: TargetKey) -> Self {
        Self {
            key,
            unit: PhantomData,
        }
    }
}

// Written out, as `ParamTarget`'s are: the identity is the key.
impl<U> Clone for AutomationTarget<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for AutomationTarget<U> {}
impl<U> PartialEq for AutomationTarget<U> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl<U> Eq for AutomationTarget<U> {}
impl<U> std::hash::Hash for AutomationTarget<U> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}
impl<U> std::fmt::Debug for AutomationTarget<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AutomationTarget<{}>({:?})",
            std::any::type_name::<U>(),
            self.key
        )
    }
}

impl<U: crate::Scalar> sealed::Sealed for AutomationTarget<U> {
    fn id(self) -> TargetId {
        TargetId::Slot(SlotId::Automation(self.key))
    }
}
impl<U: crate::Scalar> Target for AutomationTarget<U> {
    type Value = Arc<Trajectory<U>>;
}
impl<U: crate::Scalar> sealed::Store for AutomationTarget<U> {
    type Cell = SlotCell<Arc<Trajectory<U>>>;

    crate::value::no_host_gesture!();

    fn resolve(self, params: &Params) -> Option<(Self::Cell, Self::Value)> {
        let cell = params.slot::<Arc<Trajectory<U>>>(SlotId::Automation(self.key))?;
        let base = cell.load();
        Some((cell, base))
    }

    fn check(cell: &Self::Cell, value: &Self::Value) -> Result<(), SetRefusal> {
        cell.check(value)
    }

    fn store(cell: &Self::Cell, value: &Self::Value) {
        cell.store(value);
    }
}
target_builders!([U: crate::Scalar] AutomationTarget<U>);

impl<U: crate::Scalar> ParamTarget<U> {
    /// The param's automation curve.
    #[must_use]
    pub fn automation(self) -> AutomationTarget<U> {
        AutomationTarget::new(crate::ParamRef::from(self).key())
    }
}
