//! [`Hardware`] with the `midi-hardware` feature: the OS MIDI layer and who the session is
//! on the wire.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use std::sync::Arc;
use tutti_platform::Instant;

use tutti_buffers::Float;
use tutti_midi_device::MidiIdentity;
use tutti_midi_hardware::Presented;

use tutti_midi_runtime::MidiInputPorts;
use tutti_rt::Reclaim;
use tutti_units::{SampleRate, Samples};

use super::MidiNodes;
use crate::builder::Parts;
use crate::graph::Graph;
use crate::midi::MidiState;
use crate::value::{Cell, SlotCell, SlotId, ValueTable};
use crate::{MidiPolicy, SessionEvent};

/// The OS MIDI layer and the identity the session presents on it.
#[derive(Debug)]
pub(crate) struct Hardware {
    pub(crate) io: Option<tutti_midi_hardware::MidiSession>,
    /// Who the session is on the wire, as set ([`Session::policy`](crate::Session::policy)).
    policy: Arc<Cell<MidiPolicy>>,
}

impl Hardware {
    /// The layer the builder opened, and the state that receives its device events.
    pub(crate) fn new<F: Float>(parts: &mut Parts<F>) -> (Self, MidiState) {
        let io = parts.midi_io.take();
        let policy = crate::midi_hardware::policy_cell(io.is_some());
        let state = MidiState::with_device_events(parts.device_events.take());
        (Self { io, policy }, state)
    }

    /// Registers the policy cell in `table`.
    pub(crate) fn provide_values(&self, table: &ValueTable) {
        table.provide(
            SlotId::MidiPolicy,
            Arc::clone(&self.policy) as SlotCell<MidiPolicy>,
        );
    }

    /// The hardware inlets' clock and port set, scanned.
    pub(crate) fn reclaim(&self) -> usize {
        self.io
            .as_ref()
            .map_or(0, |io| io.ports().inlets().reclaim())
    }

    /// Makes the OS MIDI layer present what the policy says, then steps the
    /// devices: a device that connected is heard from the plan that holds its
    /// inlet. Whether the graph was edited.
    pub(crate) fn poll<F: Float>(
        &mut self,
        state: &mut MidiState,
        nodes: Option<&MidiNodes>,
        graph: &mut Graph<F>,
        now: Instant,
        out: &mut Vec<SessionEvent>,
    ) -> bool {
        self.apply_policy(nodes);
        state.midi_input_phase(self.io.as_mut(), nodes, graph, now, out)
    }

    fn apply_policy(&mut self, nodes: Option<&MidiNodes>) {
        let groups = nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |nodes| nodes.input_ports)
            .groups();
        let wanted = self
            .policy
            .current()
            .identity
            .map(|identity: MidiIdentity| Presented { identity, groups });
        if let Some(io) = &mut self.io
            && io.presented() != wanted
        {
            io.present(wanted);
        }
    }

    /// The inlets place an arrival a block after it came, a block of the
    /// device's callback.
    pub(crate) fn set_block(&self, block: Samples, rate: SampleRate) {
        if let Some(io) = &self.io {
            io.ports()
                .inlets()
                .set_lead(crate::midi::input_lead(block, rate));
        }
    }
}
