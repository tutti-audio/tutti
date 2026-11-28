//! Targets on a hosted plugin: [`PluginParamTarget`] for a parameter's position
//! [`PluginAutomationModeTarget`] for the host's automation mode and
//! [`PluginTrackInfoTarget`] for what it says of the plugin's track and
//! [`PluginMidiMapTarget`] for the app's controller map.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tutti_graph::NodeKey;
use tutti_plugin_host::{ParamReporter, PluginHandle, PluginParamValues};
use tutti_plugin_types::{
    AutomationMode, Delivered, MainThreadMarker, MidiAssignment, ParamAccess, ParamMapping,
    ParamUnit, ParamWriteError, ParameterInfo, PlainMap, PlainUnit, TrackInfo, effective_midi_map,
};
use tutti_rt::Mutex;
use tutti_units::{
    Amplitude, ArcDegrees, BeatDuration, Bpm, Cents, Db, Hz, Mix, Normalized, Pan, ParamId,
    PlaybackRate, Seconds, Semitones,
};

use super::error::{PluginParamRefusal, SetRefusal, UnitError};
use super::slot::{Slot, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{Modulatable, Target, TargetId, sealed, target_builders};
use crate::plugin_params::Driven;

/// One parameter of the plugin hosted at a node: what `key.param(id)` returns,
/// written and read as a position on `0..=1` ([`Normalized`]), or, from
/// [`PluginParam::unit`](crate::PluginParam::unit), as a tutti unit `U` through the
/// parameter's declared map.
///
/// A plain reference that checks nothing when it is made. A plugin's parameter
/// is a target once the plugin's declaration is read: before that, and for an id
/// the plugin does not declare, `set` and `get` fail with
/// [`ValueErrorKind::NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget). A
/// read-only parameter refuses a write with [`SetRefusal::Plugin`]. A write lands in the session's mirror under the value
/// lock, so a read answers it at once, and is then sent to the plugin. A write
/// the plugin's transport cannot take (an out-of-process plugin's full command queue, an
/// in-process plugin busy in an editor call) is counted in
/// [`Plugin::losses`](crate::Plugin::losses) and does not refuse the call; so is
/// a write to a parameter the plugin dropped after the session read its
/// declaration (a rescan), which the re-read then makes
/// [`NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget).
///
/// Between a plugin's rescan and its re-read landing in `update`, a unit write
/// converts with the map read before the rescan.
///
/// The map of a unit target is the declaration as the session holds it at each
/// call, never a copy made when the target was: a rescan that removes the unit
/// or the map refuses the call with [`UnitError`] (as
/// [`SetRefusal::Plugin`]).
///
/// A parameter a modulation lane drives takes the value as the lane's knob; only
/// the position target is a modulation target.
/// `Copy`, and equal when node and id are.
pub struct PluginParamTarget<U = Normalized> {
    node: NodeKey,
    id: ParamId,
    value: PhantomData<fn() -> U>,
}

impl<U> Clone for PluginParamTarget<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for PluginParamTarget<U> {}
impl<U> std::fmt::Debug for PluginParamTarget<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginParamTarget")
            .field("node", &self.node)
            .field("id", &self.id)
            .finish()
    }
}
impl<U> PartialEq for PluginParamTarget<U> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.id == other.id
    }
}
impl<U> Eq for PluginParamTarget<U> {}
impl<U> std::hash::Hash for PluginParamTarget<U> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.node.hash(state);
        self.id.hash(state);
    }
}

impl<U> PluginParamTarget<U> {
    /// Parameter `id` of the plugin at `node`, read in `U`.
    pub(crate) const fn new(node: NodeKey, id: ParamId) -> Self {
        Self {
            node,
            id,
            value: PhantomData,
        }
    }

    /// The plugin's node.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.node
    }

    /// The parameter's id.
    #[must_use]
    pub const fn id(self) -> ParamId {
        self.id
    }
}

impl From<PluginParamTarget> for crate::ParamRef {
    fn from(param: PluginParamTarget) -> Self {
        Self::plugin(param.node, param.id)
    }
}

impl From<PluginParamTarget> for crate::ModTarget {
    fn from(param: PluginParamTarget) -> Self {
        Self::Param(param.into())
    }
}

impl<U> sealed::Sealed for PluginParamTarget<U> {
    fn id(self) -> TargetId {
        TargetId::PluginParam(self.node, self.id)
    }
}
impl Target for PluginParamTarget {
    type Value = Normalized;
}
impl Modulatable for PluginParamTarget {}

/// What a plugin parameter is written through: the plugin's table entry and the
/// parameter's id.
#[derive(Debug)]
pub struct PluginCell {
    entry: Arc<PluginEntry>,
    id: ParamId,
}

impl sealed::Store for PluginParamTarget {
    type Cell = PluginCell;

    crate::value::no_host_gesture!();

    fn resolve(self, params: &Params) -> Option<(PluginCell, Normalized)> {
        let entry = params.plugin(self.node)?;
        let base = entry.load(self.id)?;
        Some((PluginCell { entry, id: self.id }, base))
    }

    fn check(cell: &PluginCell, _: &Normalized) -> Result<(), SetRefusal> {
        cell.entry.writable(cell.id).map_err(SetRefusal::Plugin)
    }

    fn store(cell: &PluginCell, value: &Normalized) {
        cell.entry.write(cell.id, *value);
    }
}

target_builders!(unwatched [] PluginParamTarget);

/// A parameter's map between its position and its plain value in a declared
/// unit, as the declaration read at the call gives it.
///
/// The plain value is a bare `f64` here because it is the plugin's own number
/// in the unit the plugin declares; the typed value `U` is built from it by
/// [`PlainUnit`].
#[derive(Clone, Copy, Debug)]
struct Conversion {
    map: PlainMap,
    unit: ParamUnit,
    /// The ends of the map's plain range, low first.
    low: f64,
    high: f64,
}

impl Conversion {
    /// The conversion of parameter `id` into `U`, from its declared map and
    /// unit.
    fn of<U: PluginUnit>(
        id: ParamId,
        map: Option<PlainMap>,
        measures: Option<ParamUnit>,
    ) -> Result<Self, UnitError> {
        let (Some(map), Some(declared)) = (map, measures) else {
            return Err(UnitError::NoUnit(id));
        };
        let wanted = std::any::type_name::<U>();
        if !U::reads(declared) {
            return Err(UnitError::UnitMismatch {
                id,
                declared,
                wanted,
            });
        }
        let ends = [
            map.plain(Normalized::of(0.0)),
            map.plain(Normalized::of(1.0)),
        ];
        // Every type here is an interval: if both ends read, so does everything
        // between them.
        if ends
            .iter()
            .any(|&end| !end.is_finite() || U::from_plain(declared, end).is_none())
        {
            return Err(UnitError::RangeUnfit {
                id,
                declared,
                wanted,
            });
        }
        Ok(Self {
            map,
            unit: declared,
            low: ends[0].min(ends[1]),
            high: ends[0].max(ends[1]),
        })
    }

    /// The position `position` in `U`.
    fn read<U: PluginUnit>(&self, position: Normalized) -> Option<U> {
        U::from_plain(
            self.unit,
            self.map.plain(position).clamp(self.low, self.high),
        )
    }

    /// The position of `value`, or why the range does not take it. The
    /// position is the map's own for the plain value it names, so a stepped map
    /// lands on a step. A value outside the declared range is `OutOfRange` even
    /// when it would round into it (12.4 on -12..12).
    fn position<U: PluginUnit>(&self, value: U) -> Result<Normalized, SetRefusal> {
        let plain = value.to_plain(self.unit);
        if !plain.is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        if !(self.low..=self.high).contains(&plain) {
            return Err(SetRefusal::OutOfRange);
        }
        Ok(self
            .map
            .normalized(self.map.plain(self.map.normalized(plain))))
    }
}

impl<U: PluginUnit> PluginParamTarget<U> {
    /// Parameter `info` of the plugin at `node` as a target in `U`, when its
    /// declaration reads as `U`.
    pub(crate) fn declared(node: NodeKey, info: &ParameterInfo) -> Result<Self, UnitError> {
        Conversion::of::<U>(info.id(), info.plain_map(), info.measures())?;
        Ok(Self::new(node, info.id()))
    }

    /// The parameter in `U`: its entry and conversion, and the base now.
    fn resolve_unit(self, params: &Params) -> Option<(UnitCell, U)> {
        let entry = params.plugin(self.node)?;
        let conversion = entry.conversion::<U>(self.id)?.ok()?;
        let base = conversion.read::<U>(entry.load(self.id)?)?;
        Some((
            UnitCell {
                cell: PluginCell { entry, id: self.id },
                conversion,
            },
            base,
        ))
    }

    /// Why the parameter, declared, is not a target in `U`.
    fn unresolved_unit(self, params: &Params) -> Option<SetRefusal> {
        let entry = params.plugin(self.node)?;
        let refused = entry.conversion::<U>(self.id)?.err()?;
        Some(SetRefusal::Plugin(refused.into()))
    }
}

mod unit_sealed {
    /// Names the units a plugin parameter is a target in.
    pub trait Sealed {}
}

/// A tutti unit a plugin parameter is a target in
/// ([`PluginParam::unit`](crate::PluginParam::unit)): the [`PlainUnit`]s with a
/// target, sealed so the units that make a target are the units that store.
pub trait PluginUnit: PlainUnit + unit_sealed::Sealed {}

/// What a plugin parameter in a unit is written through: its cell and the
/// conversion read when the write was staged.
#[derive(Debug)]
pub struct UnitCell {
    cell: PluginCell,
    conversion: Conversion,
}

/// The targets of a parameter in each of the units a plugin declares
/// ([`PlainUnit`]): one impl per type, since a blanket impl over
/// [`PluginUnit`] overlaps `Normalized`'s under coherence.
macro_rules! unit_targets {
    ($($ty:ty),+ $(,)?) => {$(
        impl unit_sealed::Sealed for $ty {}
        impl PluginUnit for $ty {}

        impl Target for PluginParamTarget<$ty> {
            type Value = $ty;
        }

        impl sealed::Store for PluginParamTarget<$ty> {
            type Cell = UnitCell;

            crate::value::no_host_gesture!();

            fn resolve(self, params: &Params) -> Option<(UnitCell, $ty)> {
                self.resolve_unit(params)
            }

            fn unresolved(self, params: &Params) -> Option<SetRefusal> {
                self.unresolved_unit(params)
            }

            fn check(cell: &UnitCell, value: &$ty) -> Result<(), SetRefusal> {
                cell.conversion.position(*value)?;
                cell.cell.entry.writable(cell.cell.id).map_err(SetRefusal::Plugin)
            }

            fn store(cell: &UnitCell, value: &$ty) {
                // `check` passed on this value and conversion.
                if let Ok(position) = cell.conversion.position(*value) {
                    cell.cell.entry.write(cell.cell.id, position);
                }
            }
        }

        target_builders!(unwatched [] PluginParamTarget<$ty>);
    )+};
}

unit_targets!(
    Amplitude,
    ArcDegrees,
    BeatDuration,
    Bpm,
    Cents,
    Db,
    Hz,
    Mix,
    Pan,
    PlaybackRate,
    Seconds,
    Semitones,
);

/// A hosted plugin as the value table holds it: what a write to one of its
/// parameters needs, and a lock-free copy of the value the plugin's mirror
/// holds of each. The mirror is the source of every value.
#[derive(Debug)]
pub(crate) struct PluginEntry {
    handle: PluginHandle,
    driven: Driven,
    reporter: ParamReporter,
    /// The parameters the plugin declares, `None` until its read lands.
    declared: Option<BTreeMap<ParamId, Declared>>,
    /// What the plugin's handle did not take of the writes that passed
    /// `writable`: shared with the plugin's record, which counts the lost ones
    /// in its losses, sends the owed ones again and tells of the refused ones.
    ledger: Arc<WriteLedger>,
}

/// A refusal by name of a write that passed the session's own check: the plugin
/// no longer declares the parameter, or declares it read-only now. Told once, as
/// the [`PluginChange`](crate::PluginChange) of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteRefusal {
    /// The plugin does not declare the parameter (a rescan dropped it).
    Undeclared(ParamId),
    /// The plugin declares the parameter read-only.
    ReadOnly(ParamId),
}

/// The host writes to one plugin that its handle did not take, by cause. A
/// level is state, so what the carrier lost is owed again and what the target
/// refused is given up with a notice:
///
/// - `Unanswered` (a served plugin gave no parameter list to check against) is
///   **owed**: the newest value of each parameter is sent again at the next
///   `Phase::Plugins`, until the plugin takes it. A newer write replaces it.
/// - `Undeclared` and `ReadOnly` are **refused**: re-sending would repeat the
///   refusal, so each is told once ([`WriteRefusal`]).
/// - `Lost` (the plugin is gone) is counted, as it always was.
///
/// What is owed or told belongs to the plugin instance it was written for:
/// [`end_epoch`](Self::end_epoch) forgets it when the plugin is replaced. The
/// count of lost writes is cumulative, as the plugin's other losses are.
#[derive(Debug, Default)]
pub(crate) struct WriteLedger {
    lost: AtomicU64,
    state: Mutex<Owed>,
}

#[derive(Debug, Default)]
struct Owed {
    /// The newest value of each parameter the plugin has not taken.
    values: BTreeMap<ParamId, Normalized>,
    /// Refusals not told yet.
    refused: Vec<WriteRefusal>,
}

impl WriteLedger {
    /// The writes lost because the plugin was gone.
    pub(crate) fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// Sends `value` to `id` through `handle`, under the ledger's lock so a send
    /// again cannot overtake a newer write, and records what the handle answers.
    fn send(&self, handle: &PluginHandle, id: ParamId, value: Normalized) {
        let mut owed = self.state.lock();
        owed.settle(handle, &self.lost, id, value);
    }

    /// Sends each of `pending` (the writes a saved state did not hold) as
    /// [`send`](Self::send) sends a write: a replay is a write like any other, so
    /// what the plugin does not answer for is owed and what it refuses is told.
    pub(crate) fn send_each(&self, handle: &PluginHandle, pending: &[(ParamId, Normalized)]) {
        for &(id, value) in pending {
            self.send(handle, id, value);
        }
    }

    /// Sends again each value the plugin did not take for want of an answer
    /// (a parameter list), and returns the refusals not told yet. The main
    /// thread's `Phase::Plugins`.
    pub(crate) fn resend(&self, handle: &PluginHandle) -> Vec<WriteRefusal> {
        let mut owed = self.state.lock();
        for (id, value) in std::mem::take(&mut owed.values) {
            owed.settle(handle, &self.lost, id, value);
        }
        std::mem::take(&mut owed.refused)
    }

    /// The plugin instance these writes were for is gone: nothing owed or
    /// untold is carried to its successor.
    pub(crate) fn end_epoch(&self) {
        *self.state.lock() = Owed::default();
    }
}

impl Owed {
    /// One send and what it answers: taken, owed, refused by name, or lost.
    fn settle(&mut self, handle: &PluginHandle, lost: &AtomicU64, id: ParamId, value: Normalized) {
        match handle.send_parameter(id, value) {
            Ok(()) => {
                self.values.remove(&id);
            }
            Err(ParamWriteError::Unanswered(_)) => {
                self.values.insert(id, value);
            }
            Err(ParamWriteError::Undeclared(_)) => {
                self.values.remove(&id);
                self.refused.push(WriteRefusal::Undeclared(id));
            }
            Err(ParamWriteError::ReadOnly(_)) => {
                self.values.remove(&id);
                self.refused.push(WriteRefusal::ReadOnly(id));
            }
            Err(ParamWriteError::Lost(_)) => {
                self.values.remove(&id);
                lost.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// One declared parameter. The plugin's mirror is the source of its value;
/// this is the value table's lock-free copy of it.
#[derive(Debug)]
struct Declared {
    access: ParamAccess,
    /// How the plain value follows from the position, when the declaration
    /// says; read at each call that converts, never held by a target.
    plain_map: Option<PlainMap>,
    /// The unit the plain value is measured in, when the plugin declares one.
    measures: Option<ParamUnit>,
    /// The bits of the position the mirror holds.
    value: AtomicU64,
}

impl Declared {
    fn of(info: &ParameterInfo, value: Normalized) -> Self {
        Self {
            access: ParamAccess::of(info),
            plain_map: info.plain_map(),
            measures: info.measures(),
            value: AtomicU64::new(value.get().to_bits()),
        }
    }
}

/// What a plugin's mirror declared of one parameter, kept across a replace by
/// the same plugin.
#[derive(Debug, Clone)]
pub(crate) struct CarriedParam {
    id: ParamId,
    access: ParamAccess,
    plain_map: Option<PlainMap>,
    measures: Option<ParamUnit>,
    value: Normalized,
}

impl CarriedParam {
    pub(crate) fn new(info: &ParameterInfo, value: Normalized) -> Self {
        Self {
            id: info.id(),
            access: ParamAccess::of(info),
            plain_map: info.plain_map(),
            measures: info.measures(),
            value,
        }
    }
}

impl PluginEntry {
    /// The entry for the plugin of `handle` whose values `mirror` holds. While
    /// the mirror is not read, the entry declares `carried`, what the plugin
    /// at the node declared before it was replaced by itself.
    pub(crate) fn new(
        handle: PluginHandle,
        driven: Driven,
        mirror: &PluginParamValues,
        carried: Option<&[CarriedParam]>,
        ledger: Arc<WriteLedger>,
    ) -> Self {
        let declared = if mirror.is_ready() {
            Some(
                mirror
                    .descriptors()
                    .iter()
                    .enumerate()
                    .map(|(at, info)| (info.id(), Declared::of(info, mirror.value_at(at))))
                    .collect(),
            )
        } else {
            carried.map(|held| {
                held.iter()
                    .map(|p| {
                        let declared = Declared {
                            access: p.access,
                            plain_map: p.plain_map,
                            measures: p.measures,
                            value: AtomicU64::new(p.value.get().to_bits()),
                        };
                        (p.id, declared)
                    })
                    .collect()
            })
        };
        Self {
            handle,
            driven,
            reporter: mirror.reporter(),
            declared,
            ledger,
        }
    }

    /// The parameter `id`, when the plugin's read has landed and declares it.
    fn param(&self, id: ParamId) -> Option<&Declared> {
        self.declared.as_ref()?.get(&id)
    }

    /// The position the mirror holds of `id`, or `None` for a parameter that
    /// is not declared (or not read yet).
    fn load(&self, id: ParamId) -> Option<Normalized> {
        self.param(id).map(|declared| {
            Normalized::clamped(f64::from_bits(declared.value.load(Ordering::Relaxed)))
        })
    }

    /// The conversion of `id` into `U` from the declaration held now: `None`
    /// for a parameter that is not declared.
    fn conversion<U: PluginUnit>(&self, id: ParamId) -> Option<Result<Conversion, UnitError>> {
        let declared = self.param(id)?;
        Some(Conversion::of::<U>(
            id,
            declared.plain_map,
            declared.measures,
        ))
    }

    /// Whether the plugin lists `id`: the parameters the session read.
    fn declares(&self, id: ParamId) -> Result<(), PluginParamRefusal> {
        match &self.declared {
            None => Err(PluginParamRefusal::NotRead),
            Some(listed) if listed.contains_key(&id) => Ok(()),
            Some(_) => Err(PluginParamRefusal::UnknownParam(id)),
        }
    }

    /// Whether the host may write `id`.
    fn writable(&self, id: ParamId) -> Result<(), PluginParamRefusal> {
        match self.param(id).map(|declared| declared.access) {
            Some(ParamAccess::ReadOnly) => Err(PluginParamRefusal::ReadOnly(id)),
            _ => Ok(()),
        }
    }

    /// Writes the mirror, then the plugin: the lane's knob for a parameter a
    /// lane drives, the plugin's handle otherwise. Infallible once
    /// [`writable`](Self::writable) passed; called under the value lock.
    fn write(&self, id: ParamId, value: Normalized) {
        self.note(id, value);
        // A value table's `store` is infallible and a batch lands whole, so a
        // write the send refuses is the ledger's: owed again when the plugin did
        // not answer, told by name when `id` was dropped by a rescan (the mirror
        // resyncs at the next read of its list, which makes `id` no such target)
        // or is read-only, counted when the plugin is lost. A recovery replaces
        // the mirror with the replacement's own read and ends the ledger's epoch,
        // so the value `note` stored is not sent again.
        if !self.driven.write(id, value) {
            self.ledger.send(&self.handle, id, value);
        }
        self.reporter.report(id, value);
    }

    /// Records the position the mirror holds of `id`.
    pub(crate) fn note(&self, id: ParamId, value: Normalized) {
        if let Some(declared) = self.param(id) {
            declared
                .value
                .store(value.get().to_bits(), Ordering::Relaxed);
        }
    }
}

/// A hosted plugin's automation mode: the [`Slot`] of its
/// [`PluginAutomationModeTarget`].
pub(crate) struct AutomationModeSlot {
    handle: PluginHandle,
    mode: Mutex<AutomationMode>,
}

impl AutomationModeSlot {
    /// The slot for the plugin of `handle`, in [`AutomationMode::Off`].
    pub(crate) fn cell(handle: PluginHandle) -> SlotCell<AutomationMode> {
        Arc::new(Self {
            handle,
            mode: Mutex::new(AutomationMode::Off),
        })
    }
}

impl Slot<AutomationMode> for AutomationModeSlot {
    fn load(&self) -> AutomationMode {
        *self.mode.lock()
    }

    fn check(&self, _: &AutomationMode) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &AutomationMode) {
        *self.mode.lock() = *value;
        // An advisory for the plugin's editor: the mode the host holds is the
        // value, and a push the plugin's transport did not take is not retried.
        let _ = self.handle.set_automation_mode(*value);
    }
}

keyed_slot_target!(
    /// What the host tells a plugin it is doing with automation (reading it,
    /// writing it, both or neither), so the plugin's editor can show it, as
    /// [`Plugin::automation_mode`](crate::Plugin::automation_mode) names it.
    /// The plugin does nothing audible with it.
    ///
    /// The target holds the mode the host last set, which starts as
    /// [`AutomationMode::Off`]; a write is passed on to the plugin at once.
    PluginAutomationModeTarget,
    AutomationMode,
    NodeKey,
    PluginAutomationMode
);

impl PluginAutomationModeTarget {
    /// The plugin's node.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.0
    }
}

impl SlotId {
    /// The slot of the automation mode of the plugin at `node`.
    pub(crate) const fn automation_mode(node: NodeKey) -> Self {
        Self::PluginAutomationMode(node)
    }
}

/// What the host says of the track a hosted plugin sits on: the [`Slot`] of its
/// [`PluginTrackInfoTarget`].
///
/// A write is held and owed to the plugin: the main thread's next
/// [`Phase::Plugins`](crate::Phase::Plugins) delivers it, because only that
/// thread may.
pub(crate) struct TrackInfoSlot {
    info: Mutex<TrackInfo>,
    owed: AtomicBool,
}

impl TrackInfoSlot {
    /// A slot holding the default [`TrackInfo`], which nothing is owed for.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            info: Mutex::new(TrackInfo::default()),
            owed: AtomicBool::new(false),
        })
    }

    /// Owes the held value to a plugin that has not been told it: a value
    /// still at its default is what a new plugin starts with.
    pub(crate) fn resend(&self) {
        let owed = *self.info.lock() != TrackInfo::default();
        self.owed.store(owed, Ordering::Release);
    }

    /// Tells the plugin of `handle` the held value if it is owed. A queue that
    /// was full leaves it owed for the next call; any other answer settles it.
    pub(crate) fn deliver(&self, handle: &PluginHandle, main: MainThreadMarker) {
        if self.owed.swap(false, Ordering::AcqRel) {
            let info = self.info.lock().clone();
            if handle.set_track_info(main, &info) == Delivered::Dropped {
                self.owed.store(true, Ordering::Release);
            }
        }
    }
}

impl Slot<TrackInfo> for TrackInfoSlot {
    fn load(&self) -> TrackInfo {
        self.info.lock().clone()
    }

    fn check(&self, _: &TrackInfo) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &TrackInfo) {
        *self.info.lock() = value.clone();
        self.owed.store(true, Ordering::Release);
    }
}

keyed_slot_target!(
    /// What the host tells a plugin about the track it sits on (its name,
    /// colour, width and kind), as
    /// [`Plugin::track_info`](crate::Plugin::track_info) names it. A format with
    /// no place for it takes it and drops it.
    ///
    /// The target holds the [`TrackInfo`] the host last set, which starts as the
    /// default. A write is delivered to the plugin on the main thread by the
    /// next [`Session::update`](crate::Session::update) made there, once per
    /// write; a write the plugin's transport could not take is delivered again
    /// at the update after. A new plugin at the node is told the held value.
    /// The value is advisory, like the automation mode's: a write never fails
    /// for what the plugin does with it, and the plugin does nothing audible.
    PluginTrackInfoTarget,
    TrackInfo,
    NodeKey,
    PluginTrackInfo
);

impl PluginTrackInfoTarget {
    /// The plugin's node.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.0
    }
}

impl SlotId {
    /// The slot of the track info of the plugin at `node`.
    pub(crate) const fn track_info(node: NodeKey) -> Self {
        Self::PluginTrackInfo(node)
    }
}

/// The controller map the app gives a plugin: which MIDI message drives which
/// parameter, beside the table the plugin declares
/// ([`Plugin::assignments`](crate::Plugin::assignments)).
///
/// An entry is keyed by its channel (`None` for any) and its source; a map
/// names each key once ([`PluginMidiMapTarget`] refuses a duplicate). The map
/// is the app's alone: the plugin's own table is never copied into it, and
/// [`effective`](Self::effective) reads the two together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "Vec<MidiAssignment>", into = "Vec<MidiAssignment>")
)]
#[non_exhaustive]
pub struct MidiMap {
    assignments: Vec<MidiAssignment>,
}

impl MidiMap {
    /// The map of `assignments`, in order.
    #[must_use]
    pub fn new(assignments: impl IntoIterator<Item = MidiAssignment>) -> Self {
        Self {
            assignments: assignments.into_iter().collect(),
        }
    }

    /// The app's entries, in the order given.
    #[must_use]
    pub fn assignments(&self) -> &[MidiAssignment] {
        &self.assignments
    }

    /// What drives what once the app's entries are laid over `declared`, the
    /// plugin's own table ([`effective_midi_map`]).
    pub fn effective<'a>(
        &'a self,
        declared: &'a [MidiAssignment],
    ) -> impl Iterator<Item = MidiAssignment> + 'a {
        effective_midi_map(declared, &self.assignments)
    }

    /// Refuses a map that names one channel and source twice.
    fn distinct(&self) -> Result<(), PluginParamRefusal> {
        let twice = self.assignments.iter().enumerate().find(|&(at, a)| {
            self.assignments[..at]
                .iter()
                .any(|b| b.channel == a.channel && b.source == a.source)
        });
        match twice {
            Some((_, a)) => Err(PluginParamRefusal::DuplicateMidiEntry {
                channel: a.channel,
                message: a.source,
            }),
            None => Ok(()),
        }
    }
}

/// The wire form of a map is its entries. A list that names a channel and
/// source twice is not a map.
impl TryFrom<Vec<MidiAssignment>> for MidiMap {
    type Error = PluginParamRefusal;

    fn try_from(assignments: Vec<MidiAssignment>) -> Result<Self, PluginParamRefusal> {
        let map = Self { assignments };
        map.distinct()?;
        Ok(map)
    }
}

impl From<MidiMap> for Vec<MidiAssignment> {
    fn from(map: MidiMap) -> Self {
        map.assignments
    }
}

/// The app's MIDI map of a hosted plugin: the [`Slot`] of its
/// [`PluginMidiMapTarget`].
///
/// A write is held and owed to the plugin: the main thread's next
/// [`Phase::Plugins`](crate::Phase::Plugins) delivers it, because only that
/// thread may.
pub(crate) struct MidiMapSlot {
    map: Mutex<MidiMap>,
    owed: AtomicBool,
}

impl MidiMapSlot {
    /// A slot holding the empty map, which nothing is owed for.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(MidiMap::default()),
            owed: AtomicBool::new(false),
        })
    }

    /// Owes the held map to a plugin that has not been given it: an empty map
    /// is what a new plugin starts with.
    pub(crate) fn resend(&self) {
        let owed = !self.map.lock().assignments.is_empty();
        self.owed.store(owed, Ordering::Release);
    }

    /// Hands the plugin of `handle` the held map if it is owed, once.
    pub(crate) fn deliver(&self, handle: &PluginHandle, main: MainThreadMarker) {
        if self.owed.swap(false, Ordering::AcqRel) {
            let map = self.map.lock().clone();
            // Who applies the entries is the plugin host's node, which acts on
            // the answer where the plugin's control gave it.
            let _ = handle.set_app_midi_map(main, map.assignments());
        }
    }
}

impl Slot<MidiMap> for MidiMapSlot {
    fn load(&self) -> MidiMap {
        self.map.lock().clone()
    }

    fn check(&self, value: &MidiMap) -> Result<(), SetRefusal> {
        Ok(value.distinct()?)
    }

    fn store(&self, value: &MidiMap) {
        *self.map.lock() = value.clone();
        self.owed.store(true, Ordering::Release);
    }
}

/// The app's controller map of a hosted plugin ([`MidiMap`]), as
/// [`Plugin::midi_map`](crate::Plugin::midi_map) names it.
///
/// `set(target.to(map))` replaces the app's map and `get` reads it back. A
/// write is refused at the call, with nothing delivered, when an entry names a
/// parameter the plugin does not list
/// ([`PluginParamRefusal::UnknownParam`]; [`NotRead`](PluginParamRefusal::NotRead)
/// before the listing is read) or two entries share a channel and source
/// ([`PluginParamRefusal::DuplicateMidiEntry`]). The map is delivered to the
/// plugin on the main thread by the next [`Session::update`](crate::Session::update)
/// made there, once per write; a new plugin at the node is given the held map.
/// The map is saved with the project's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PluginMidiMapTarget(NodeKey);

impl PluginMidiMapTarget {
    pub(crate) const fn new(node: NodeKey) -> Self {
        Self(node)
    }

    /// The plugin's node.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.0
    }
}

impl sealed::Sealed for PluginMidiMapTarget {
    fn id(self) -> TargetId {
        TargetId::Slot(SlotId::PluginMidiMap(self.0))
    }
}
impl Target for PluginMidiMapTarget {
    type Value = MidiMap;
}

/// What a MIDI map is written through: the plugin's table entry, which lists
/// its parameters, and the map's slot.
pub struct MidiMapCell {
    entry: Arc<PluginEntry>,
    slot: SlotCell<MidiMap>,
}

impl std::fmt::Debug for MidiMapCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiMapCell")
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

impl sealed::Store for PluginMidiMapTarget {
    type Cell = MidiMapCell;

    crate::value::no_host_gesture!();

    fn resolve(self, params: &Params) -> Option<(MidiMapCell, MidiMap)> {
        let entry = params.plugin(self.0)?;
        let slot = params.slot::<MidiMap>(SlotId::PluginMidiMap(self.0))?;
        let base = slot.load();
        Some((MidiMapCell { entry, slot }, base))
    }

    fn check(cell: &MidiMapCell, value: &MidiMap) -> Result<(), SetRefusal> {
        for assignment in value.assignments() {
            cell.entry.declares(assignment.param)?;
        }
        cell.slot.check(value)
    }

    fn store(cell: &MidiMapCell, value: &MidiMap) {
        cell.slot.store(value);
    }
}

target_builders!([] PluginMidiMapTarget);

impl SlotId {
    /// The slot of the app's MIDI map of the plugin at `node`.
    pub(crate) const fn midi_map(node: NodeKey) -> Self {
        Self::PluginMidiMap(node)
    }
}
