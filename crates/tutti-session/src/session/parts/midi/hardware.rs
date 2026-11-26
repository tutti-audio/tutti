//! [`Hardware`] with the `midi-hardware` feature: the OS MIDI layer and who the session is
//! on the wire.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use std::sync::Arc;
use tutti_platform::Instant;

use tutti_buffers::Float;
use tutti_midi_ci::ResourceStore;
use tutti_midi_device::MidiIdentity;
use tutti_midi_hardware::Presented;

use tutti_midi_runtime::MidiInputPorts;
use tutti_platform::Task;
use tutti_rt::Reclaim;
use tutti_units::{SampleRate, Samples};

use super::MidiNodes;
use crate::builder::Parts;
use crate::graph::Graph;
use crate::midi::MidiState;
use crate::midi_link::MidiLink;
use crate::value::{Cell, SlotCell, SlotId, ValueTable};
use crate::{MidiPolicy, SessionEvent};

/// The OS MIDI layer and the identity the session presents on it.
#[derive(Debug)]
pub(crate) struct Hardware {
    /// Ends the MIDI-CI task as it drops, before the link it drives.
    _task: Option<Task<()>>,
    /// The OS MIDI layer, shared with the MIDI-CI task.
    pub(crate) io: Option<Arc<MidiLink>>,
    /// Who the session is on the wire, as set ([`Session::policy`](crate::Session::policy)).
    policy: Arc<Cell<MidiPolicy>>,
    /// The store the layer serves now, as the policy held it.
    served: Option<Arc<ResourceStore>>,
}

impl Hardware {
    /// The layer the builder opened and started, the MIDI-CI task on its own
    /// thread that drives it, and the state that receives its device events.
    pub(crate) fn new<F: Float>(parts: &mut Parts<F>) -> (Self, MidiState) {
        let (task, io) = match parts.midi_io.take() {
            Some((link, task)) => (Some(task), Some(link)),
            None => (None, None),
        };
        let policy = crate::midi_hardware::policy_cell(io.is_some());
        let state = MidiState::with_device_events(parts.device_events.take());
        (
            Self {
                _task: task,
                io,
                policy,
                served: None,
            },
            state,
        )
    }

    /// The layer, locked for a call.
    pub(crate) fn locked(
        &self,
    ) -> Option<std::sync::MutexGuard<'_, tutti_midi_hardware::MidiSession>> {
        self.io.as_deref().map(MidiLink::lock)
    }

    /// The link to the layer and its task.
    pub(crate) fn link(&self) -> Option<&MidiLink> {
        self.io.as_deref()
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
        self.locked().map_or(0, |io| io.ports().inlets().reclaim())
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
        state.midi_input_phase(self.link(), graph, now, out)
    }

    /// Makes the layer present the policy's identity and serve its store. The
    /// policy is cloned once per poll, an `Arc` clone on the control side, and
    /// the store is handed to the layer only when it is not the one served:
    /// `None` serves the empty store.
    fn apply_policy(&mut self, nodes: Option<&MidiNodes>) {
        let policy = self.policy.current();
        let groups = nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |nodes| nodes.input_ports)
            .groups();
        let wanted = policy
            .identity
            .map(|identity: MidiIdentity| Presented { identity, groups });
        let Some(mut io) = self.locked() else {
            return;
        };
        if io.presented() != wanted {
            io.present(wanted);
        }
        if !crate::midi_hardware::same_store(&policy.properties, &self.served) {
            io.serve_properties(policy.properties.as_deref().cloned().unwrap_or_default());
            drop(io);
            self.served = policy.properties;
        }
    }

    /// The inlets place an arrival a block after it came, a block of the
    /// device's callback.
    pub(crate) fn set_block(&self, block: Samples, rate: SampleRate) {
        if let Some(io) = self.locked() {
            io.ports()
                .inlets()
                .set_lead(crate::midi::input_lead(block, rate));
        }
    }
}
