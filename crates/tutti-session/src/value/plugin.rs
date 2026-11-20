//! Targets on a hosted plugin: [`PluginParamTarget`] for a parameter's position
//! and [`PluginAutomationModeTarget`] for the host's automation mode.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tutti_graph::NodeKey;
use tutti_plugin_host::{ParamReporter, PluginHandle, PluginParamValues};
use tutti_plugin_types::{
    AutomationMode, ParamAccess, ParamMapping, ParamUnit, ParameterInfo, PlainMap, PlainUnit,
};
use tutti_rt::LockExt;
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
/// [`PluginParam::try_unit`](crate::PluginParam::try_unit), as a tutti unit `U` through the
/// parameter's declared map.
///
/// A plain reference that checks nothing when it is made. A plugin's parameter
/// is a target once the plugin's declaration is read: before that, and for an id
/// the plugin does not declare, `set` and `get` fail with
/// [`ValueErrorKind::NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget). A
/// read-only parameter refuses a write with [`SetRefusal::Plugin`]. A write lands in the session's mirror under the value
/// lock, so a read answers it at once, and is then sent to the plugin. A write
/// the plugin's transport cannot take (a served plugin's full command queue, an
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

target_builders!(moved [] PluginParamTarget);

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
/// ([`PluginParam::try_unit`](crate::PluginParam::try_unit)): the [`PlainUnit`]s with a
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

        target_builders!([] PluginParamTarget<$ty>);
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
    /// The writes the plugin refused after `writable` passed, which the
    /// plugin's record counts in its losses.
    refused: Arc<AtomicU64>,
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
        refused: Arc<AtomicU64>,
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
            refused,
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
        // `writable` passed, so the plugin declared `id` when its parameters
        // were read; one it dropped since (a rescan) is refused by the send. A
        // value table's `store` is infallible and a batch lands whole, so the
        // write is counted in the plugin's losses, and the mirror resyncs at the
        // next read of its list, which makes `id` no such target.
        if !self.driven.write(id, value) && self.handle.send_parameter(id, value).is_err() {
            self.refused.fetch_add(1, Ordering::Relaxed);
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
        *self.mode.lock_recover()
    }

    fn check(&self, _: &AutomationMode) -> Result<(), SetRefusal> {
        Ok(())
    }

    fn store(&self, value: &AutomationMode) {
        *self.mode.lock_recover() = *value;
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
