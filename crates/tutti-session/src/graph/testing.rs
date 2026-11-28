//! What a test of the session reads that no host should: the graph's own
//! bookkeeping. Behind the off-by-default `testing` feature, which this
//! crate's own tests turn on through a dev-dependency on itself.

use tutti_buffers::Float;
use tutti_graph::NodeKey;

use super::Graph;

impl<F: Float> Graph<F> {
    /// What a test reads of the graph's plumbing: [`Testing`].
    pub fn testing(&self) -> Testing<'_, F> {
        Testing { graph: self }
    }
}

/// The graph's plumbing as a test reads it: what the session drives and no
/// host calls. Made by [`Graph::testing`].
#[derive(Debug, Clone, Copy)]
pub struct Testing<'g, F: Float = f32> {
    graph: &'g Graph<F>,
}

impl<F: Float> Testing<'_, F> {
    /// Whether the graph keeps a reclaimer for `node`
    /// ([`NodeParts::reclaimer`](tutti_graph::NodeParts::reclaimer)): for a test that a path inserts a node
    /// through [`Graph::add`].
    pub fn keeps_reclaimer(&self, node: impl Into<NodeKey>) -> bool {
        let node = node.into();
        self.graph.nodes.get(&node).is_some_and(|entry| {
            entry
                .controls
                .as_ref()
                .is_some_and(|held| held.reclaimer.is_some())
        })
    }

    /// Whether the node of the loaded plugin at `node` was re-inserted for new
    /// ports and its Processor has not reached it yet (its record is
    /// `Settling`): `None` for a node with no such record.
    #[cfg(feature = "plugin")]
    pub fn rebuild_settling(&self, node: impl Into<NodeKey>) -> Option<bool> {
        let node = node.into();
        self.graph
            .plugins
            .get(&node)?
            .rebind
            .as_ref()
            .map(crate::plugin::RebindState::is_settling)
    }

    /// How many replacements wait for a re-prepare to resume.
    pub fn waiting_replacements(&self) -> usize {
        self.graph.parked.len()
    }
}
