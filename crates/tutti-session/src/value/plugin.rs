//! Targets on a hosted plugin: [`PluginParamTarget`] for a parameter's position
//! and [`PluginAutomationModeTarget`] for the host's automation mode.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tutti_graph::NodeKey;
use tutti_plugin_host::{ParamReporter, PluginHandle, PluginParamValues};
use tutti_plugin_types::{AutomationMode, ParamAccess, UnknownParam};
use tutti_rt::LockExt;
use tutti_units::{Normalized, ParamId};

use super::error::{PluginParamRefusal, SetRefusal};
use super::slot::{Slot, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{Modulatable, Target, TargetId, sealed, target_builders};
use crate::plugin_params::Driven;

/// One parameter of the plugin hosted at a node, written and read as a position
/// on `0..=1`: what `key.param(id)` returns.
///
/// A plain reference that checks nothing when it is made. `session.set` refuses
/// it where it is written, with [`SetRefusal::Plugin`]: before the plugin's
/// parameters are read, for an id the plugin does not declare, and for a
/// read-only parameter. A write lands in the session's mirror under the value
/// lock, so a read answers it at once, and is then sent to the plugin. A write
/// the plugin's transport cannot take (a served plugin's full command queue, an
/// in-process plugin busy in an editor call) is counted in
/// [`Plugin::losses`](crate::Plugin::losses) and does not refuse the call.
///
/// A parameter a modulation lane drives takes the value as the lane's knob.
/// `Copy`, and equal when node and id are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PluginParamTarget {
    node: NodeKey,
    id: ParamId,
}

impl PluginParamTarget {
    /// Parameter `id` of the plugin at `node`.
    pub(crate) const fn new(node: NodeKey, id: ParamId) -> Self {
        Self { node, id }
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

impl sealed::Sealed for PluginParamTarget {
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
        let base = entry.load(self.id);
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

/// A hosted plugin as the value table holds it: what a write to one of its
/// parameters needs, and the value the host mirror holds of each.
#[derive(Debug)]
pub(crate) struct PluginEntry {
    handle: PluginHandle,
    driven: Driven,
    reporter: ParamReporter,
    /// The parameters the plugin declares, `None` until its read lands.
    declared: Option<BTreeMap<ParamId, Declared>>,
}

#[derive(Debug)]
struct Declared {
    access: ParamAccess,
    /// The bits of the position the mirror holds.
    value: AtomicU64,
}

impl PluginEntry {
    /// The entry for the plugin of `handle` whose values `mirror` holds.
    pub(crate) fn new(handle: PluginHandle, driven: Driven, mirror: &PluginParamValues) -> Self {
        let declared = mirror.is_ready().then(|| {
            mirror
                .descriptors()
                .iter()
                .enumerate()
                .map(|(at, info)| {
                    let declared = Declared {
                        access: ParamAccess::of(info),
                        value: AtomicU64::new(mirror.value_at(at).get().to_bits()),
                    };
                    (info.id(), declared)
                })
                .collect()
        });
        Self {
            handle,
            driven,
            reporter: mirror.reporter(),
            declared,
        }
    }

    fn param(&self, id: ParamId) -> Result<&Declared, PluginParamRefusal> {
        self.declared
            .as_ref()
            .ok_or(PluginParamRefusal::NotRead)?
            .get(&id)
            .ok_or(PluginParamRefusal::UnknownParam(UnknownParam(id)))
    }

    /// The position the mirror holds, or the start of the range for a
    /// parameter that is not declared.
    fn load(&self, id: ParamId) -> Normalized {
        self.param(id).map_or(Normalized::of(0.0), |declared| {
            Normalized::clamped(f64::from_bits(declared.value.load(Ordering::Relaxed)))
        })
    }

    /// Whether the host may write `id`.
    fn writable(&self, id: ParamId) -> Result<(), PluginParamRefusal> {
        match self.param(id)?.access {
            ParamAccess::ReadOnly => Err(PluginParamRefusal::ReadOnly(id)),
            ParamAccess::Writable | ParamAccess::Unreported => Ok(()),
        }
    }

    /// Writes the mirror, then the plugin: the lane's knob for a parameter a
    /// lane drives, the plugin's handle otherwise. Infallible once
    /// [`writable`](Self::writable) passed; called under the value lock.
    fn write(&self, id: ParamId, value: Normalized) {
        self.note(id, value);
        if !self.driven.write(id, value) {
            self.handle.send_parameter(id, value);
        }
        self.reporter.report(id, value);
    }

    /// Records the position the mirror holds of `id`.
    pub(crate) fn note(&self, id: ParamId, value: Normalized) {
        if let Ok(declared) = self.param(id) {
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
