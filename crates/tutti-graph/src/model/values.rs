//! The values the graph holds beside its structure: a node's [`NodeMode`] and
//! [`Mix`], a param port's range, and an edge gain's base, range and fade.
//!
//! A value is not wiring: a change of one sends no plan. The editor owns the
//! one table. A unit's cells start from it when a commit places the unit, a
//! fork starts every unit steady in it, and the reference interpreter is
//! handed it beside the graph it follows. What stays in the
//! [`GraphSpec`](crate::GraphSpec) is which lane drives a port and whether an
//! edge has a gain lane.

use std::collections::BTreeMap;

use crate::model::mode::{NodeMix, NodeMode};
use crate::node::param::{ParamIn, ParamRange};
use crate::{EdgeFade, EdgeGain, EdgeId, NodeKey};
use tutti_units::{Gain, Mix};

/// The table. A value at its default has no entry, so two tables with the same
/// values are equal: a node's mode ([`NodeMode::Active`]) and mix
/// ([`Mix::WET`]), a port's range ([`ParamRange::UNBOUNDED`], the whole blend
/// for a mix), an edge's base ([`Gain::UNITY`]) and range
/// ([`EdgeGain::DEFAULT_RANGE`]) and fade ([`EdgeFade::None`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GraphValues {
    modes: BTreeMap<NodeKey, NodeMode>,
    mixes: BTreeMap<NodeKey, Mix>,
    ranges: BTreeMap<ParamIn, ParamRange>,
    bases: BTreeMap<EdgeId, Gain>,
    gain_ranges: BTreeMap<EdgeId, ParamRange>,
    fades: BTreeMap<EdgeId, EdgeFade>,
}

/// Sets `value` at `key`, or removes the entry when `value` is `default`.
fn set<K: Ord, V: PartialEq>(map: &mut BTreeMap<K, V>, key: K, value: V, default: V) {
    if value == default {
        map.remove(&key);
    } else {
        map.insert(key, value);
    }
}

/// The range a port without an entry has.
fn default_range(at: ParamIn) -> ParamRange {
    if at.param == NodeMix::ADDR {
        NodeMix::DEFAULT_RANGE
    } else {
        ParamRange::UNBOUNDED
    }
}

impl GraphValues {
    /// `node`'s mode: [`NodeMode::Active`] unless one was set.
    pub(crate) fn mode(&self, node: NodeKey) -> NodeMode {
        self.modes.get(&node).copied().unwrap_or_default()
    }

    /// Sets `node`'s mode.
    pub(crate) fn set_mode(&mut self, node: NodeKey, mode: NodeMode) {
        set(&mut self.modes, node, mode, NodeMode::Active);
    }

    /// `node`'s mix: [`Mix::WET`] unless one was set.
    pub(crate) fn mix(&self, node: NodeKey) -> Mix {
        self.mixes.get(&node).copied().unwrap_or(Mix::WET)
    }

    /// Sets `node`'s mix.
    pub(crate) fn set_mix(&mut self, node: NodeKey, mix: Mix) {
        if mix == Mix::WET {
            self.mixes.remove(&node);
        } else {
            self.mixes.insert(node, mix);
        }
    }

    /// Port `at`'s range: unbounded unless one was set, and the whole blend
    /// for a node's mix.
    pub(crate) fn range(&self, at: ParamIn) -> ParamRange {
        self.ranges
            .get(&at)
            .copied()
            .unwrap_or_else(|| default_range(at))
    }

    /// Sets port `at`'s range.
    pub(crate) fn set_range(&mut self, at: ParamIn, range: ParamRange) {
        set(&mut self.ranges, at, range, default_range(at));
    }

    /// Edge `id`'s base: unity unless one was set.
    pub(crate) fn base(&self, id: EdgeId) -> Gain {
        self.bases.get(&id).copied().unwrap_or(Gain::UNITY)
    }

    /// Sets edge `id`'s base.
    pub(crate) fn set_base(&mut self, id: EdgeId, base: Gain) {
        set(&mut self.bases, id, base, Gain::UNITY);
    }

    /// Edge `id`'s range: [`EdgeGain::DEFAULT_RANGE`] unless one was set.
    pub(crate) fn gain_range(&self, id: EdgeId) -> ParamRange {
        self.gain_ranges
            .get(&id)
            .copied()
            .unwrap_or(EdgeGain::DEFAULT_RANGE)
    }

    /// Sets edge `id`'s range.
    pub(crate) fn set_gain_range(&mut self, id: EdgeId, range: ParamRange) {
        set(&mut self.gain_ranges, id, range, EdgeGain::DEFAULT_RANGE);
    }

    /// Edge `id`'s fade: [`EdgeFade::None`] unless one was set.
    pub(crate) fn fade(&self, id: EdgeId) -> EdgeFade {
        self.fades.get(&id).copied().unwrap_or_default()
    }

    /// Sets edge `id`'s fade.
    pub(crate) fn set_fade(&mut self, id: EdgeId, fade: EdgeFade) {
        set(&mut self.fades, id, fade, EdgeFade::None);
    }

    /// Drops edge `id`'s base, range and fade: a lane declared again starts at
    /// unity with the default range and no fade.
    pub(crate) fn forget_edge(&mut self, id: EdgeId) {
        self.bases.remove(&id);
        self.gain_ranges.remove(&id);
        self.fades.remove(&id);
    }

    /// Keeps the base, range and fade of the edges `keep` accepts.
    pub(crate) fn retain_edges(&mut self, mut keep: impl FnMut(EdgeId) -> bool) {
        self.bases.retain(|&id, _| keep(id));
        self.gain_ranges.retain(|&id, _| keep(id));
        self.fades.retain(|&id, _| keep(id));
    }

    /// Copies into `to` the range of every port of the nodes `keep` accepts.
    pub(crate) fn copy_ranges(&self, to: &mut Self, mut keep: impl FnMut(NodeKey) -> bool) {
        for (&at, &range) in &self.ranges {
            if keep(at.node) {
                to.set_range(at, range);
            }
        }
    }

    /// Keeps the range of the ports `keep` accepts.
    pub(crate) fn retain_ranges(&mut self, mut keep: impl FnMut(ParamIn) -> bool) {
        self.ranges.retain(|&at, _| keep(at));
    }

    /// Drops everything held for `node`: its mode, mix and port ranges. The
    /// node starts again at the defaults.
    pub(crate) fn forget(&mut self, node: NodeKey) {
        self.modes.remove(&node);
        self.mixes.remove(&node);
        self.ranges.retain(|at, _| at.node != node);
    }

    /// Keeps the values of the nodes `keep` accepts.
    #[cfg(feature = "testing")]
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeKey) -> bool) {
        self.modes.retain(|&k, _| keep(k));
        self.mixes.retain(|&k, _| keep(k));
        self.ranges.retain(|at, _| keep(at.node));
    }

    /// Whether every value is the default.
    #[cfg(feature = "testing")]
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
