//! Targets on the session's own modulation declarations: a route's polarity
//! and curve, an LFO's rate and a macro's value.
//!
//! Each is a field of a declaration the apply reads
//! ([`Mods`](crate::Mods)), held in a [`Declared`] cell the declaration and
//! the target share. A write is staged like any published value and lands at
//! the update, ahead of the modulation apply, which then republishes the
//! lanes the declaration reaches: no commit, and no phase reset, since the
//! compile dates an LFO's clock itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tutti_modulation::{Clock, Law, LfoRate, Polarity, Source};
use tutti_rt::LockExt;
use tutti_units::{Bipolar, Hz, Real};

use super::error::SetRefusal;
use super::slot::{Held, SlotCell, SlotId, keyed_slot_target};
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
        self.value.lock_recover().clone()
    }

    /// Replaces the value and marks the declarations edited.
    pub(crate) fn set(&self, value: T) {
        *self.value.lock_recover() = value;
        self.edited.store(true, Ordering::Release);
    }

    /// Edits the value in place; `edit` answers whether it changed anything,
    /// which marks the declarations edited.
    fn edit(&self, edit: impl FnOnce(&mut T) -> bool) {
        if edit(&mut self.value.lock_recover()) {
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
                clock: Clock::Beats { per_cycle, .. },
                ..
            } => LfoRate::Beats(per_cycle),
            // The slot is registered only while the source is an LFO.
            Source::Lfo {
                clock: Clock::Seconds { rate, .. },
                ..
            } => LfoRate::Hz(rate),
            _ => LfoRate::Hz(Hz(0.0)),
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
            let (Clock::Beats { phase, .. } | Clock::Seconds { phase, .. }) = *clock;
            let next = match *value {
                LfoRate::Beats(per_cycle) => Clock::Beats { per_cycle, phase },
                LfoRate::Hz(rate) => Clock::Seconds {
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
            Source::Macro { value } => Bipolar::clamped(f64::from(value)),
            // The slot is registered only while the source is a macro.
            _ => Bipolar::ZERO,
        }
    }

    fn replace(&self, value: &Bipolar) -> bool {
        self.0.edit(|source| {
            // The compile's raw value is an `f32`: the one narrowing.
            let next = f32::from_unit(*value);
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
    /// ([`Polarity`]), as [`ModRouteId::polarity`] names it.
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update), which republishes the
    /// lanes the route reaches with no commit; a read answers the staged
    /// value, else the one in force. A write to a route removed before the
    /// update is dropped.
    PolarityTarget,
    Polarity,
    ModRouteId,
    RoutePolarity
);

keyed_slot_target!(
    /// The law that shapes a modulation route's value ([`Law`]), as
    /// [`ModRouteId::curve`] names it. Staged and published as
    /// [`PolarityTarget`] is.
    CurveTarget,
    Law,
    ModRouteId,
    RouteCurve
);

keyed_slot_target!(
    /// An LFO's rate ([`LfoRate`]: cycles per second, or beats per cycle), as
    /// [`ModSourceId::rate`] names it. Staged and published as
    /// [`PolarityTarget`] is; the lanes it reaches take the new rate from a
    /// dated anchor, so the LFO's phase continues across the update.
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
        PolarityTarget::new(self)
    }

    /// The route's law.
    #[must_use]
    pub const fn curve(self) -> CurveTarget {
        CurveTarget::new(self)
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
