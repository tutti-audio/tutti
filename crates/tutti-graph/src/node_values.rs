//! The values the graph holds for a node beside its structure: its
//! [`NodeMode`] and its [`Mix`].
//!
//! A mode and a mix are values, not wiring: a change of either sends no plan.
//! The editor owns the one table. A unit's cell starts from it when a commit
//! places the unit, a fork starts every unit steady in it, and the reference
//! interpreter is handed it beside the graph it follows.

use std::collections::BTreeMap;

use crate::NodeKey;
use crate::mode::NodeMode;
use tutti_units::Mix;

/// Each node's mode and mix. A node with the default of either ([`NodeMode::Active`],
/// [`Mix::WET`]) has no entry for it, so two tables with the same values are equal.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodeValues {
    modes: BTreeMap<NodeKey, NodeMode>,
    mixes: BTreeMap<NodeKey, Mix>,
}

impl NodeValues {
    /// `node`'s mode: [`NodeMode::Active`] unless one was set.
    pub(crate) fn mode(&self, node: NodeKey) -> NodeMode {
        self.modes.get(&node).copied().unwrap_or_default()
    }

    /// Sets `node`'s mode.
    pub(crate) fn set_mode(&mut self, node: NodeKey, mode: NodeMode) {
        if mode == NodeMode::Active {
            self.modes.remove(&node);
        } else {
            self.modes.insert(node, mode);
        }
    }

    /// `node`'s mix: [`Mix::WET`] unless one was set.
    pub(crate) fn mix(&self, node: NodeKey) -> Mix {
        self.mixes.get(&node).copied().unwrap_or(Mix::WET)
    }

    /// Sets `node`'s mix.
    pub(crate) fn set_mix(&mut self, node: NodeKey, mix: Mix) {
        if mix.get().to_bits() == Mix::WET.get().to_bits() {
            self.mixes.remove(&node);
        } else {
            self.mixes.insert(node, mix);
        }
    }

    /// Drops `node`'s mode and mix: the node starts again at the defaults.
    pub(crate) fn forget(&mut self, node: NodeKey) {
        self.modes.remove(&node);
        self.mixes.remove(&node);
    }

    /// Keeps the values of the nodes `keep` accepts.
    #[cfg(feature = "testing")]
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeKey) -> bool) {
        self.modes.retain(|&k, _| keep(k));
        self.mixes.retain(|&k, _| keep(k));
    }

    /// Whether every node has the defaults.
    #[cfg(feature = "testing")]
    pub(crate) fn is_empty(&self) -> bool {
        self.modes.is_empty() && self.mixes.is_empty()
    }
}
