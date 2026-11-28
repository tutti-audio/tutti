//! [`MidiPart`] with the `midi` feature: the engine's MIDI nodes, the clips, live inputs,
//! routes and outputs the session declares, and the hardware layer under them.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use std::sync::Arc;
use tutti_platform::Instant;

use tutti_buffers::Float;
use tutti_graph::NodeKey;
use tutti_midi_runtime::MidiClipNode;
use tutti_timeline::MotionCommand;
use tutti_units::{Losses, SampleRate, Samples};

use super::TransportPart;
use crate::Key;
use crate::builder::{Options, Parts};
use crate::graph::Graph;
use crate::midi::MidiState;
use crate::midi_follow::{ClockAction, FollowCx, Placed};
use crate::value::{SlotCell, SlotId, ValueTable};
use crate::{Playhead, SessionEvent, TransportHandle};

#[cfg(feature = "midi-hardware")]
mod hardware;
#[cfg(not(feature = "midi-hardware"))]
#[path = "midi/hardware_none.rs"]
mod hardware;

use hardware::Hardware;

/// The engine's MIDI, as graph nodes the builder inserts: the sync node (Beat
/// Clock, MTC and Machine Control, sending nothing until a host sets its
/// messages), with its controls. An input port's input node is not here: each
/// input port makes its own (`MidiInput::node`), and each output its own
/// (`MidiSink::node`, `MidiOutput::node`), fed by the sync node.
#[derive(Clone, Debug)]
pub struct MidiNodes {
    /// Which of an input port's event outputs carries which Group and channel
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub input_ports: tutti_midi_runtime::MidiInputPorts,
    /// The sync node, on Group 1.
    pub sync: NodeKey,
    /// What the sync node sends (`messages()`) and could not send (`losses()`).
    pub sync_controls: Arc<tutti_midi_runtime::SyncOutControls>,
}

/// The session's MIDI: what it declared, and the OS layer under it.
#[derive(Debug)]
pub(crate) struct MidiPart {
    /// The engine's MIDI nodes, when the builder inserted them.
    pub(crate) nodes: Option<MidiNodes>,
    /// Clips, live inputs, routes and the outputs.
    pub(crate) state: MidiState,
    /// The OS MIDI layer and who the session is on the wire.
    pub(crate) hardware: Hardware,
}

/// What the followed clock and Machine Control asked of the transport in one
/// poll, and whether following edited the graph.
#[derive(Debug, Default)]
pub(crate) struct Followed {
    actions: Vec<Placed>,
    edited: bool,
}

impl Followed {
    /// Whether the graph was edited, which the caller commits.
    pub(crate) fn edited(&self) -> bool {
        self.edited
    }
}

impl MidiPart {
    pub(crate) fn new<F: Float>(parts: &mut Parts<F>, options: &Options) -> Self {
        let (hardware, state) = Hardware::new(parts);
        Self {
            nodes: parts.midi.take(),
            state: state.with_mpe(options.mpe_mode),
            hardware,
        }
    }

    /// Registers the MIDI cells in `table`.
    pub(crate) fn provide_values(&self, table: &ValueTable) {
        self.hardware.provide_values(table);
        table.provide(
            SlotId::OutputLatency,
            self.state.output_latency_cell() as SlotCell<tutti_midi_runtime::OutputLatency>,
        );
        table.provide_staged(
            SlotId::MpeMode,
            crate::value::PublishedSlot::new(self.state.mpe().clone()),
        );
    }

    /// The cells the audio thread published to, scanned and what no block holds
    /// freed: how many retired values are still held.
    pub(crate) fn reclaim(&self) -> usize {
        self.hardware.reclaim() + self.state.mpe().reclaim()
    }

    /// `node` left the graph: what the session held of it goes. A clip's
    /// transform, or a transform's clip, is the other node of the pair, which
    /// the caller takes out of the graph too.
    #[must_use = "the clip's other node is still in the graph"]
    pub(crate) fn forget_node(&mut self, node: NodeKey) -> Option<NodeKey> {
        self.state.forget_node(node)
    }

    /// Each of `gained` that is a clip player node sends its new receiver the
    /// clip's setup.
    pub(crate) fn receivers_gained<F: Float>(&self, graph: &Graph<F>, gained: &[NodeKey]) {
        for &node in gained {
            if let Some(controls) = graph.controls(&Key::<MidiClipNode>::new(node)) {
                controls.receiver_changed();
            }
        }
    }

    /// `Phase::MidiOut`: drains every output's tap. Whether the graph was edited.
    pub(crate) fn out_phase<F: Float>(&mut self, graph: &mut Graph<F>, now: Instant) -> bool {
        self.state.midi_out_phase(graph, now)
    }

    /// The first of `Phase::Poll`'s MIDI: who the session is on the wire is in
    /// force, then the devices are stepped. Whether the graph was edited.
    pub(crate) fn poll_devices<F: Float>(
        &mut self,
        graph: &mut Graph<F>,
        now: Instant,
        out: &mut Vec<SessionEvent>,
    ) -> bool {
        let networks = self
            .state
            .midi_network_phase(self.nodes.as_ref(), graph, out);
        let devices = self
            .hardware
            .poll(&mut self.state, self.nodes.as_ref(), graph, now, out);
        // The input nodes may have changed in either: the routes are compiled once.
        if networks || devices {
            self.state.compile_feeds_for(self.nodes.as_ref(), graph);
        }
        networks || devices
    }

    /// The tempo, the MIDI clock and Machine Control the session follows, read
    /// after the devices: what a device sent in the block before is in its tap.
    pub(crate) fn poll_follow<F: Float>(
        &mut self,
        transport: &TransportPart,
        playhead: Playhead,
        graph: &mut Graph<F>,
        out: &mut Vec<SessionEvent>,
    ) -> Followed {
        let follow = FollowCx {
            settings: transport.settings(),
            meter: transport.settings().project_meter(),
            bases: &transport.bases,
            playhead,
        };
        let tempo_edited = self.state.follow_phase(&follow, graph, out);
        let (clock_actions, clock_edited) = self.state.clock_phase(&follow, graph, out);
        let (machine_actions, machine_edited) = self.state.machine_phase(&follow, graph, out);
        Followed {
            actions: clock_actions.into_iter().chain(machine_actions).collect(),
            edited: tempo_edited || clock_edited || machine_edited,
        }
    }

    /// Sends what a followed MIDI clock asked of the transport, each command at
    /// the frame it was placed on (`At::NextBlock` for Machine Control). A command
    /// the transport's list has no room for is dropped and logged, and counted
    /// ([`MidiEvent::ClockCommandsRefused`](crate::MidiEvent::ClockCommandsRefused)):
    /// the source says its position again with the next message.
    pub(crate) fn send_follow(
        followed: Followed,
        handle: &TransportHandle,
        out: &mut Vec<SessionEvent>,
    ) {
        let refused: Losses = followed
            .actions
            .into_iter()
            .map(|placed| Losses::refused(u64::from(!Self::send_action(placed, handle))))
            .sum();
        if refused != Losses::NONE {
            out.push(crate::MidiEvent::ClockCommandsRefused(refused).into());
        }
    }

    fn send_action(placed: Placed, handle: &TransportHandle) -> bool {
        let event = match placed.action {
            ClockAction::Play => MotionCommand::Play,
            ClockAction::Stop => MotionCommand::stop(),
            ClockAction::Locate(beat) => MotionCommand::locate(beat),
            ClockAction::PlayFrom(beat) => MotionCommand::play_from(beat),
        };
        match handle.schedule(placed.at, event) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!("midi: a followed clock's {event:?} was not sent: {error}");
                false
            }
        }
    }

    /// The hardware inlets place an arrival a block after it came, a block of
    /// the device's callback.
    pub(crate) fn set_block(&self, block: Samples, rate: SampleRate) {
        self.hardware.set_block(block, rate);
    }

    /// The outputs' taps are on one scale of frames, the rate they were opened
    /// at: reopens those of a graph re-prepared at another rate. Whether the
    /// graph was edited.
    pub(crate) fn follow_rate<F: Float>(&mut self, graph: &mut Graph<F>, now: Instant) -> bool {
        self.state.follow_rate(graph, now)
    }
}
