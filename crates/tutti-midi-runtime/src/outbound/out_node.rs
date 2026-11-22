//! [`MidiOutNode`]: the graph's end of a MIDI output, one event input passed
//! through to one event output.
//!
//! What the graph sends to an output port arrives at this node's event input,
//! from any number of sources (they sum: events merge in offset order). The
//! node copies them to its event output, which the session's output port taps
//! (`Graph::taps().open_event`) and drains to the device. It keeps no state
//! between blocks, so its fork is a copy of it and sends nothing: a fork has no
//! tap, and an offline render drives no external gear. A fork is a side copy in
//! an executor of its own, never a replacement of the live node, so it
//! follows no retirement ([`following`](MidiOutNode::following)). An event past
//! [`MIDI_OUT_EVENT_CAPACITY`] in a block is not passed and is counted as lost.

use tutti_graph::{
    Cx, Event, EventKind, InEvents, Io, Node, Offset, OutEvents, Prepare, Shape, Status, UmpAt,
};
use tutti_ump::NoteKey;

use crate::ledger::{Class, owed_off};
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
    /// The keys passed on with a Note On and no Note Off since, while the node
    /// follows a retirement: what it ends when the retirement comes.
    held: Option<Box<[u64; HELD_WORDS]>>,
}

/// One bit per key: 16 Groups, 16 Channels and 128 notes, as a [`NoteKey`]
/// packs a MIDI 2.0 key.
const HELD_WORDS: usize = 1 << 9;

/// The bit of a MIDI 2.0 key ([`Class`] reads every note as one) that says so.
const MIDI2_KEY: u16 = 1 << 15;

/// Notes `raw` as started or ended in `held`.
fn track(held: &mut [u64; HELD_WORDS], raw: tutti_ump::Ump) {
    match Class::of(raw) {
        Class::On(key) => {
            let at = bit(key);
            held[at >> 6] |= 1 << (at & 63);
        }
        Class::Off(key) => {
            let at = bit(key);
            held[at >> 6] &= !(1 << (at & 63));
        }
        Class::Other => {}
    }
}

/// Writes a Note Off for each key `held` has, at `at`, clearing a key only when
/// the port took its off; the first refusal (the port is full) leaves the rest
/// for the next block. A refused off is deferred, not lost: the refusal is given
/// back at once, so the executor's lost count does not carry it.
fn end_notes(held: &mut [u64; HELD_WORDS], out: &mut tutti_graph::EventWriter<'_>, at: Offset) {
    for (word, bits) in held.iter_mut().enumerate() {
        let mut rest = *bits;
        while rest != 0 {
            let low = rest.trailing_zeros() as u16;
            let key = NoteKey::from_index(MIDI2_KEY | (word as u16) << 6 | low);
            if out.push(owed_off(key, at)).is_err() {
                out.recovered();
                return;
            }
            rest &= rest - 1;
            *bits &= !(1 << low);
        }
    }
}

/// The key's place in the table.
fn bit(key: NoteKey) -> usize {
    usize::from(key.index() & !MIDI2_KEY)
}

/// A [`DeviceGeneration`] the node reads each block, and the generations of the
/// two blocks before.
#[derive(Debug)]
struct Follow {
    cell: DeviceGeneration,
    /// The generation read at the start of the last block, and of the one before.
    seen: [u64; 2],
    /// The generation last stored as handled.
    acknowledged: u64,
}

/// Where the node is in a retirement, at the start of a block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No block has read the retirement yet, or it is the block that did.
    Live,
    /// The last block read the retirement: this block passes what reaches it,
    /// the last that does, and ends the notes it started.
    Closing,
    /// The block that passed its notes' ends has gone: nothing more is passed.
    Closed,
}

/// Whether a generation says its device (the handle) is gone: odd while there.
const fn departed(generation: u64) -> bool {
    generation.is_multiple_of(2)
}

impl Follow {
    /// Reads the generation, and acknowledges the one read two blocks ago.
    ///
    /// A generation is handled at the start of the second block after the one
    /// that read it. Block N read it inside `process`, so N's inlets may have
    /// run before the app's last send; block N+1 began after N's node saw the
    /// generation, so its inlets see every send made before it moved, and N+1
    /// has ended, taps included, when N+2 starts.
    ///
    /// The phase is by what the blocks before read: N+1 is the one that
    /// [`Closing`](Phase::Closing) ends the notes in, and N+2 on are
    /// [`Closed`](Phase::Closed).
    fn step(&mut self, owing: impl FnOnce() -> bool) -> Phase {
        let [last, before] = self.seen;
        let now = self.cell.generation();
        // A retired generation is handled only when no note is left to end: the
        // control side waits for the offs the port had no room for.
        if before != self.acknowledged && (!departed(before) || !owing()) {
            self.cell.mark_handled(before);
            self.acknowledged = before;
        }
        self.seen = [now, last];
        if departed(before) {
            Phase::Closed
        } else if departed(last) {
            Phase::Closing
        } else {
            Phase::Live
        }
    }
}

impl Clone for MidiOutNode {
    /// A copy that follows nothing. A fork is an isolated duplicate with an
    /// executor of its own (an offline render, an export), never a stand-in for
    /// the live node of the session graph, so it has no retirement to
    /// acknowledge.
    fn clone(&self) -> Self {
        Self {
            staged: self.staged.clone(),
            follow: None,
            held: None,
        }
    }
}

impl MidiOutNode {
    const SHAPE: Shape = Shape::EMPTY
        .with_events(1, 1)
        .with_event_capacity(MIDI_OUT_PORT_CAPACITY)
        // Event-only: a copy per event, nothing per frame.
        .with_cost(tutti_graph::Cost::Light);
    const IN: InEvents = Self::SHAPE.event_input(0).expect("declared");
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");

    /// A node with room for [`MIDI_OUT_EVENT_CAPACITY`] events a block.
    #[must_use]
    pub fn new() -> Self {
        Self {
            staged: vec![BLANK; MIDI_OUT_EVENT_CAPACITY].into_boxed_slice(),
            follow: None,
            held: None,
        }
    }

    /// Acknowledges the retirement `generation` says (it says the handle is
    /// there while odd: [`arrive`](crate::GenerationWriter::arrive) before the
    /// node is built, [`depart`](crate::GenerationWriter::depart) to retire).
    ///
    /// The node stores the generation it has handled at the second block after
    /// the one that read it, so the control side takes the node out
    /// ([`is_idle`](DeviceGeneration::is_idle)) once what was queued to it before the
    /// generation moved has reached its tap. A node that is never processed
    /// never goes idle.
    ///
    /// **What reaches the tap is what reaches the node within those two blocks.**
    /// An event on a delayed or latency-compensated path, in a feedback loop, or
    /// stamped for a later block does not, and is not sent; the node counts the
    /// events it drops once retired as lost (`Io::count_lost`).
    ///
    /// **A retiring node ends what it started.** It tracks the notes it passed
    /// on; in the last block it passes events it writes a Note Off for each,
    /// at the block's last event, and afterwards passes nothing. The offs are
    /// explicit Note Offs, as MIDI 1.0's Detailed Specification (Channel Mode
    /// Messages, All Notes Off) says All Notes Off does not replace a Note Off,
    /// and a receiver in Omni mode may ignore it; they are MIDI 2.0 Note Offs
    /// whichever protocol the note was started in, as the ledger's are.
    /// What a block's room does not hold is refused and counted by the executor.
    ///
    /// Its copies (the forks) follow nothing.
    #[must_use]
    pub fn following(mut self, generation: &DeviceGeneration) -> Self {
        debug_assert!(
            !departed(generation.generation()),
            "a node that follows a departed generation passes nothing"
        );
        self.follow = Some(Follow {
            cell: generation.clone(),
            seen: [generation.generation(); 2],
            // Nothing stored yet: the first block stores.
            acknowledged: u64::MAX,
        });
        self.held = Some(Box::new([0; HELD_WORDS]));
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
        Self::SHAPE
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let held = &self.held;
        let phase = self.follow.as_mut().map_or(Phase::Live, |follow| {
            follow.step(|| {
                held.as_deref()
                    .is_some_and(|keys| keys.iter().any(|w| *w != 0))
            })
        });
        let mut kept = 0;
        let mut lost = 0;
        for event in io.events(Self::IN) {
            let EventKind::Midi(raw) = event.kind else {
                continue;
            };
            if phase == Phase::Closed {
                lost += 1;
            } else if let Some(slot) = self.staged.get_mut(kept) {
                *slot = (event.offset, raw);
                kept += 1;
            } else {
                lost += 1;
            }
        }
        if lost > 0 {
            // Past the node's room, or retired: counted by the executor, as a
            // refused push is.
            io.count_lost(lost);
        }
        let mut at = Offset::ZERO;
        for (offset, raw) in &self.staged[..kept] {
            // Refused past the capacity: counted by the executor, and a note
            // the port refused is not one to end.
            if io
                .event_out(Self::OUT)
                .push(Event::midi(*offset, *raw))
                .is_ok()
            {
                at = *offset;
                if let Some(held) = self.held.as_deref_mut() {
                    track(held, *raw);
                }
            }
        }
        if let Some(held) = self.held.as_deref_mut()
            && phase != Phase::Live
        {
            end_notes(held, io.event_out(Self::OUT), at);
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
