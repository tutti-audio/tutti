//! [`MachineControlNode`]: outbound MIDI Machine Control as a graph node, out
//! of one event port.
//!
//! It looks with its [`MachineControlSender`] once per transport segment of each
//! block (`Env::segments`), so a start, a stop or a locate inside a block sends
//! its message on its own frame. A message goes out as SysEx7 packets in one
//! run, so a block never splits one. Tap its port (`Graph::taps().open_event`)
//! for a hardware pump to drain.
//!
//! The node owns the sender. A transport time no time code names is refused
//! and counted ([`MachineControlControls::losses`], `refused`), never sent as
//! another. A message goes out whole or not at all: when the port has no room
//! for all its packets the message is counted as lost (`lost`, one per message),
//! so a block never carries half a SysEx run.
//!
//! Its fork sends nothing: an offline render drives no external gear.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Losses, Node, NodeParts,
    Offset, Prepare, Shape, Status,
};
use tutti_midi_protocol::{MtcRate, Recipient};
use tutti_types::Beat;
use tutti_ump::{MidiGroup, Sysex7Packets};

use super::machine_control_out::MachineControlSender;

/// The most events a [`MachineControlNode`] sends in one block, its declared
/// event capacity: a block holds at most nine segments, each at most one
/// message of two packets (a LOCATE and a DEFERRED PLAY are 12 bytes with
/// their header).
pub const MACHINE_CONTROL_EVENT_CAPACITY: usize = 18;

/// What a [`MachineControlNode`] reports to its owner.
#[derive(Clone, Debug, Default)]
pub struct MachineControlControls {
    refused: Arc<AtomicU64>,
    lost: Arc<AtomicU64>,
}

impl MachineControlControls {
    /// What the node could not send, since it was made. `refused` counts the
    /// looks at a transport that moved to a time no time code names
    /// ([`NoTimeCode`](crate::NoTimeCode)); `lost` counts the messages the
    /// port had no room for (or took only part of).
    #[must_use]
    pub fn losses(&self) -> Losses {
        Losses::lost(self.lost.load(Ordering::Relaxed))
            + Losses::refused(self.refused.load(Ordering::Relaxed))
    }
}

/// Outbound MIDI Machine Control as a graph node with one event output.
/// See the module docs.
///
/// ```
/// use tutti_graph::{Editor, NodeKey, Prepare};
/// use tutti_midi_protocol::{MtcRate, Recipient};
/// use tutti_midi_runtime::MachineControlNode;
/// use tutti_types::{MaxBlock, SampleRate, Samples};
/// use tutti_ump::MidiGroup;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(512))));
/// let node = MachineControlNode::new(MidiGroup::FIRST, Recipient::AllCall, MtcRate::Fps30);
/// let controls = editor.insert(NodeKey(1), "mmc", node);
/// assert_eq!(controls.losses().total(), 0);
/// ```
#[derive(Debug)]
pub struct MachineControlNode {
    sender: MachineControlSender,
    group: MidiGroup,
    controls: MachineControlControls,
    /// A fork's node sends nothing.
    silent: bool,
}

impl MachineControlNode {
    /// A node that sends Machine Control to `to` on `group`, stating time codes
    /// at `rate`.
    #[must_use]
    pub fn new(group: MidiGroup, to: Recipient, rate: MtcRate) -> Self {
        Self {
            sender: MachineControlSender::new(to, rate),
            group,
            controls: MachineControlControls::default(),
            silent: false,
        }
    }
}

impl Node for MachineControlNode {
    /// No audio; one event output of [`MACHINE_CONTROL_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(MACHINE_CONTROL_EVENT_CAPACITY as u32)
            // Event-only: a message at a transport change, nothing per frame.
            .with_cost(tutti_graph::Cost::Light)
            .with_no_note_events()
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block_len = cx.env.block_len;
        let out = io.event_out(0);
        for (start, segment) in cx.env.segments() {
            let base = start.get();
            let transport = &segment.transport;
            // The transport's time is its beat at its tempo from beat 0, the
            // timeline the clock's MTC runs on.
            let seconds = (transport.beat() - Beat(0.0)).to_seconds(transport.tempo);
            let payload = match self.sender.observe(
                transport.playing,
                seconds,
                segment.frame,
                cx.env.sample_rate,
            ) {
                Ok(Some(payload)) if !self.silent => payload,
                Ok(_) => continue,
                Err(_) => {
                    self.controls.refused.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let at = Offset::clamped(base, block_len);
            // A payload built by the sender is seven-bit and short.
            if let Ok(packets) = Sysex7Packets::from_payload(self.group, &payload) {
                let room = MACHINE_CONTROL_EVENT_CAPACITY.saturating_sub(out.len());
                // Both losses below are defensive: the declared capacity is nine
                // segments of two packets, which a block never exceeds.
                if packets.len() > room {
                    self.controls.lost.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let mut pushed = true;
                for packet in packets {
                    pushed &= out.push(Event::midi(at, packet)).is_ok();
                }
                if !pushed {
                    self.controls.lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A [`MachineControlNode`]'s fork: a node of its own that sends nothing.
struct SilentSender {
    group: MidiGroup,
    to: Recipient,
    rate: MtcRate,
}

impl ForkSource for SilentSender {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        let mut node = MachineControlNode::new(self.group, self.to, self.rate);
        node.silent = true;
        Ok(Forked::new(Box::new(node)))
    }
}

impl IntoNode for MachineControlNode {
    /// How many messages it could not send.
    type Controls = MachineControlControls;

    fn into_parts(self) -> NodeParts<MachineControlControls> {
        let controls = self.controls.clone();
        let fork = SilentSender {
            group: self.group,
            to: self.sender.to(),
            rate: self.sender.rate(),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}
