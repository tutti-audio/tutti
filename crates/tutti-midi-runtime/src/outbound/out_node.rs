//! [`MidiOutNode`]: the graph's end of a MIDI output, one event input passed
//! through to one event output.
//!
//! What the graph sends to an output port arrives at this node's event input,
//! from any number of sources (they sum: events merge in offset order). The
//! node copies them to its event output, which the session's output port taps
//! (`Graph::taps().open_event`) and drains to the device. It keeps no state
//! between blocks, so its fork is a copy of it and sends nothing: a fork has no
//! tap, and an offline render drives no external gear.

use tutti_graph::{Cx, Event, EventKind, Io, Node, Prepare, Shape, Status, UmpAt};

use crate::live::order::BLANK;

/// The most events a [`MidiOutNode`] passes in one block, its declared event
/// capacity. Past it the events of a block are refused and counted by the
/// executor.
pub const MIDI_OUT_EVENT_CAPACITY: usize = 1024;

/// One event input passed through to one event output. See the module docs.
///
/// ```
/// use tutti_graph::{Editor, NodeKey, Prepare};
/// use tutti_midi_runtime::MidiOutNode;
/// use tutti_types::{MaxBlock, SampleRate, Samples};
///
/// let (mut editor, _exec) =
///     Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(512))));
/// editor.insert(NodeKey(1), "midi out", tutti_graph::ForkByClone(MidiOutNode::new()));
/// ```
#[derive(Clone, Debug)]
pub struct MidiOutNode {
    /// The block's events, between reading the input and writing the output.
    staged: Box<[UmpAt]>,
}

impl MidiOutNode {
    /// A node with room for [`MIDI_OUT_EVENT_CAPACITY`] events a block.
    #[must_use]
    pub fn new() -> Self {
        Self {
            staged: vec![BLANK; MIDI_OUT_EVENT_CAPACITY].into_boxed_slice(),
        }
    }
}

impl Default for MidiOutNode {
    fn default() -> Self {
        Self::new()
    }
}

impl Node for MidiOutNode {
    /// No audio; one event input and one event output of
    /// [`MIDI_OUT_EVENT_CAPACITY`] events.
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(1, 1)
            .with_event_capacity(MIDI_OUT_EVENT_CAPACITY as u32)
            // Event-only: a copy per event, nothing per frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut kept = 0;
        for event in io.events(0) {
            let EventKind::Midi(raw) = event.kind else {
                continue;
            };
            if let Some(slot) = self.staged.get_mut(kept) {
                *slot = (event.offset, raw);
                kept += 1;
            }
        }
        for (offset, raw) in &self.staged[..kept] {
            // Refused past the capacity: counted by the executor.
            let _ = io.event_out(0).push(Event::midi(*offset, *raw));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
