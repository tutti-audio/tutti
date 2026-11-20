//! The session's MIDI ports on a build without the OS MIDI layer (feature
//! `midi-hardware` off): none are ever opened, so each step over them does
//! nothing. `midi_port` is this module's twin when the layer is built.

use tutti_graph::NodeKey;

use crate::midi::OutputTiming;

/// The ports of a session with no OS MIDI layer: none.
#[derive(Default)]
pub(crate) struct Ports;

impl Ports {
    /// No input port has an input node.
    pub(crate) fn input_nodes(&self) -> Vec<NodeKey> {
        Vec::new()
    }

    /// No node is an input port's input node.
    pub(crate) fn is_input_node(&self, _node: NodeKey) -> bool {
        false
    }

    /// No output tap to reopen: the graph is not edited.
    pub(crate) fn reopen<F: tutti_buffers::Float>(
        &mut self,
        _: &mut crate::graph::Graph<F>,
        _: Option<&OutputTiming>,
    ) -> bool {
        false
    }

    /// No output tap to drain: the graph is not edited.
    pub(crate) fn drain_outputs<F: tutti_buffers::Float>(
        &mut self,
        _: &mut crate::graph::Graph<F>,
        _: Option<&OutputTiming>,
    ) -> bool {
        false
    }
}
