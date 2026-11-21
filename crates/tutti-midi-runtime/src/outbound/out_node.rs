//! [`MidiOutNode`]: the graph's end of a MIDI output, one event input passed
//! through to one event output.
//!
//! What the graph sends to an output port arrives at this node's event input,
//! from any number of sources (they sum: events merge in offset order). The
//! node copies them to its event output, which the session's output port taps
//! (`Graph::taps().open_event`) and drains to the device. It keeps no state
//! between blocks, so its fork is a copy of it and sends nothing: a fork has no
//! tap, and an offline render drives no external gear. An event past
//! [`MIDI_OUT_EVENT_CAPACITY`] in a block is not passed and is counted as lost.

use tutti_graph::{Cx, Event, EventKind, Io, Node, Prepare, Shape, Status, UmpAt};

use crate::live::DeviceGeneration;
use crate::live::order::BLANK;

/// The most events a [`MidiOutNode`] passes in one block, its declared event
/// capacity. Past it the events of a block are refused and counted by the
/// executor.
pub const MIDI_OUT_EVENT_CAPACITY: usize = 1024;

const MIDI_OUT_PORT_CAPACITY: std::num::NonZeroU32 =
    std::num::NonZeroU32::new(MIDI_OUT_EVENT_CAPACITY as u32).unwrap();

/// One event input passed through to one event output. See the module docs.
///
/// ```
/// use tutti_graph::{Editor, NodeKey, Prepare};
/// use tutti_midi_runtime::MidiOutNode;
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::MaxBlock;
///
/// let (mut editor, _exec) =
///     Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
/// editor.insert(NodeKey(1), "midi out", tutti_graph::ForkByClone(MidiOutNode::new()));
/// ```
#[derive(Debug)]
pub struct MidiOutNode {
    /// The block's events, between reading the input and writing the output.
    staged: Box<[UmpAt]>,
    /// The retirement the node acknowledges ([`following`](Self::following)).
    follow: Option<Follow>,
}

/// A [`DeviceGeneration`] the node reads each block, and the generations of the
/// two blocks before.
#[derive(Debug)]
struct Follow {
    cell: DeviceGeneration,
    /// The generation read at the start of the last block, and of the one before.
    seen: [u64; 2],
}

impl Follow {
    /// Reads the generation, and acknowledges the one read two blocks ago.
    ///
    /// A generation is handled once a whole block has run after the block that
    /// read it: that block began after the control side stored the generation,
    /// so every source of this node ran in it after that store, and it has
    /// ended, taps included, when the next block starts.
    fn step(&mut self) {
        let now = self.cell.generation();
        self.cell.mark_handled(self.seen[1]);
        self.seen = [now, self.seen[0]];
    }
}

impl Clone for MidiOutNode {
    /// A copy that follows nothing: a fork acknowledges no retirement.
    fn clone(&self) -> Self {
        Self {
            staged: self.staged.clone(),
            follow: None,
        }
    }
}

impl MidiOutNode {
    /// A node with room for [`MIDI_OUT_EVENT_CAPACITY`] events a block.
    #[must_use]
    pub fn new() -> Self {
        Self {
            staged: vec![BLANK; MIDI_OUT_EVENT_CAPACITY].into_boxed_slice(),
            follow: None,
        }
    }

    /// Acknowledges the retirement `generation` says: the node stores the
    /// generation it has handled once a whole block has run, from its sources
    /// to its taps, after the block that read it, so the control side takes the
    /// node out ([`is_idle`](DeviceGeneration::is_idle)) when what was queued
    /// to it before the generation moved has reached its tap. A node that is
    /// never processed never goes idle. Its copies (the forks) follow nothing.
    #[must_use]
    pub fn following(mut self, generation: &DeviceGeneration) -> Self {
        self.follow = Some(Follow {
            cell: generation.clone(),
            seen: [generation.generation(); 2],
        });
        self
    }
}

impl Default for MidiOutNode {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: tutti_buffers::Float> Node<F> for MidiOutNode {
    /// No audio; one event input and one event output of
    /// [`MIDI_OUT_EVENT_CAPACITY`] events.
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(1, 1)
            .with_event_capacity(MIDI_OUT_PORT_CAPACITY)
            // Event-only: a copy per event, nothing per frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        if let Some(follow) = &mut self.follow {
            follow.step();
        }
        let mut kept = 0;
        let mut lost = 0;
        for event in io.events(0) {
            let EventKind::Midi(raw) = event.kind else {
                continue;
            };
            if let Some(slot) = self.staged.get_mut(kept) {
                *slot = (event.offset, raw);
                kept += 1;
            } else {
                lost += 1;
            }
        }
        if lost > 0 {
            // Past the node's room: counted by the executor, as a refused push is.
            io.count_lost(lost);
        }
        for (offset, raw) in &self.staged[..kept] {
            // Refused past the capacity: counted by the executor.
            let _ = io.event_out(0).push(Event::midi(*offset, *raw));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
