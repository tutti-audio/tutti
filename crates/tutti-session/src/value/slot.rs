//! Targets on the session's own values: the project's tempo and meter, the
//! click's level and mode. Each lives in a cell the session owns, registered
//! in the value table when the session is built, and is written and read with
//! the session's verbs like a node's param.

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_music::MeterMap;
use tutti_nodes::{ClickSettings, MetronomeMode};
use tutti_rt::{Setting, SettingWord};
use tutti_tempo::TempoMap;
use tutti_transport::{ProjectMeter, TransportSettings};
use tutti_units::Amplitude;

use super::error::SetRefusal;
use super::state::Params;
use super::target::{Target, TargetId, sealed, target_builders};

/// Which of the session's own values a slot target names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotId {
    /// The project's tempo map.
    Tempo,
    /// The project's meter.
    Meter,
    /// The click's level.
    ClickVolume,
    /// When the click sounds.
    ClickMode,
}

/// A session value behind a target: how it is read, checked and stored.
/// Control side only: every call runs under the value lock.
pub trait Slot<V>: Send + Sync + 'static {
    /// The value now.
    fn load(&self) -> V;

    /// Whether `value` may be stored.
    fn check(&self, value: &V) -> Result<(), SetRefusal>;

    /// Stores `value`. Infallible once `check` passed.
    fn store(&self, value: &V);
}

/// The cell a slot target lands through.
pub type SlotCell<V> = Arc<dyn Slot<V>>;

/// What a slot is registered as: the type-erased [`SlotCell`] of its value.
pub(crate) type Erased = Box<dyn Any + Send + Sync>;

pub(crate) fn erase<V: 'static>(slot: SlotCell<V>) -> Erased {
    Box::new(slot)
}

impl Params {
    fn slot<V: 'static>(&self, id: SlotId) -> Option<SlotCell<V>> {
        self.slots.get(&id)?.downcast_ref::<SlotCell<V>>().cloned()
    }
}

/// Writes the target, `Target`, `Land` and builders of a slot target whose
/// value type is fixed.
macro_rules! slot_target {
    ($(#[$meta:meta])* $name:ident, $value:ty, $id:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(());

        impl $name {
            pub(crate) const fn new() -> Self {
                Self(())
            }
        }

        impl sealed::Sealed for $name {
            fn id(self) -> TargetId {
                TargetId::Slot($id)
            }
        }
        impl Target for $name {
            type Value = $value;
        }
        impl sealed::Land for $name {
            type Cell = SlotCell<$value>;

            fn resolve(self, params: &Params) -> Option<(Self::Cell, $value)> {
                let cell = params.slot::<$value>($id)?;
                let base = cell.load();
                Some((cell, base))
            }

            fn check(cell: &Self::Cell, value: &$value) -> Result<(), SetRefusal> {
                cell.check(value)
            }

            fn land(cell: &Self::Cell, value: &$value) {
                cell.store(value);
            }
        }
        target_builders!([] $name);
    };
}

slot_target!(
    /// The project's tempo: the map the engine takes at the next block, as
    /// `session.transport_handle().tempo()` names it.
    ///
    /// A constant tempo is a one-step map, `tempo.to(TempoMap::constant(bpm)?)`;
    /// a map by value works as it stands. A read answers the map in force.
    /// A write is refused with [`SetRefusal::Following`] while the session
    /// follows a live tempo.
    TempoTarget,
    Arc<TempoMap>,
    SlotId::Tempo
);

slot_target!(
    /// The project's meter, which the metronome and every hosted plugin read
    /// once per block, as `session.transport_handle().meter()` names it.
    MeterTarget,
    Arc<MeterMap>,
    SlotId::Meter
);

slot_target!(
    /// The click's level, `0.0..=1.0` in linear [`Amplitude`], as
    /// `session.click().volume()` names it. NaN and infinity are refused with
    /// [`SetRefusal::NotFinite`], a finite level outside the range with
    /// [`SetRefusal::OutOfRange`].
    ClickVolumeTarget,
    Amplitude,
    SlotId::ClickVolume
);

/// A session setting held in one atomic word ([`SettingWord`]): the click's
/// [`MetronomeMode`], as `session.click().mode()` names it. The audio thread
/// reads it once per block; a write lands at once.
pub struct SettingTarget<T> {
    id: SlotId,
    value: PhantomData<fn() -> T>,
}

impl<T> SettingTarget<T> {
    pub(crate) const fn new(id: SlotId) -> Self {
        Self {
            id,
            value: PhantomData,
        }
    }
}

impl<T> Clone for SettingTarget<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SettingTarget<T> {}
impl<T> PartialEq for SettingTarget<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<T> Eq for SettingTarget<T> {}
impl<T> std::hash::Hash for SettingTarget<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}
impl<T> std::fmt::Debug for SettingTarget<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SettingTarget<{}>({:?})",
            std::any::type_name::<T>(),
            self.id
        )
    }
}

impl<T: SettingWord + PartialEq + Send + Sync + 'static> sealed::Sealed for SettingTarget<T> {
    fn id(self) -> TargetId {
        TargetId::Slot(self.id)
    }
}
impl<T: SettingWord + PartialEq + Send + Sync + 'static> Target for SettingTarget<T> {
    type Value = T;
}
impl<T: SettingWord + PartialEq + Send + Sync + 'static> sealed::Land for SettingTarget<T> {
    type Cell = SlotCell<T>;

    fn resolve(self, params: &Params) -> Option<(Self::Cell, T)> {
        let cell = params.slot::<T>(self.id)?;
        let base = cell.load();
        Some((cell, base))
    }

    fn check(cell: &Self::Cell, value: &T) -> Result<(), SetRefusal> {
        cell.check(value)
    }

    fn land(cell: &Self::Cell, value: &T) {
        cell.store(value);
    }
}
target_builders!([T: SettingWord + PartialEq + Send + Sync + 'static] SettingTarget<T>);

// The session's cells.

/// The tempo map, in the transport's settings.
pub(crate) struct TempoSlot {
    pub settings: TransportSettings,
    /// Whether a live tempo owns the map.
    pub following: Arc<std::sync::atomic::AtomicBool>,
}

impl Slot<Arc<TempoMap>> for TempoSlot {
    fn load(&self) -> Arc<TempoMap> {
        self.settings.tempo_map()
    }

    fn check(&self, _: &Arc<TempoMap>) -> Result<(), SetRefusal> {
        if self.following.load(Ordering::Acquire) {
            return Err(SetRefusal::Following);
        }
        Ok(())
    }

    fn store(&self, value: &Arc<TempoMap>) {
        self.settings.set_tempo_map(Arc::clone(value));
    }
}

/// The meter, published to the metronome and every plugin following it.
pub(crate) struct MeterSlot(pub ProjectMeter);

impl Slot<Arc<MeterMap>> for MeterSlot {
    fn load(&self) -> Arc<MeterMap> {
        self.0.current()
    }

    fn check(&self, _: &Arc<MeterMap>) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &Arc<MeterMap>) {
        self.0.set_meter(Arc::clone(value));
    }
}

/// The click's level.
pub(crate) struct ClickVolumeSlot(pub Arc<ClickSettings>);

impl Slot<Amplitude> for ClickVolumeSlot {
    fn load(&self) -> Amplitude {
        self.0.volume()
    }

    fn check(&self, value: &Amplitude) -> Result<(), SetRefusal> {
        let level = value.get();
        if !level.is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        if !(0.0..=1.0).contains(&level) {
            return Err(SetRefusal::OutOfRange);
        }
        Ok(())
    }

    fn store(&self, value: &Amplitude) {
        self.0.volume_cell().store(value.get(), Ordering::Release);
    }
}

/// A [`Setting`] inside a shared owner, found by `project`.
pub(crate) struct SettingSlot<O, T> {
    pub owner: Arc<O>,
    pub project: fn(&O) -> &Setting<T>,
}

impl<O: Send + Sync + 'static, T: SettingWord + Send + Sync + 'static> Slot<T>
    for SettingSlot<O, T>
{
    fn load(&self) -> T {
        (self.project)(&self.owner).get()
    }

    fn check(&self, _: &T) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &T) {
        (self.project)(&self.owner).set(*value);
    }
}

/// The click's targets: `session.click()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickTargets(());

impl ClickTargets {
    pub(crate) const fn new() -> Self {
        Self(())
    }

    /// The click's level.
    #[must_use]
    pub const fn volume(self) -> ClickVolumeTarget {
        ClickVolumeTarget::new()
    }

    /// When the click sounds.
    #[must_use]
    pub const fn mode(self) -> SettingTarget<MetronomeMode> {
        SettingTarget::new(SlotId::ClickMode)
    }
}
