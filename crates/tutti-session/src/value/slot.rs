//! Targets on the session's own values: the project's tempo and meter, the
//! click's level and mode. Each lives in a cell the session owns, registered
//! in the value table when the session is built, and is written and read with
//! the session's verbs like a node's param.

use std::any::{Any, TypeId};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_graph::{ErasedPublished, ErasedSetting, NodeKey, SettingCell};
use tutti_music::MeterMap;
use tutti_nodes::{ClickLevel, ClickSettings, MetronomeMode};
use tutti_rt::{AtomicCell, AtomicWord, Mutex};
use tutti_tempo::TempoMap;
use tutti_timeline::{LoopEdit, LoopRegion, ProjectMeter, Transport, TransportSettings};
use tutti_units::{Amplitude, At, SettingKind};

use super::error::SetRefusal;
use super::saved::capture;
use super::state::Params;
use super::target::{AnyAssign, Modulatable, Target, TargetId, sealed, target_builders};
use tutti_modulation::TargetKey;

use crate::{ChannelEdge, ModRouteId, ModSourceId, ModTarget};

/// Which of the session's own values a slot target names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotId {
    /// The project's tempo map.
    Tempo,
    /// The project's meter.
    Meter,
    /// The loop region.
    Loop,
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
    /// The session's MPE mode.
    #[cfg(feature = "midi")]
    MpeMode,
    /// The MPE 1.1 zones of one Group of a MIDI output port's device.
    #[cfg(feature = "midi-hardware")]
    Mpe11((NodeKey, tutti_ump::MidiGroup)),
    /// The events a MIDI clip node plays.
    #[cfg(feature = "midi")]
    ClipEvents(NodeKey),
    /// The chord and scale changes a harmony node sends.
    #[cfg(feature = "midi")]
    Harmony(NodeKey),
    /// Where a sampler voice sits on the transport.
    #[cfg(feature = "sampler")]
    Window(NodeKey),
    /// A sampler voice's loop.
    #[cfg(feature = "sampler")]
    Looping(NodeKey),
    /// Which way a sampler voice plays.
    #[cfg(feature = "sampler")]
    Direction(NodeKey),
    /// A sampler voice's varispeed.
    #[cfg(feature = "sampler")]
    Speed(NodeKey),
    /// How a modulation route maps its source's value onto its span.
    RoutePolarity(ShapeKey),
    /// The law that shapes a modulation route's value.
    RouteCurve(ShapeKey),
    /// An LFO source's rate.
    SourceRate(ModSourceId),
    /// A macro source's value.
    SourceValue(ModSourceId),
    /// A param's automation curve, by the key the compile holds it under.
    Automation(TargetKey),
    /// A node's setting of a kind, the word it reads once per block.
    Setting(NodeKey, SettingKind),
    /// A node's mode.
    Mode(NodeKey),
    /// A node's mix.
    Mix(NodeKey),
    /// The gain of an edge.
    Gain(ChannelEdge),
    /// The range declared on a modulation target.
    Range(ModTarget),
    /// The automation mode the host tells a plugin.
    #[cfg(feature = "plugin")]
    PluginAutomationMode(NodeKey),
    /// The track info the host tells a plugin.
    #[cfg(feature = "plugin")]
    PluginTrackInfo(NodeKey),
    /// The app's MIDI map of a plugin.
    #[cfg(feature = "plugin")]
    PluginMidiMap(NodeKey),
    /// The depth of the route from a source into a target.
    RouteDepth(DepthKey),
}

impl SlotId {
    /// Whether a project saves the value: the one answer, which
    /// [`SavedValues`](crate::SavedValues) captures by. A policy, a MIDI
    /// device's latency and a plugin's automation mode and track info are the session's
    /// setup of a place, not of the project, so a project moved to another
    /// machine or another plugin does not carry them.
    pub(crate) const fn saved(self) -> bool {
        match self {
            Self::OutputPolicy => false,
            #[cfg(feature = "midi-hardware")]
            Self::MidiPolicy => false,
            #[cfg(feature = "midi")]
            Self::OutputLatency => false,
            #[cfg(feature = "midi-hardware")]
            Self::Mpe11(_) => false,
            #[cfg(feature = "plugin")]
            Self::PluginAutomationMode(_) | Self::PluginTrackInfo(_) => false,
            #[cfg(feature = "plugin")]
            Self::PluginMidiMap(_) => true,
            Self::Tempo
            | Self::Meter
            | Self::Loop
            | Self::ClickVolume
            | Self::ClickMode
            | Self::RoutePolarity(_)
            | Self::RouteCurve(_)
            | Self::SourceRate(_)
            | Self::SourceValue(_)
            | Self::Automation(_)
            | Self::Setting(..)
            | Self::Mode(_)
            | Self::Mix(_)
            | Self::Gain(_)
            | Self::Range(_)
            | Self::RouteDepth(_) => true,
            #[cfg(feature = "midi")]
            Self::MpeMode | Self::ClipEvents(_) | Self::Harmony(_) => true,
            #[cfg(feature = "sampler")]
            Self::Window(_) | Self::Looping(_) | Self::Direction(_) | Self::Speed(_) => true,
        }
    }

    /// The node the value is of: its params' automation, its mode and mix, its
    /// settings and what its kind holds. `None` for a value of the session, of an
    /// edge or of a declaration (a source, a route, a range). Every variant is
    /// listed, so a new one chooses.
    pub(crate) const fn node(self) -> Option<NodeKey> {
        match self {
            Self::Setting(node, _) | Self::Mode(node) | Self::Mix(node) => Some(node),
            #[cfg(feature = "midi")]
            Self::ClipEvents(node) | Self::Harmony(node) => Some(node),
            #[cfg(feature = "sampler")]
            Self::Window(node)
            | Self::Looping(node)
            | Self::Direction(node)
            | Self::Speed(node) => Some(node),
            #[cfg(feature = "midi-hardware")]
            Self::Mpe11((node, _)) => Some(node),
            #[cfg(feature = "plugin")]
            Self::PluginAutomationMode(node)
            | Self::PluginTrackInfo(node)
            | Self::PluginMidiMap(node) => Some(node),
            Self::Automation(TargetKey::Param { node, .. } | TargetKey::Mix(node)) => {
                Some(NodeKey(node))
            }
            Self::Automation(TargetKey::Edge(_))
            | Self::Tempo
            | Self::Meter
            | Self::Loop
            | Self::ClickVolume
            | Self::ClickMode
            | Self::OutputPolicy
            | Self::RoutePolarity(_)
            | Self::RouteCurve(_)
            | Self::SourceRate(_)
            | Self::SourceValue(_)
            | Self::Gain(_)
            | Self::Range(_)
            | Self::RouteDepth(_) => None,
            #[cfg(feature = "midi-hardware")]
            Self::MidiPolicy => None,
            #[cfg(feature = "midi")]
            Self::OutputLatency | Self::MpeMode => None,
        }
    }

    /// The node whose *instance* holds the value, or `None` for a value that
    /// outlives a replace of the node: an automation, a mode and a mix are
    /// declarations on the node's key, so a replacement keeps them.
    pub(crate) const fn instance_node(self) -> Option<NodeKey> {
        match self {
            Self::Automation(_) | Self::Mode(_) | Self::Mix(_) => None,
            _ => self.node(),
        }
    }
}

/// A session value behind a target: how it is read, checked and stored.
/// Control side only: every call runs under the value lock.
pub trait Slot<V>: Send + Sync + 'static {
    /// The value now.
    fn load(&self) -> V;

    /// The value the audio thread uses: the stored one, unless a live source
    /// moves it from the base ([`BaseSlot`](super::base::BaseSlot)).
    fn effective(&self) -> V {
        self.load()
    }

    /// Whether `value` may be stored.
    fn check(&self, value: &V) -> Result<(), SetRefusal>;

    /// Stores `value`. Infallible once `check` passed.
    fn store(&self, value: &V);
}

/// The cell a slot target lands through.
pub(crate) type SlotCell<V> = Arc<dyn Slot<V>>;

/// What a slot is registered as: the type-erased [`SlotCell`] of its value,
/// and, for a staging slot, the handle the update publishes it through.
pub(crate) struct Erased {
    cell: Box<dyn Any + Send + Sync>,
    stage: Option<Arc<dyn Stage>>,
    /// The assignment of the value now, made from the cell's value type.
    capture: fn(SlotId, &(dyn Any + Send + Sync)) -> AnyAssign,
}

impl Erased {
    /// The assignment of the value `id` holds now.
    pub(super) fn capture(&self, id: SlotId) -> AnyAssign {
        (self.capture)(id, &*self.cell)
    }
}

impl std::fmt::Debug for Erased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Erased")
            .field("staging", &self.stage.is_some())
            .finish_non_exhaustive()
    }
}

/// An atomic slot: its writes land at once.
pub(crate) fn erase<V: Clone + PartialEq + Send + Sync + 'static>(slot: SlotCell<V>) -> Erased {
    Erased {
        cell: Box::new(slot),
        stage: None,
        capture: capture::<V>,
    }
}

/// A staging slot, such as a [`PublishedSlot`]: its writes wait for
/// [`Params::publish_pending`].
pub(crate) fn erase_staged<V: Clone + PartialEq + Send + Sync + 'static, S: Slot<V> + Stage>(
    slot: Arc<S>,
) -> Erased {
    Erased {
        cell: Box::new(Arc::clone(&slot) as SlotCell<V>),
        stage: Some(slot),
        capture: capture::<V>,
    }
}

/// A slot whose writes wait in a pending value until the update.
/// Control side only: every call runs under the value lock.
pub(crate) trait Stage: Send + Sync + 'static {
    /// Publishes the pending value, if there is one.
    fn publish_pending(&self);
}

impl Params {
    pub(super) fn slot<V: 'static>(&self, id: SlotId) -> Option<SlotCell<V>> {
        self.slots
            .get(&id)?
            .cell
            .downcast_ref::<SlotCell<V>>()
            .cloned()
    }

    /// Publishes every pending value, in [`SlotId`] order: the tempo before
    /// the meter, the session's own before a node's, a node's by key.
    pub(super) fn publish_pending(&self) {
        for erased in self.slots.values() {
            if let Some(stage) = &erased.stage {
                stage.publish_pending();
            }
        }
    }
}

/// Writes the target, `Target`, `Store` and builders of a slot target whose
/// value type is fixed.
macro_rules! slot_target {
    (moved $(#[$meta:meta])* $name:ident, $value:ty, $id:expr) => {
        slot_target!(@define [$(#[$meta])*] $name, $value, $id);
        target_builders!(moved [] $name);
    };
    ($(#[$meta:meta])* $name:ident, $value:ty, $id:expr) => {
        slot_target!(@define [$(#[$meta])*] $name, $value, $id);
        target_builders!([] $name);
    };
    (@define [$(#[$meta:meta])*] $name:ident, $value:ty, $id:expr) => {
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
        impl sealed::Store for $name {
            type Cell = SlotCell<$value>;

            fn resolve(self, params: &Params) -> Option<(Self::Cell, $value)> {
                let cell = params.slot::<$value>($id)?;
                let base = cell.load();
                Some((cell, base))
            }

            fn check(cell: &Self::Cell, value: &$value) -> Result<(), SetRefusal> {
                cell.check(value)
            }

            fn store(cell: &Self::Cell, value: &$value) {
                cell.store(value);
            }
        }
    };
}

slot_target!(
    moved
    /// The project's tempo: the map the engine takes at the first block after
    /// the next [`Session::update`](crate::Session::update), as
    /// `session.transport_handle().tempo()` names it.
    ///
    /// A constant tempo is a one-step map, `tempo.to(TempoMap::constant(bpm)?)`;
    /// a map by value works as it stands. The value is the **base**, the map
    /// the app writes: a write is read back at once, and the update publishes
    /// it with the meter and the other staged values. A live tempo the session
    /// follows ([`Midi::follow_tempo`](crate::Midi::follow_tempo)) lies over
    /// the base: [`effective`](Self::effective) reads the map the engine uses,
    /// the base up to the first live step and the live steps after it, and the
    /// base while nothing is followed.
    TempoTarget,
    Arc<TempoMap>,
    SlotId::Tempo
);

slot_target!(
    moved
    /// The project's meter, which the metronome and every hosted plugin read
    /// once per block, as `session.transport_handle().meter()` names it. The
    /// value is the **base**, as the tempo's is: a write is read back at once
    /// and published by the next [`Session::update`](crate::Session::update),
    /// and [`effective`](Self::effective) reads the meter in force, with the
    /// time signatures a followed source set.
    MeterTarget,
    Arc<MeterMap>,
    SlotId::Meter
);

impl Modulatable for TempoTarget {}
impl Modulatable for MeterTarget {}

impl sealed::Lane for TempoTarget {
    fn delivered(self, params: &Params) -> Option<Arc<TempoMap>> {
        params
            .slot::<Arc<TempoMap>>(SlotId::Tempo)
            .map(|cell| cell.effective())
    }
}

impl sealed::Lane for MeterTarget {
    fn delivered(self, params: &Params) -> Option<Arc<MeterMap>> {
        params
            .slot::<Arc<MeterMap>>(SlotId::Meter)
            .map(|cell| cell.effective())
    }
}

slot_target!(
    /// The loop region: its bounds, kept while looping is disarmed, and
    /// whether looping is armed ([`LoopRegion`]), as
    /// `session.transport_handle().looping()` names it.
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update) as one edit for the next
    /// block, so the engine takes the bounds and the arming together; a
    /// read answers the pending region, else the one sent and not yet
    /// applied, else the one the engine last published. Disarm
    /// with `looping.modify(|r| LoopRegion::new(r.bounds(), false))`. A
    /// change at a frame or beat is a [`LoopEdit`] scheduled on the
    /// transport.
    LoopTarget,
    LoopRegion,
    SlotId::Loop
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
    /// against the device and the main output now, staged, and applied by the next
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

/// Writes the target, `Target`, `Store` and builders of a slot target whose
/// `SlotId` is built from a key of the thing it is on (`$key`), with the
/// value type fixed.
macro_rules! keyed_slot_target {
    ($(#[$meta:meta])* $name:ident, $value:ty, $key:ty, $id:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name($key);

        impl $name {
            pub(crate) const fn new(key: $key) -> Self {
                Self(key)
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
        impl sealed::Store for $name {
            type Cell = SlotCell<$value>;

            fn resolve(self, params: &Params) -> Option<(Self::Cell, $value)> {
                let cell = params.slot::<$value>(SlotId::$id(self.0))?;
                let base = cell.load();
                Some((cell, base))
            }

            fn check(cell: &Self::Cell, value: &$value) -> Result<(), SetRefusal> {
                cell.check(value)
            }

            fn store(cell: &Self::Cell, value: &$value) {
                cell.store(value);
            }
        }
        target_builders!([] $name);
    };
}
pub(super) use keyed_slot_target;

/// Writes the target, `Target`, `Store` and builders of a slot target on a node
/// of the graph: its `SlotId` names the node, and the value type is fixed.
#[cfg(any(feature = "midi", feature = "sampler"))]
macro_rules! node_slot_target {
    ($(#[$meta:meta])* $name:ident, $value:ty, $id:ident) => {
        keyed_slot_target!($(#[$meta])* $name, $value, NodeKey, $id);

        impl $name {
            /// The node the target is on.
            #[must_use]
            pub const fn node(self) -> NodeKey {
                self.0
            }
        }
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
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update); the clip is the node's
    /// from its next block after that, which ends the notes the last one left
    /// sounding. A value equal to the clip in force (the same events and note
    /// pairing) changes nothing, so rebuilding the same clip cuts no note. A
    /// read answers the staged clip, else the one in force. A write to a node
    /// removed before the update is dropped.
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
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update); the changes are the node's
    /// from its next block after that, which re-states the context in force
    /// on that block's first frame. A value equal to the changes in force
    /// changes nothing. A read answers the staged changes, else the ones in
    /// force.
    HarmonyTarget,
    Arc<tutti_midi_runtime::HarmonyChanges>,
    Harmony
);

#[cfg(feature = "sampler")]
node_slot_target!(
    /// Where a sampler voice sits on the transport, as `sampler.window()`
    /// names it: `Some` places it on the transport at a
    /// [`VoiceWindow`](tutti_sampler::VoiceWindow), `None` leaves it
    /// free-running ([`play`](tutti_sampler::VoiceNodeHandle::play) and
    /// [`stop`](tutti_sampler::VoiceNodeHandle::stop) drive such a voice and
    /// refuse a placed one with [`PlayError::Placed`](tutti_sampler::PlayError)).
    /// A streamed voice is always placed: `None` leaves it at its window.
    /// Stretch applies only to a placed voice, so a stretched clip given
    /// `None` plays dry.
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update); the voice reads the window
    /// from its next block after that. A read answers the staged window, else
    /// the one in force. A write to a node removed before the update is
    /// dropped.
    WindowTarget,
    Option<tutti_sampler::VoiceWindow>,
    Window
);

#[cfg(feature = "sampler")]
node_slot_target!(
    /// A sampler voice's loop, as `sampler.looping()` names it: `Some` plays
    /// the [`Loop`](tutti_sampler::Loop), `None` plays once.
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update). The voice's node applies
    /// it to a resident source at its next block after that. For a streamed
    /// voice the streamer reads the same cell on its next cycle and applies
    /// the loop to the ring's layout, which the node reads: the session
    /// forwards nothing. A read answers the staged loop, else the one in
    /// force. A write to a node removed before the update is dropped.
    LoopingTarget,
    Option<tutti_sampler::Loop>,
    Looping
);

#[cfg(feature = "sampler")]
node_slot_target!(
    /// Which way a sampler voice plays, as `sampler.direction()` names it:
    /// [`Direction::Reverse`](tutti_sampler::PlayDirection) reads the source from
    /// its end and ignores the loop.
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update), and lands as the loop's
    /// does: a resident voice at its next block, a streamed one through the
    /// streamer's next cycle. A free-running voice (no window) turned mid-play
    /// jumps rather than continuing: its cursor `p` then reads the file
    /// mirrored, frame `len - 1 - p`, and goes on backwards from there. A read
    /// answers the staged direction, else the one in force. A write to a node removed before the update is dropped.
    DirectionTarget,
    tutti_sampler::PlayDirection,
    Direction
);

#[cfg(feature = "sampler")]
node_slot_target!(
    /// A sampler voice's varispeed, as `sampler.speed()` names it: the rate
    /// the voice reads its source at, pitch following it
    /// ([`PlaybackRate::UNITY`](tutti_units::PlaybackRate) is normal speed).
    /// A rate that is not finite is refused with [`SetRefusal::NotFinite`],
    /// and one outside [`MIN`](tutti_units::PlaybackRate::MIN)..=
    /// [`MAX`](tutti_units::PlaybackRate::MAX) with [`SetRefusal::OutOfRange`].
    ///
    /// A write is staged and published by the next
    /// [`Session::update`](crate::Session::update); the voice's node reads it
    /// at its next block after that, on either tier (a streamed voice's node
    /// hands it on to the stream's playback cells). A read answers the staged
    /// rate, else the one in force. A write to a node removed before the
    /// update is dropped.
    SpeedTarget,
    tutti_units::PlaybackRate,
    Speed
);

/// Generates a [`Key`](crate::Key)'s method for each setting kind, from the
/// node's [`HasSetting`](tutti_graph::HasSetting): only a node that has the
/// setting has the method.
macro_rules! setting_methods {
    ($($name:ident => $kind:ident, $marker:ident, $doc:literal;)+) => {
        impl<N: ?Sized> crate::Key<N> {$(
            #[doc = $doc]
            #[must_use]
            pub const fn $name(
                self,
            ) -> SettingTarget<<N as tutti_graph::HasSetting<tutti_units::$marker>>::Word>
            where
                N: tutti_graph::HasSetting<tutti_units::$marker>,
            {
                SettingTarget::new(SlotId::Setting(self.erase(), SettingKind::$kind))
            }
        )+}
    };
}

setting_methods! {
    sync => Sync, SyncSetting,
        "How the node is timed, free or locked to the beat (only a node with the setting has this method).";
    key_filter => KeyFilter, KeyFilterSetting,
        "Which filter the detector hears its key through (only a node with the setting has this method).";
}

/// Generates a [`Key`](crate::Key)'s method for each published kind, from the
/// node's [`HasPublished`](tutti_graph::HasPublished): only a node that
/// publishes the kind has the method.
macro_rules! published_methods {
    ($($(#[$cfg:meta])* $name:ident, $target:ident => $kind:ty, $doc:literal;)+) => {
        impl<N: ?Sized> crate::Key<N> {$(
            #[doc = $doc]
            $(#[$cfg])*
            #[must_use]
            pub const fn $name(self) -> $target
            where
                N: tutti_graph::HasPublished<$kind>,
            {
                $target::new(self.erase())
            }
        )+}
    };
}

published_methods! {
    #[cfg(feature = "midi")]
    events, ClipEventsTarget => tutti_midi_runtime::ClipEventsKind,
        "The events the clip plays (only a node that publishes them has this method).";
    #[cfg(feature = "midi")]
    changes, HarmonyTarget => tutti_midi_runtime::HarmonyChangesKind,
        "The chord and scale changes the node sends (only a node that publishes them has this method).";
    #[cfg(feature = "sampler")]
    window, WindowTarget => tutti_sampler::WindowKind,
        "The window the voice plays on the transport (only a node that publishes it has this method).";
    #[cfg(feature = "sampler")]
    looping, LoopingTarget => tutti_sampler::LoopingKind,
        "The voice's loop (only a node that publishes it has this method).";
    #[cfg(feature = "sampler")]
    direction, DirectionTarget => tutti_sampler::DirectionKind,
        "Which way the voice plays (only a node that publishes it has this method).";
    #[cfg(feature = "sampler")]
    speed, SpeedTarget => tutti_sampler::SpeedKind,
        "The voice's varispeed (only a node that publishes it has this method).";
}

#[cfg(feature = "midi")]
slot_target!(
    /// The session's MPE mode ([`MpeMode`](tutti_midi_protocol::MpeMode)), as
    /// `session.mpe()` names it: how the hardware inputs are read and how
    /// every clip's MIDI is transformed. It begins as the builder's
    /// ([`SessionBuilder::mpe_mode`](crate::SessionBuilder::mpe_mode)),
    /// `Disabled` by default.
    ///
    /// It is one value for everything that follows it: each input's input node
    /// and each clip's MPE transform (the node [`Midi::play_clip`](crate::Midi::play_clip)
    /// wires between a clip and its target) adopts it at its next block, and
    /// a clip played later starts in it. A new mode stops and resets the
    /// channels that enter or leave MPE control, and the others keep their
    /// notes; the mode in force changes nothing. The transforms are the one
    /// holder of the zones: an instrument, whenever it was added, reads
    /// per-note and per-channel messages and no zone. A MIDI source played
    /// into an instrument by hand is not classic MPE and is not transformed.
    ///
    /// A device that enables the MIDI-CI MPE Profile (M2-120) is not under this
    /// mode: its input follows the Profile's zones and rules, in the Groups it
    /// enabled them on, until it disables the Profile or goes. The session
    /// does that from the peer's Profile reports (`MidiEvent::CiProfiles`).
    MpeModeTarget,
    tutti_midi_protocol::MpeMode,
    SlotId::MpeMode
);

#[cfg(feature = "midi-hardware")]
keyed_slot_target!(
    /// The MPE 1.1 (M1-100) zones an app chooses for one Group of a MIDI output
    /// port's device, as `Mpe11Target::of(output.node(), group)` names them:
    /// `Some` sends the Configuration Message of each zone and then MPE to it,
    /// `None` ends the zones (a Configuration Message with no Member Channels).
    /// It is the alternate translation M2-104 D.4.2 allows for a device that
    /// has no MPE Profile: an MPE Profile the device enables takes precedence
    /// while it is enabled, and the zones here are used again after it ends.
    ///
    /// A write lands in the next [`Session::update`](crate::Session::update).
    /// The value is kept across an unplug and a replug of the port: the device
    /// that returns is sent the zones in force. A read answers the value last
    /// written, `None` before any. The target exists while the node is a MIDI
    /// output port of the session ([`MidiOutput::node`](crate::MidiOutput::node));
    /// a call that names any other node, or a port that has left, fails with
    /// `NoSuchTarget`.
    Mpe11Target,
    Option<tutti_midi_protocol::Mpe11Zones>,
    (NodeKey, tutti_ump::MidiGroup),
    Mpe11
);

#[cfg(feature = "midi-hardware")]
impl Mpe11Target {
    /// The zones of `group` of the device output port at `node`
    /// ([`MidiOutput::node`](crate::MidiOutput::node)).
    #[must_use]
    pub fn of(node: impl Into<NodeKey>, group: tutti_ump::MidiGroup) -> Self {
        Self::new((node.into(), group))
    }
}

/// A setting held in one atomic word ([`AtomicWord`]): the click's
/// [`MetronomeMode`], as `session.click().mode()` names it, or a node's, as its
/// key names it (`key.sync()`, `key.key_filter()`). The audio thread reads it
/// once per block; a write lands at once.
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

impl<T: AtomicWord + PartialEq + Send + Sync + 'static> sealed::Sealed for SettingTarget<T> {
    fn id(self) -> TargetId {
        TargetId::Slot(self.id)
    }
}
impl<T: AtomicWord + PartialEq + Send + Sync + 'static> Target for SettingTarget<T> {
    type Value = T;
}
impl<T: AtomicWord + PartialEq + Send + Sync + 'static> sealed::Store for SettingTarget<T> {
    type Cell = SlotCell<SettingWord>;

    /// A slot whose word is another enum's is no target of `T`'s.
    fn resolve(self, params: &Params) -> Option<(Self::Cell, T)> {
        let cell = params.slot::<SettingWord>(self.id)?;
        let word = cell.load();
        if word.of != TypeId::of::<T>() {
            return None;
        }
        Some((cell, T::from_word(word.word)?))
    }

    fn check(cell: &Self::Cell, value: &T) -> Result<(), SetRefusal> {
        cell.check(&SettingWord::of(*value))
    }

    fn store(cell: &Self::Cell, value: &T) {
        cell.store(&SettingWord::of(*value));
    }
}
target_builders!([T: AtomicWord + PartialEq + Send + Sync + 'static] SettingTarget<T>);

// The session's cells.

/// A control side that holds one published value: what a [`PublishedSlot`]
/// reads, checks and replaces it through.
pub(crate) trait Held: Send + Sync + 'static {
    /// The value in force.
    type Value: Clone + PartialEq + Send + Sync + 'static;

    /// The value now.
    fn current(&self) -> Self::Value;

    /// Whether `value` may be published, when it is written and when it is
    /// published.
    fn check(&self, _: &Self::Value) -> Result<(), SetRefusal> {
        Ok(())
    }

    /// Publishes `value`; an equal one may change nothing. Whether it was
    /// taken: a refusal leaves the value pending for the next update.
    fn replace(&self, value: &Self::Value) -> bool;
}

/// A value held through its control side ([`Held`]). A write stages a
/// pending value, a later one replacing it; [`Slot::load`] answers the
/// pending value, else the one in force; the update publishes the pending
/// value ([`Params::publish_pending`]).
pub(crate) struct PublishedSlot<C: Held> {
    cell: C,
    pending: Mutex<Option<C::Value>>,
}

impl<C: Held> PublishedSlot<C> {
    pub(crate) fn new(cell: C) -> Arc<Self> {
        Arc::new(Self {
            cell,
            pending: Mutex::new(None),
        })
    }
}

impl<C: Held> Slot<C::Value> for PublishedSlot<C> {
    fn load(&self) -> C::Value {
        let pending = self.pending.lock().clone();
        pending.unwrap_or_else(|| self.cell.current())
    }

    fn check(&self, value: &C::Value) -> Result<(), SetRefusal> {
        self.cell.check(value)
    }

    fn store(&self, value: &C::Value) {
        *self.pending.lock() = Some(value.clone());
    }
}

impl<C: Held> Stage for PublishedSlot<C> {
    fn publish_pending(&self) {
        let mut pending = self.pending.lock();
        let Some(value) = pending.take() else {
            return;
        };
        // A value the check no longer accepts is dropped, as its write would
        // now be refused.
        if self.cell.check(&value).is_ok() && !self.cell.replace(&value) {
            *pending = Some(value);
        }
    }
}

/// A node's published cell as a slot's control side: what the node handed its
/// [`NodeParts`](tutti_graph::NodeParts) as a [`Published`](tutti_graph::Published).
#[cfg(any(feature = "midi", feature = "sampler"))]
pub(crate) struct NodeCell<V>(Arc<dyn tutti_graph::Published<Value = V>>);

#[cfg(any(feature = "midi", feature = "sampler"))]
impl<V: Clone + PartialEq + Send + Sync + 'static> Held for NodeCell<V> {
    type Value = V;

    fn current(&self) -> V {
        self.0.current()
    }

    fn replace(&self, value: &V) -> bool {
        self.0.replace(value);
        true
    }
}

/// `cell` as the slot of its value type.
#[cfg(any(feature = "midi", feature = "sampler"))]
fn node_cell<V: Clone + PartialEq + Send + Sync + 'static>(
    cell: Arc<dyn tutti_graph::Published<Value = V>>,
) -> Erased {
    erase_staged::<V, _>(PublishedSlot::new(NodeCell(cell)))
}

/// The slot a node's published cell is registered as under `node`, when its
/// kind is one the session knows (each kind here has the value type its
/// session target holds): a node that publishes another kind has nothing that
/// can address its value, and is left out.
#[cfg_attr(
    not(any(feature = "midi", feature = "sampler")),
    allow(unused_variables, reason = "no kind is addressed without a feature")
)]
pub(crate) fn slot_of(node: NodeKey, entry: &ErasedPublished) -> Option<(SlotId, Erased)> {
    published_kinds!(K, id => {
        let cell = entry.cell::<K>()?;
        #[cfg(any(feature = "midi", feature = "sampler"))]
        return Some((id(node), node_cell(cell)));
        #[cfg(not(any(feature = "midi", feature = "sampler")))]
        {
            let _ = (cell, id, node);
            None
        }
    })
}

/// The first `Some` that `$body` answers for a published kind the session
/// addresses, with `$kind` the kind's type and `$id` the function that makes
/// its slot of a node: the one list of those kinds. A node that publishes
/// another kind has nothing that can address its value, and the session
/// leaves it out.
macro_rules! published_kinds {
    ($kind:ident, $id:ident => $body:expr) => {{
        #[allow(unused_mut, reason = "no kind is addressed without a feature")]
        let mut answer = None;
        #[cfg(feature = "midi")]
        {
            answer = answer.or_else(|| {
                type $kind = tutti_midi_runtime::ClipEventsKind;
                let $id: fn(NodeKey) -> SlotId = SlotId::ClipEvents;
                (|| $body)()
            });
            answer = answer.or_else(|| {
                type $kind = tutti_midi_runtime::HarmonyChangesKind;
                let $id: fn(NodeKey) -> SlotId = SlotId::Harmony;
                (|| $body)()
            });
        }
        #[cfg(feature = "sampler")]
        {
            answer = answer.or_else(|| {
                type $kind = tutti_sampler::WindowKind;
                let $id: fn(NodeKey) -> SlotId = SlotId::Window;
                (|| $body)()
            });
            answer = answer.or_else(|| {
                type $kind = tutti_sampler::LoopingKind;
                let $id: fn(NodeKey) -> SlotId = SlotId::Looping;
                (|| $body)()
            });
            answer = answer.or_else(|| {
                type $kind = tutti_sampler::DirectionKind;
                let $id: fn(NodeKey) -> SlotId = SlotId::Direction;
                (|| $body)()
            });
            answer = answer.or_else(|| {
                type $kind = tutti_sampler::SpeedKind;
                let $id: fn(NodeKey) -> SlotId = SlotId::Speed;
                (|| $body)()
            });
        }
        answer
    }};
}
pub(super) use published_kinds;

/// The slot a node's setting cell is registered as under `node`.
pub(crate) fn setting_of(node: NodeKey, entry: &ErasedSetting) -> (SlotId, Erased) {
    let slot = WordSlot {
        cell: Arc::clone(entry.cell()),
        of: entry.word(),
    };
    (
        SlotId::Setting(node, entry.kind()),
        erase(Arc::new(slot) as SlotCell<SettingWord>),
    )
}

/// The tempo map, in the transport's settings.
pub(crate) struct TempoCell {
    pub settings: TransportSettings,
}

impl Held for TempoCell {
    type Value = Arc<TempoMap>;

    fn current(&self) -> Arc<TempoMap> {
        self.settings.tempo_map()
    }

    fn replace(&self, value: &Arc<TempoMap>) -> bool {
        self.settings.set_tempo_map(Arc::clone(value));
        true
    }
}

/// The meter, published to the metronome and every plugin following it.
impl Held for ProjectMeter {
    type Value = Arc<MeterMap>;

    fn current(&self) -> Arc<MeterMap> {
        Self::current(self)
    }

    fn replace(&self, value: &Arc<MeterMap>) -> bool {
        self.set_meter(Arc::clone(value));
        true
    }
}

/// The loop region, which the engine owns: a write is one edit scheduled for
/// the next block. The value read is the region sent and not yet applied,
/// else the region the engine last published: the sent one is forgotten once
/// the reading equals it or no scheduled command is left in flight, so an
/// edit timed later shows through the reading.
pub(crate) struct LoopCell {
    transport: Transport,
    sent: Mutex<Option<LoopRegion>>,
}

impl LoopCell {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            transport,
            sent: Mutex::new(None),
        }
    }
}

impl Held for LoopCell {
    type Value = LoopRegion;

    fn current(&self) -> LoopRegion {
        let motion = self.transport.motion();
        let reading = motion.loop_region();
        let mut sent = self.sent.lock();
        match *sent {
            Some(region) if region != reading && motion.scheduled_outstanding() > 0 => region,
            _ => {
                *sent = None;
                reading
            }
        }
    }

    fn replace(&self, value: &LoopRegion) -> bool {
        let landed = self
            .transport
            .motion()
            .schedule(At::NextBlock, LoopEdit::region(*value))
            .is_ok();
        if landed {
            *self.sent.lock() = Some(*value);
        }
        landed
    }
}

/// A published cell of a `Copy` value: written through the cell itself, which
/// publishes when the value differs.
#[cfg(feature = "midi")]
impl<T: Copy + PartialEq + Send + Sync + 'static> Held for tutti_rt::Publisher<T> {
    type Value = T;

    fn current(&self) -> T {
        *tutti_rt::Publisher::current(self)
    }

    fn replace(&self, value: &T) -> bool {
        if *tutti_rt::Publisher::current(self) != *value {
            self.publish(*value);
        }
        true
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
        self.0.set_volume(level);
    }
}

/// A setting's word as a slot's value: the word and the enum it encodes, which
/// a [`SettingTarget`] of another enum does not address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingWord {
    word: u64,
    of: TypeId,
}

impl SettingWord {
    /// `word` of the enum `of`.
    pub(crate) fn new(word: u64, of: TypeId) -> Self {
        Self { word, of }
    }

    fn of<T: AtomicWord + 'static>(value: T) -> Self {
        Self {
            word: value.to_word(),
            of: TypeId::of::<T>(),
        }
    }
}

/// A [`SettingCell`] as the slot of its word: the one slot every setting is.
pub(crate) struct WordSlot {
    pub cell: Arc<dyn SettingCell>,
    /// The enum the cell holds.
    pub of: TypeId,
}

impl WordSlot {
    /// The slot of `cell`, a cell of `T`.
    pub(crate) fn new<T: AtomicWord + 'static>(cell: Arc<dyn SettingCell>) -> Arc<Self> {
        Arc::new(Self {
            cell,
            of: TypeId::of::<T>(),
        })
    }
}

impl Slot<SettingWord> for WordSlot {
    fn load(&self) -> SettingWord {
        SettingWord {
            word: self.cell.load_word(),
            of: self.of,
        }
    }

    fn check(&self, _: &SettingWord) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &SettingWord) {
        // The word is one `SettingTarget::store` encoded from the cell's enum.
        let _ = self.cell.store_word(value.word);
    }
}

/// An [`AtomicCell`] inside a shared owner, found by `project`.
pub(crate) struct SettingSlot<O, T> {
    pub owner: Arc<O>,
    pub project: fn(&O) -> &AtomicCell<T>,
}

impl<O: Send + Sync + 'static, T: AtomicWord + Send + Sync + 'static> SettingCell
    for SettingSlot<O, T>
{
    fn load_word(&self) -> u64 {
        (self.project)(&self.owner).load_word()
    }

    fn store_word(&self, word: u64) -> bool {
        (self.project)(&self.owner).store_word(word)
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
    value: Mutex<V>,
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
            value: Mutex::new(value),
            staged: std::sync::atomic::AtomicBool::new(false),
            check: Box::new(check),
        })
    }

    /// The value now: the one in force, or the one written and not yet taken.
    pub(crate) fn current(&self) -> V {
        self.value.lock().clone()
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
        *self.value.lock() = value.clone();
        self.staged.store(true, Ordering::Release);
    }
}

#[cfg(all(test, feature = "midi"))]
mod tests {
    use super::*;
    use tutti_midi_protocol::MpeMode;
    use tutti_rt::Publisher;

    /// **A value equal to the one in force publishes nothing**: once the
    /// stage publishes, the cell's publish number moves for a different value
    /// and holds for an equal one.
    #[test]
    fn an_equal_value_publishes_nothing_and_a_different_one_publishes() {
        let cell = Publisher::new(MpeMode::Disabled);
        let slot = PublishedSlot::new(cell.clone());
        let lower = tutti_fixtures::midi::lower_zone(3);
        let start = cell.seq();
        slot.store(&MpeMode::Disabled);
        slot.publish_pending();
        assert_eq!(cell.seq(), start);
        slot.store(&lower);
        slot.publish_pending();
        assert_eq!(cell.seq(), start + 1);
        slot.store(&lower);
        slot.publish_pending();
        assert_eq!(cell.seq(), start + 1);

        let controls = tutti_graph::IntoNode::<f32>::into_parts(
            tutti_midi_runtime::MidiClipNode::from(tutti_midi_runtime::MidiClipConfig::default()),
        )
        .controls;
        let slot = PublishedSlot::new(NodeCell(Arc::new(controls.clone())));
        let before = controls.current();
        slot.store(&Arc::new(tutti_midi_runtime::ClipEvents::default()));
        slot.publish_pending();
        assert!(
            Arc::ptr_eq(&controls.current(), &before),
            "an equal clip, by value, publishes nothing"
        );
    }
}

/// What names a route's depth: the route from a source into a target, or the
/// depth of the scale of a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepthKey {
    /// The route from a source into a target.
    Into(ModSourceId, ModTarget),
    /// The scale of a route: a route has at most one.
    Scale(ModRouteId),
}

/// What names a polarity or a curve: a route's, or its scale's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShapeKey {
    /// The route's own.
    Route(ModRouteId),
    /// The scale of the route: a route has at most one.
    Scale(ModRouteId),
}
