//! Targets on the session's own values: the project's tempo and meter, the
//! click's level and mode. Each lives in a cell the session owns, registered
//! in the value table when the session is built, and is written and read with
//! the session's verbs like a node's param.

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_graph::NodeKey;
use tutti_music::MeterMap;
use tutti_nodes::{ClickLevel, ClickSettings, MetronomeMode};
use tutti_rt::{LockExt, Setting, SettingWord};
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
    /// Where the mix lands.
    OutputPolicy,
    /// Who the session is on the MIDI wire.
    #[cfg(feature = "midi-hardware")]
    MidiPolicy,
    /// The MIDI outputs' own latency and offset.
    #[cfg(feature = "midi")]
    OutputLatency,
    /// The events a MIDI clip node plays.
    #[cfg(feature = "midi")]
    ClipEvents(NodeKey),
    /// The chord and scale changes a harmony node sends.
    #[cfg(feature = "midi")]
    Harmony(NodeKey),
}

impl SlotId {
    /// The node the value is of, or `None` for one of the session's own.
    pub(crate) const fn node(self) -> Option<NodeKey> {
        match self {
            #[cfg(feature = "midi")]
            Self::ClipEvents(node) | Self::Harmony(node) => Some(node),
            _ => None,
        }
    }
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

/// The values of a node that the session's verbs reach, besides its params:
/// what [`SessionNode::slots`](crate::SessionNode::slots) answers, kept under
/// the node's key until the node is removed or replaced.
#[derive(Default)]
pub struct NodeSlots(pub(crate) Vec<(SlotId, Erased)>);

impl NodeSlots {
    /// `slot` as the value `id` names.
    #[cfg(feature = "midi")]
    pub(crate) fn of<V: 'static>(id: SlotId, slot: SlotCell<V>) -> Self {
        Self(vec![(id, erase(slot))])
    }
}

impl std::fmt::Debug for NodeSlots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeSlots").finish_non_exhaustive()
    }
}

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

slot_target!(
    /// Where the mix lands ([`OutputPolicy`](crate::OutputPolicy)), as
    /// `session.policy::<OutputPolicy>()` names it. A write is checked
    /// against the device and the root now, staged, and applied by the next
    /// [`Session::update`](crate::Session::update), live. It is refused with
    /// [`SetRefusal::OutputPolicy`] when it has no place.
    OutputPolicyTarget,
    crate::OutputPolicy,
    SlotId::OutputPolicy
);

#[cfg(feature = "midi-hardware")]
slot_target!(
    /// Who the session is on the MIDI wire ([`MidiPolicy`](crate::MidiPolicy)),
    /// as `session.policy::<MidiPolicy>()` names it. A write is applied by the
    /// next [`Session::update`](crate::Session::update), and is refused with
    /// [`SetRefusal::MidiPolicy`] for an identity on a session with no OS
    /// MIDI layer.
    MidiPolicyTarget,
    crate::MidiPolicy,
    SlotId::MidiPolicy
);

#[cfg(feature = "midi")]
slot_target!(
    /// The MIDI outputs' own latency and offset: what a MIDI device takes to
    /// sound an event, and a trim on top, as `session.midi().output_latency()`
    /// names it. It applies from the next drain, to every output: it is one
    /// setting of the session, not an output's.
    OutputLatencyTarget,
    tutti_midi_runtime::OutputLatency,
    SlotId::OutputLatency
);

/// Writes the target, `Target`, `Land` and builders of a slot target on a node
/// of the graph: its `SlotId` names the node, and the value type is fixed.
#[cfg(feature = "midi")]
macro_rules! node_slot_target {
    ($(#[$meta:meta])* $name:ident, $value:ty, $id:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(NodeKey);

        impl $name {
            pub(crate) const fn new(node: NodeKey) -> Self {
                Self(node)
            }

            /// The node the target is on.
            #[must_use]
            pub const fn node(self) -> NodeKey {
                self.0
            }
        }

        impl sealed::Sealed for $name {
            fn id(self) -> TargetId {
                TargetId::Slot(SlotId::$id(self.0))
            }
        }
        impl Target for $name {
            type Value = $value;
        }
        impl sealed::Land for $name {
            type Cell = SlotCell<$value>;

            fn resolve(self, params: &Params) -> Option<(Self::Cell, $value)> {
                let cell = params.slot::<$value>(SlotId::$id(self.0))?;
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

#[cfg(feature = "midi")]
node_slot_target!(
    /// The events a MIDI clip node plays, as `clip.events()` names them: a
    /// [`ClipEvents`](tutti_midi_runtime::ClipEvents), built by collecting
    /// [`TimedMidiEvent`](tutti_midi_runtime::TimedMidiEvent)s, which sorts
    /// them, pairs each note-off with its note-on and builds the chase table
    /// where the clip is built.
    ///
    /// The clip is the node's from its next block, which ends the notes the
    /// last one left sounding. A value equal to the clip in force (the same
    /// events and note pairing) changes nothing, so rebuilding the same clip
    /// cuts no note. A read answers the clip in force.
    ClipEventsTarget,
    Arc<tutti_midi_runtime::ClipEvents>,
    ClipEvents
);

#[cfg(feature = "midi")]
node_slot_target!(
    /// The chord and scale changes a harmony node sends, as `harmony.changes()`
    /// names them: [`HarmonyChanges`](tutti_midi_runtime::HarmonyChanges),
    /// built by collecting [`TimedHarmony`](tutti_midi_runtime::TimedHarmony)s
    /// in any order.
    ///
    /// The changes are the node's from its next block, which re-states the
    /// context in force on that block's first frame. A value equal to the
    /// changes in force changes nothing. A read answers the changes in force.
    HarmonyTarget,
    Arc<tutti_midi_runtime::HarmonyChanges>,
    Harmony
);

#[cfg(feature = "midi")]
impl crate::Key<tutti_midi_runtime::MidiClipNode> {
    /// The events the clip plays.
    #[must_use]
    pub const fn events(self) -> ClipEventsTarget {
        ClipEventsTarget::new(self.erase())
    }
}

#[cfg(feature = "midi")]
impl crate::Key<tutti_midi_runtime::HarmonyNode> {
    /// The chord and scale changes the node sends.
    #[must_use]
    pub const fn changes(self) -> HarmonyTarget {
        HarmonyTarget::new(self.erase())
    }
}

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

/// A node's control side that holds one published value: what a
/// [`PublishedSlot`] reads and replaces it through.
#[cfg(feature = "midi")]
pub(crate) trait Held: Send + Sync + 'static {
    /// The value in force.
    type Value;

    /// The value now.
    fn current(&self) -> Self::Value;

    /// Replaces the value; an equal one changes nothing.
    fn replace(&self, value: &Self::Value);
}

#[cfg(feature = "midi")]
impl Held for tutti_midi_runtime::MidiClipControls {
    type Value = Arc<tutti_midi_runtime::ClipEvents>;

    fn current(&self) -> Self::Value {
        Self::current(self)
    }

    fn replace(&self, value: &Self::Value) {
        self.set(Arc::clone(value));
    }
}

#[cfg(feature = "midi")]
impl Held for tutti_midi_runtime::HarmonyControls {
    type Value = Arc<tutti_midi_runtime::HarmonyChanges>;

    fn current(&self) -> Self::Value {
        Self::current(self)
    }

    fn replace(&self, value: &Self::Value) {
        self.set(Arc::clone(value));
    }
}

/// A value a node reads through a published cell, written through its
/// control side ([`Held`]), which publishes when the value differs from the
/// one in force.
#[cfg(feature = "midi")]
pub(crate) struct PublishedSlot<C>(pub C);

#[cfg(feature = "midi")]
impl<C: Held> Slot<C::Value> for PublishedSlot<C>
where
    C::Value: Send + Sync + 'static,
{
    fn load(&self) -> C::Value {
        self.0.current()
    }

    fn check(&self, _: &C::Value) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &C::Value) {
        self.0.replace(value);
    }
}

/// The click's level.
pub(crate) struct ClickVolumeSlot(pub Arc<ClickSettings>);

impl Slot<Amplitude> for ClickVolumeSlot {
    fn load(&self) -> Amplitude {
        self.0.volume().get()
    }

    fn check(&self, value: &Amplitude) -> Result<(), SetRefusal> {
        match ClickLevel::new(*value) {
            Some(_) => Ok(()),
            None if !value.get().is_finite() => Err(SetRefusal::NotFinite),
            None => Err(SetRefusal::OutOfRange),
        }
    }

    fn store(&self, value: &Amplitude) {
        let level = ClickLevel::new(*value).expect("`check` accepted the level");
        self.0.store_volume(level);
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

/// A control-side value the session reads when it applies it: the policy
/// last written, and whether it was written since it was last taken.
pub(crate) struct Cell<V> {
    value: std::sync::Mutex<V>,
    staged: std::sync::atomic::AtomicBool,
    /// Whether `value` may be stored: the part of the check that reads the
    /// session's own state.
    check: Box<dyn Fn(&V) -> Result<(), SetRefusal> + Send + Sync>,
}

impl<V> std::fmt::Debug for Cell<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cell").finish_non_exhaustive()
    }
}

impl<V: Clone> Cell<V> {
    /// A cell holding `value`, nothing staged, storing what `check` accepts.
    pub(crate) fn new(
        value: V,
        check: impl Fn(&V) -> Result<(), SetRefusal> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            value: std::sync::Mutex::new(value),
            staged: std::sync::atomic::AtomicBool::new(false),
            check: Box::new(check),
        })
    }

    /// The value now: the one in force, or the one written and not yet taken.
    pub(crate) fn current(&self) -> V {
        self.value.lock_recover().clone()
    }

    /// The value written since the last call, once.
    pub(crate) fn take_staged(&self) -> Option<V> {
        self.staged
            .swap(false, Ordering::AcqRel)
            .then(|| self.current())
    }
}

impl<V: Clone + Send + Sync + 'static> Slot<V> for Cell<V> {
    fn load(&self) -> V {
        self.current()
    }

    fn check(&self, value: &V) -> Result<(), SetRefusal> {
        (self.check)(value)
    }

    fn store(&self, value: &V) {
        *self.value.lock_recover() = value.clone();
        self.staged.store(true, Ordering::Release);
    }
}
