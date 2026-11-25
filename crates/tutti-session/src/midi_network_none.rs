//! The session's networks on a build without them: feature `midi-network` off, or a
//! target with no UDP sockets (the web). None is ever opened, so each step over
//! them does nothing. `midi_network` is this module's twin where they are built.

use tutti_buffers::Float;
use tutti_graph::NodeKey;
use tutti_midi_runtime::MidiInputPorts;

use crate::SessionEvent;
use crate::graph::Graph;

/// What a network reports where there is none: nothing, so no value of it exists.
/// [`MidiEvent::Network`](crate::MidiEvent::Network) names it on a web build with
/// the feature on.
#[cfg_attr(
    not(feature = "midi-network"),
    expect(
        dead_code,
        reason = "named by `MidiEvent::Network`, a feature `midi-network` variant"
    )
)]
#[derive(Debug)]
pub enum NetworkEvent {}

/// A peer that joined a network's output: none does.
pub(crate) enum Joined {}

impl Joined {
    /// The output node the peer listens to.
    pub(crate) fn sink(&self) -> NodeKey {
        match *self {}
    }

    /// Sends `packets` to the peer alone.
    pub(crate) fn send(&self, _: &[tutti_ump::Ump]) -> bool {
        match *self {}
    }
}

/// No network is open.
#[derive(Debug, Default)]
pub(crate) struct Networks;

impl Networks {
    /// No network has an input node.
    pub(crate) fn input_nodes(&self) -> Vec<NodeKey> {
        Vec::new()
    }

    /// No node is a network peer's input node.
    pub(crate) fn is_input_node(&self, _node: NodeKey) -> bool {
        false
    }

    /// No peer joins.
    pub(crate) fn take_joined(&mut self) -> Vec<Joined> {
        Vec::new()
    }

    /// No network to poll: the graph is not edited.
    pub(crate) fn poll<F: Float>(
        &mut self,
        _: &mut Graph<F>,
        _: &tutti_rt::Publisher<tutti_midi_protocol::MpeMode>,
        _: MidiInputPorts,
        _: &mut Vec<SessionEvent>,
    ) -> bool {
        false
    }
}
