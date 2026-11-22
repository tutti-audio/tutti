//! The MPE transform as a graph node.
//!
//! [`MpeTransformNode`] runs one [`MpeTranslator`] over one event stream: MIDI 1.0
//! in is translated by a [`Midi2Translator`], classic-MPE channel-spread comes out as
//! native per-note messages, and a Manager Channel's zone-wide messages and bend
//! come out on each member channel of its zone, as the MPE translator's docs say. It is
//! the input edge's transform for a stream that does not come off a wire (a
//! clip's), so a graph holds the MPE zones in two kinds of place alone, this
//! node and [`MidiInputNode`](crate::MidiInputNode), and no instrument holds
//! any.
//!
//! One event input and one event output, every Group. Events that are not MIDI
//! are not forwarded. The zones are the [`MpeMode`] of the cell the node is
//! built over ([`MpeTransformNode::new`]), which the session owns and every
//! reader of it shares; the node adopts a new one at the next block. A mode
//! starts each Group at its zones' default bend ranges and a change of it stops
//! and resets the channels that enter or leave MPE control, which is what a
//! receiver's zone change sends ([`MpeTranslator::zone_defaults`]); the transform
//! sends it for a Group the moment the mode changes when the Group has carried
//! events, and ahead of the Group's first event when it has not, so a stream
//! uses one Group and the other fifteen cost nothing. The output can hold more events than the input
//! (a zone-wide message is one per channel of the zone): a block's translation
//! goes in whole or is dropped whole, and the drops are counted
//! ([`MpeTransformControls::losses`]).
//!
//! Its fork is a transform in the mode the cell holds when it is made, which
//! it keeps: a later write to the cell does not reach it, as an export renders
//! the session as it stood at the fork. It has no state of the notes: an
//! offline render starts its clip from the top.

use std::num::NonZeroU32;
use tutti_buffers::Float;
use tutti_graph::{
    Cost, Cx, Event, EventKind, ForkCause, ForkSource, Forked, InEvents, IntoNode, Io, Node,
    NodeParts, Offset, OutEvents, Prepare, RenderMode, Shape, Status, UmpAt, note_off_room,
};
use tutti_midi_protocol::{Midi2Translator, MpeMode};
use tutti_rt::{Counter, CounterWriter, Publisher, Reader, counter};
use tutti_ump::{MidiGroup, MidiGroupSet};
use tutti_units::Losses;

use super::input_node::{MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORT_CAPACITY, Staging};
use super::mpe_translator::MpeTranslator;
use super::order::BLANK;
use crate::schedule::CLIP_EVENT_DENSITY;

/// The MPE transform of one event stream as a graph node. See the module docs.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_units::{SampleRate, Samples};
/// use tutti_graph::{Editor, NodeKey, Prepare};
/// use tutti_midi_protocol::MpeMode;
/// use tutti_midi_runtime::MpeTransformNode;
/// use tutti_rt::Publisher;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
/// let mode = Publisher::new(MpeMode::Disabled);
/// let controls = editor.insert(NodeKey(1), "mpe", MpeTransformNode::new(&mode)).controls;
/// assert_eq!(controls.losses().refused, 0);
/// ```
pub struct MpeTransformNode {
    translator: Midi2Translator,
    mpe: MpeTranslator,
    /// The mode in force.
    mode: MpeMode,
    /// The session's mode, read once per block.
    request: Reader<MpeMode>,
    /// The cell `request` reads, kept for the fork, which registers a reader
    /// of its own on it; the node never touches it.
    request_cell: Publisher<MpeMode>,
    /// What the block sends, which may outnumber what it read.
    out: Box<[UmpAt]>,
    /// The events a block translates and the port holds.
    capacity: NonZeroU32,
    /// The Groups that have carried an event.
    active: MidiGroupSet,
    /// The Groups that were sent the mode in force.
    told: MidiGroupSet,
    /// Translated events a block had no room for.
    dropped: CounterWriter<Losses>,
    /// The reading end of `dropped`, given to the controls.
    dropped_reader: Counter<Losses>,
}

impl std::fmt::Debug for MpeTransformNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpeTransformNode")
            .field("mode", &self.mode)
            .field("dropped", &self.dropped_reader)
            .finish_non_exhaustive()
    }
}

/// Sends `group` the mode's stops and default ranges at `at`; the Group is
/// `told` if they all fit, and is otherwise sent them again ahead of its next
/// event.
fn tell(
    mpe: &MpeTranslator,
    told: &mut MidiGroupSet,
    group: MidiGroup,
    at: Offset,
    staging: &mut Staging<'_>,
) {
    if staging.whole(false, |send| mpe.zone_defaults(group, at, send)) {
        told.insert(group);
    }
}

impl MpeTransformNode {
    const fn declared(capacity: NonZeroU32) -> Shape {
        Shape::EMPTY
            .events(1, 1)
            .event_capacity(capacity)
            // Event-only: a translation per event, nothing per frame.
            .cost(Cost::Light)
    }
    const IN: InEvents = Self::declared(MIDI_INPUT_PORT_CAPACITY)
        .event_input(0)
        .expect("declared");
    const OUT: OutEvents = Self::declared(MIDI_INPUT_PORT_CAPACITY)
        .event_output(0)
        .expect("declared");

    /// A transform that starts in the mode `mode` holds and follows it: a
    /// publish to `mode` is adopted at the next block. The reader is
    /// registered here, so build the node on the control side.
    ///
    /// A new mode stops and resets the channels that enter or leave MPE
    /// control (M1-100 section 2.2.3: All Sound Off and Reset All Controllers,
    /// sent ahead of the new zones' default bend ranges); the others keep
    /// their notes. Publishing the mode in force changes nothing.
    #[must_use]
    pub fn new(mode: &Publisher<MpeMode>) -> Self {
        let initial = *mode.current();
        let (dropped, dropped_reader) = counter();
        Self {
            translator: Midi2Translator::default(),
            mpe: MpeTranslator::new(initial),
            mode: initial,
            request: mode.reader(),
            request_cell: mode.clone(),
            out: vec![BLANK; note_off_room(MIDI_INPUT_EVENT_CAPACITY)].into_boxed_slice(),
            capacity: MIDI_INPUT_PORT_CAPACITY,
            active: MidiGroupSet::EMPTY,
            told: MidiGroupSet::EMPTY,
            dropped,
            dropped_reader,
        }
    }
}

/// The host's handle on a [`MpeTransformNode`]: what it lost. Cheap to clone.
#[derive(Clone, Debug)]
pub struct MpeTransformControls {
    dropped: Counter<Losses>,
}

impl MpeTransformControls {
    /// The translated events a block had no room for, which a translation
    /// drops whole rather than cut partway.
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.dropped.get()
    }
}

impl<F: Float> Node<F> for MpeTransformNode {
    /// No audio; one event input and one event output of the prepared capacity:
    /// [`CLIP_EVENT_DENSITY`] per frame, which is what a clip's port holds, so
    /// a clip's events pass whole (a fan-out may reach past it, and is counted).
    fn shape(&self) -> Shape {
        Self::declared(self.capacity)
    }

    fn prepare(&mut self, p: &Prepare) {
        let capacity = CLIP_EVENT_DENSITY.capacity(p);
        if capacity != self.capacity {
            self.capacity = capacity;
            self.out = vec![BLANK; note_off_room(capacity.get() as usize)].into_boxed_slice();
        }
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let requested = *self.request.lend();
        let mut staging = Staging::new(&mut self.out, self.capacity.get() as usize);
        // A new mode's stops and ranges, at frame 0, ahead of the block's
        // events, for the Groups that have carried some.
        if requested != self.mode {
            self.mode = requested;
            self.mpe.set_mode(requested);
            self.told = MidiGroupSet::EMPTY;
            for group in self.active.iter() {
                tell(&self.mpe, &mut self.told, group, Offset::ZERO, &mut staging);
            }
        }
        for event in io.events(Self::IN) {
            let EventKind::Midi(raw) = event.kind else {
                continue;
            };
            // A Group's first event since the mode was set starts it at the
            // mode's ranges.
            if let Some(group) = raw.group() {
                self.active.insert(group);
                if !self.told.contains(group) {
                    tell(&self.mpe, &mut self.told, group, event.offset, &mut staging);
                }
            }
            // `None` is an absorbed select, or a MIDI 2.0 Control Change of
            // CC 0, 6, 32, 38 or 98 to 101, which a MIDI 2.0 receiver ignores
            // (M2-104 section 7.4.6).
            if let Some(ev) = self.translator.translate(raw) {
                let mpe = &mut self.mpe;
                staging.whole(ev.is_note_off(), |send| {
                    mpe.translate(&(event.offset, ev), send);
                });
            }
        }
        let Staging { kept, dropped, .. } = staging;
        if dropped > 0 {
            self.dropped.add(Losses::refused(dropped));
            // The executor's too, so an export watching its count sees it.
            io.count_lost(dropped);
        }
        for ev in &self.out[..kept] {
            // Refused past the capacity: counted by the executor.
            let _ = io.event_out(Self::OUT).push(Event::midi(ev.0, ev.1));
        }
        Status::Modified
    }

    /// Forgets the notes and what each channel was told, and sends each Group
    /// the mode's ranges again ahead of its next event: a reset receiver is
    /// back at its own defaults.
    fn reset(&mut self) {
        self.translator.reset();
        self.mpe.reset();
        self.told = MidiGroupSet::EMPTY;
    }
}

/// A [`MpeTransformNode`]'s fork: a transform in the mode the cell holds when
/// the fork is taken, over a cell of its own that no write reaches.
struct ModeFork {
    mode: Publisher<MpeMode>,
}

impl<F: Float> ForkSource<F> for ModeFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        let snapshot = Publisher::new(*self.mode.current());
        Ok(Forked::new(Box::new(MpeTransformNode::new(&snapshot))))
    }
}

impl<F: Float> IntoNode<F> for MpeTransformNode {
    type Controls = MpeTransformControls;

    fn into_parts(self) -> NodeParts<MpeTransformControls, F> {
        let mode = self.request_cell.clone();
        let controls = MpeTransformControls {
            dropped: self.dropped_reader.clone(),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(ModeFork { mode })),
        }
    }
}
