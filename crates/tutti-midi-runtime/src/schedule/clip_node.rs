//! [`MidiClipNode`]: a MIDI clip as a graph node, playing its events
//! out of an event port.
//!
//! The node reads the transport from its block's [`Env`](tutti_graph::Env), not from a shared
//! timeline: every block it asks, segment by segment, which of its events
//! playback reaches inside the segment, and writes each on its frame. It keeps
//! **no play cursor**. Which events fall in a block is a function of the
//! block's transport and the clip, answered by a binary search over the
//! sorted events and the engine's one beat→frame rule ([`Env::due`](tutti_graph::Env::due)), so a
//! seek, a loop wrap or a tempo change inside a block needs no bookkeeping,
//! and an offline fork plays the clip on its render's `Env`.
//!
//! What it does keep is the set of notes it has started and not ended
//! (a [`NoteLedger`]): when playback stops, jumps (a seek, a loop wrap), or the
//! clip is replaced, those notes would hang in whatever it feeds, so it ends
//! each with a note-off on the frame the jump happens, and then returns the
//! hold pedals (CC 64, 66, 67, 69) and the pitch bend it left off their
//! normal position to it, on the same frame (MIDI 1.0 Detailed Spec p. 31,
//! Stop; the pedals after the offs because a pedal that is down holds a note
//! through its Note Off, A-5). It writes only through
//! the ledger's [`Sending`], which pays a note-off the port refused before
//! anything else goes out.
//!
//! Then it **chases**: wherever playback jumps to a beat (a start, a seek, a
//! loop wrap, a bounce's first block), it sends, on the jump's frame, the
//! last message the clip set before that beat for each state a channel holds
//! (its program, each controller, each registered or assignable controller,
//! its pressure and its bend), so what it feeds hears the timeline's state
//! there. A table built with the clip, each state's setters in beat order,
//! makes that a binary search per state. Notes are not chased (a note the
//! playhead lands inside is not struck), nor per-note or relative
//! controllers.
//!
//! A clip may carry a **header** ([`ClipEvents::header`]): messages that
//! configure what the node feeds (a Program Change, a volume, System
//! Exclusive), which are not events of the clip. The node sends them, in
//! their order, all in the one block, on the frame, where the clip starts:
//! each time playback starts from stopped, and when the clip is replaced
//! while playing (the new clip's header), and again when the host says the
//! receiver changed ([`MidiClipControls::receiver_changed`]). The header
//! goes before the chase and the chase before the events, so a receiver ends
//! on the clip's own state. A loop wrap or a seek does not send it: the
//! receiver was configured when the run started. A header message the port
//! has no room for is lost, counted with every other refused event
//! (`Editor::take_event_losses`), and never waited on. A header message goes
//! out whole or not at all: a System Exclusive run (a Start, its Continues and
//! its End, or a run that never ends, which is one message to its last
//! packet) is sent only if the port has room for every packet. The header stops
//! at the first message that does not fit, and each packet from it on is
//! counted lost, so the receiver holds a prefix of the header in file order,
//! never a message without its end nor a later message that leans on a lost
//! earlier one.
//!
//! A key (group, channel, note number) is held or not (the MIDI convention),
//! by one note at a time, as a DAW plays notes of one key that overlap (M2-104
//! Appendix C.2 would allow several, told apart by a Detach, which a clip does
//! not carry): a note-on of a key it already holds ends the sounding note
//! first, on the same frame, and the new note owns the key. From then on only
//! the owning note's note-off ends the key; any other note's note-off for it
//! is swallowed. So what it feeds never has
//! two of its voices on one key, a note nested inside another ends at its
//! own end, overlaps of any depth leave the newest note sounding to its
//! end, and nothing is left sounding.
//!
//! Which note-off is a note's is decided when the clip is set: a note-off
//! belongs to the earliest-listed note-on of its key that plays before it
//! (on an earlier beat, or on its beat and listed before it) and is not
//! already taken, in the order the events are given. A
//! host that lists each note's on and off together gets its notes exactly.
//! A list already in time order (a parsed file) pairs notes of one key
//! first on, first off, which is right for notes that overlap and, for a
//! note nested inside a longer one, keeps the inner note sounding to the
//! outer's end: a time-ordered list cannot tell those two apart. A note-off
//! that belongs to no note-on is written as it comes, and ends whatever note
//! holds its key.
//!
//! The clip itself is a [`ClipEvents`] published to the node with
//! [`Publisher`] ([`MidiClipControls::set`]); the node reads it once per
//! block.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::sync::Arc;
use tutti_buffers::Float;
use tutti_rt::Mutex;

use tutti_graph::{
    Cx, EventDensity, ForkCause, ForkSource, Forked, HasPublished, IntoNode, Io,
    MIN_EVENT_PORT_CAPACITY, Node, NodeParts, Offset, OutEvents, Prepare, Published, PublishedKind,
    RenderMode, Shape, Status,
};
use tutti_midi_protocol::Midi2Translator;
use tutti_rt::{AtomicRequestFlags, Publisher, Reader, RequestFlag};
use tutti_ump::{
    Bank, CCNumber, Message, MidiChannel, MidiGroup, NoteKey, NoteNumber, NoteVelocity, U7, Ump,
    Voice1, Voice2, spans,
};
use tutti_units::{Beat, BeatDuration, FramesPerBeat};

use crate::ledger::{Class, NoteLedger, Sending};

use super::timed::TimedMidiEvent;
use super::walk::{Beated, Visit, Walk};

/// Keys a clip node tracks at once, held or refused. One group's 16 channels
/// of 128 notes is 2,048, so a clip on one group never fills it; a note-on
/// that finds it full is skipped and counted, never dropped silently. At 8
/// bytes a key the table is 16 KiB, allocated with the node.
const LEDGER_CAPACITY: usize = 2048;

/// The events per frame a [`MidiClipNode`]'s port is sized for, which with
/// its block makes its declared [`event_capacity`](Shape::event_capacity)
/// ([`EventDensity::capacity`]): one a frame, from 256 events for a short
/// block to 8,192 (`MAX_EVENT_PORT_CAPACITY`, the engine's default largest
/// block). A clip's events are on distinct frames
/// but for a chord, which the floor covers; 48,000 events a second is a
/// 16-channel MPE recording with a bend, a pressure and a slide each
/// millisecond on every channel.
///
/// Memory, with the note-off room (`note_off_room`: capacity, an eighth and 8)
/// at 32 bytes an event: the port's slot is 9.5 KB at 256 events, 37 KB at
/// 1,024 and 295 KB at 8,192 (slots are shared where lifetimes allow). Each
/// PDC delay on the port adds a FIFO of `3.75 ×` that many entries of 40
/// bytes and an output slot of 32, 72 bytes an entry: 81 KB, 314 KB and 2.5 MB
/// for a delay no longer than the block, more for a longer one (`⌈len /
/// block⌉ + 2` blocks of the rate, not 3).
///
/// A block that reaches more has the rest refused, and the graph counts them
/// (`Editor::take_event_losses`); a note-off the port refuses stays owed and
/// goes out in a following block, and the note-offs have room past the
/// capacity ([`note_off_room`](tutti_graph::note_off_room)).
pub const CLIP_EVENT_DENSITY: EventDensity = EventDensity::per_frame(1);

/// A clip as it is published: its events sorted by beat with each note-off
/// paired to its note-on, and the chase table that makes a jump to a beat
/// a binary search.
///
/// [`notes`](Self::notes) lists the notes the pairing gives.
///
/// Collect a clip from [`TimedMidiEvent`]s in any order; the sorting, the
/// translation of MIDI 1.0 events to MIDI 2.0 and the pairing are done there,
/// so the thread that publishes it does none of it. A clip with a
/// [`header`](Self::header) is collected, then given it. Two clips are equal
/// when their events, pairing and header are: a clip that differs only in its
/// header is a new clip, which [`MidiClipControls::set`] publishes.
#[derive(Debug, Default)]
pub struct ClipEvents {
    events: Box<[ClipEvent]>,
    /// For each state a channel holds (a program, a controller, a registered
    /// or assignable controller, pressure, bend), the messages that set it,
    /// in beat order; the states in the order a chase sends them
    /// ([`chase_table`]). Built on the control side with the clip.
    chase: Box<[Box<[Setter]>]>,
    /// Parallel to `events`: at a note-on's index, the index of the note-off
    /// that ends it. Built on the control side with the clip; only
    /// [`notes`](Self::notes) reads it.
    ends: Box<[Option<u32>]>,
    /// The header: the messages the node sends where the clip starts, in file
    /// order, each message the packets of one packet or of one System
    /// Exclusive run. Each packet is at its own time on the header's clock,
    /// counted from 0 at the header's start; the node ignores the times and
    /// sends all at once.
    header: Box<[Box<[TimedMidiEvent]>]>,
}

/// A note of a clip: a note-on and the note-off that ends it, if one does
/// ([`ClipEvents::notes`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipNote {
    /// The UMP group.
    pub group: MidiGroup,
    /// The channel.
    pub channel: MidiChannel,
    /// The key.
    pub note: NoteNumber,
    /// The Note On velocity at MIDI 2.0 width: a clip stores MIDI 2.0, so a
    /// MIDI 1.0 velocity is the specification's scaling of it.
    pub velocity: NoteVelocity,
    /// Where the note starts.
    pub start: Beat,
    /// How long it lasts, from its note-on to its note-off; `None` for a
    /// note-on no note-off ends: it sounds until playback stops or jumps, or
    /// the clip is replaced.
    pub length: Option<BeatDuration>,
}

impl ClipEvents {
    /// Control thread: builds the chase table and the note ends.
    fn new(events: Box<[ClipEvent]>) -> Self {
        let chase = chase_table(&events);
        let ends = note_ends(&events);
        Self {
            events,
            chase,
            ends,
            header: Box::default(),
        }
    }

    /// The clip with `packets` as its header: sent where the clip starts (see
    /// the module docs), not events of the clip, so [`len`](Self::len),
    /// [`notes`](Self::notes) and the chase do not read them. The packets
    /// keep the order given and are translated to MIDI 2.0 as the clip's
    /// events are. Each is at its time on the header's own clock: the message's
    /// tick counted from the header's start, in quarter notes (a caller with
    /// no such clock gives 0). The node does not read the times; they are
    /// kept for a player that spaces the header out. The packets of one System
    /// Exclusive run are grouped here (a run that never ends is one group to
    /// its last packet), so the node sends each group whole or not at all.
    /// Control thread.
    #[must_use]
    pub fn header(mut self, packets: impl IntoIterator<Item = TimedMidiEvent>) -> Self {
        let packets = translated(packets);
        self.header = spans(&packets, TimedMidiEvent::event)
            .map(|message| message.packets().into())
            .collect();
        self
    }

    /// The clip's notes in the order of their note-ons in the clip, each with
    /// the length its note-off gives it. A note-off that ends no note-on is
    /// not a note. Control thread; allocates nothing.
    pub fn notes(&self) -> impl Iterator<Item = ClipNote> + '_ {
        self.events
            .iter()
            .zip(&*self.ends)
            .enumerate()
            .filter_map(|(i, (event, end))| {
                // A note-on is the event that carries its own id.
                event.note.filter(|id| id.index() as usize == i)?;
                let Ok(Message::Midi2(address, Voice2::NoteOn { note, velocity, .. })) =
                    event.timed.event().decode()
                else {
                    return None;
                };
                let start = event.timed.beat();
                Some(ClipNote {
                    group: address.group,
                    channel: address.channel,
                    note,
                    velocity,
                    start,
                    length: end.map(|off| self.events[off as usize].timed.beat() - start),
                })
            })
    }

    /// How many events the clip holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the clip holds no event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl FromIterator<TimedMidiEvent> for ClipEvents {
    fn from_iter<I: IntoIterator<Item = TimedMidiEvent>>(events: I) -> Self {
        Self::new(sorted(translated(events)))
    }
}

/// Equal by events, pairing and header: the chase table is a function of the
/// events.
impl PartialEq for ClipEvents {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events && self.header == other.header
    }
}

/// A message that sets a state, at its beat: what a chase sends for it.
#[derive(Clone, Copy, Debug)]
struct Setter {
    beat: Beat,
    ump: Ump,
}

/// What a state message sets on its channel, in the order a chase sends
/// them: the program (its bank rides in the MIDI 2.0 Program Change) before
/// the controllers it may reset, the registered and assignable controllers
/// (a bend range among them) before the bend they scale, pressure, then the
/// bend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Chased {
    Program,
    Registered(U7, U7),
    Assignable(U7, U7),
    Control(CCNumber),
    Pressure,
    Bend,
}

/// The state a channel voice message sets, if it sets one a chase restores:
/// a channel's absolute state. Notes are not chased (a note the playhead
/// lands inside is not started), nor are per-note controllers, which live
/// with their notes, nor relative controllers, which add to what is there,
/// nor the Channel Mode messages (CC 120 to 127, M1 p.20): All Sound Off and
/// All Notes Off are actions, and Reset All Controllers, Local Control,
/// Omni and Mono/Poly are not a controller's value but a command that would
/// undo what the chase just restored.
fn held_by(voice: Voice2) -> Option<Chased> {
    Some(match voice {
        Voice2::ProgramChange { .. } => Chased::Program,
        Voice2::Registered { bank, index, .. } => Chased::Registered(bank, index),
        Voice2::Assignable { bank, index, .. } => Chased::Assignable(bank, index),
        Voice2::ControlChange { controller, .. } if controller < CCNumber::ALL_SOUND_OFF => {
            Chased::Control(controller)
        }
        Voice2::ChannelPressure(_) => Chased::Pressure,
        Voice2::PitchBend(_) => Chased::Bend,
        _ => return None,
    })
}

/// The chase table of `events` (sorted by beat): each state's setting
/// messages in beat order, the states by group, channel and [`Chased`] order.
/// A Program Change with no bank (none was selected since the last, so the
/// channel keeps its bank) is held with the bank in force at its beat, so a
/// chase to it selects the timeline's bank, not whatever the receiver was
/// last on. Control thread.
fn chase_table(events: &[ClipEvent]) -> Box<[Box<[Setter]>]> {
    let mut table: BTreeMap<(usize, usize, Chased), Vec<Setter>> = BTreeMap::new();
    let mut banks: HashMap<(usize, usize), Bank> = HashMap::new();
    for e in events {
        let Ok(Message::Midi2(address, voice)) = e.timed.event().decode() else {
            continue;
        };
        let Some(held) = held_by(voice) else {
            continue;
        };
        let channel = (address.group.index(), address.channel.index());
        let mut ump = e.timed.event();
        if let Voice2::ProgramChange { program, bank } = voice {
            match bank {
                Some(bank) => {
                    banks.insert(channel, bank);
                }
                None => {
                    if let Some(&bank) = banks.get(&channel) {
                        ump = Ump::from(Message::Midi2(
                            address,
                            Voice2::ProgramChange {
                                program,
                                bank: Some(bank),
                            },
                        ));
                    }
                }
            }
        }
        table
            .entry((channel.0, channel.1, held))
            .or_default()
            .push(Setter {
                beat: e.timed.beat(),
                ump,
            });
    }
    table.into_values().map(Vec::into_boxed_slice).collect()
}

/// For each note-on of `events`, the index of its note-off (see
/// [`ClipEvents::notes`]); `None` elsewhere. Control thread.
fn note_ends(events: &[ClipEvent]) -> Box<[Option<u32>]> {
    let mut ends = vec![None; events.len()];
    for (i, event) in events.iter().enumerate() {
        if let Some(id) = event.note
            && id.index() as usize != i
        {
            ends[id.index() as usize] = Some(i as u32);
        }
    }
    ends.into()
}

/// One event of a clip, with the note it starts or ends.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ClipEvent {
    timed: TimedMidiEvent,
    /// For a note-on, its own id; for a note-off, the id of the note-on it
    /// ends; `None` for a note-off that ends none, and for any other message.
    note: Option<NoteId>,
}

/// A note's id: the index of its note-on in the sorted clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NoteId(NonZeroU32);

impl NoteId {
    fn from_index(index: u32) -> Self {
        Self(NonZeroU32::new(index + 1).expect("clip index below u32::MAX"))
    }

    fn index(self) -> u32 {
        self.0.get() - 1
    }
}

fn class_of(timed: &TimedMidiEvent) -> Class {
    Class::of(timed.event())
}

/// `events` through one [`Midi2Translator`], in beat order (a clip is translated as it is loaded, so the graph carries MIDI 2.0 from it
/// as from a wire), each translated event at its source's beat and in its
/// source's place in the list, which the note pairing below reads. An event
/// the translator absorbs (an RPN select, Data Entry, Bank Select) is gone; a
/// select or Data Entry that ends a run is replaced by the run's controller,
/// and a run the clip ends on (a CC 6 with no CC 38 after it) is sent right
/// after its CC 6's place, on its beat. A MIDI 2.0 Control Change of CC 0, 6,
/// 32, 38 or 98 to 101 is dropped, as the Protocol has Program Change,
/// Registered Controller and Assignable Controller for what they carry and a
/// receiver ignores them (M2-104 section 7.4.6): a native MIDI 2.0 clip holding
/// one plays without it, which is the spec's rule and no data loss.
/// Control thread.
fn translated(events: impl IntoIterator<Item = TimedMidiEvent>) -> Vec<TimedMidiEvent> {
    let events: Vec<TimedMidiEvent> = events.into_iter().collect();
    let mut order: Vec<usize> = (0..events.len()).collect();
    // Stable: events on one beat are translated in list order.
    order.sort_by(|&a, &b| events[a].beat().get().total_cmp(&events[b].beat().get()));
    let mut out: Vec<Option<Ump>> = vec![None; events.len()];
    // A run the clip ends on, at the position of its Data Entry.
    let mut flushed: Vec<Option<Ump>> = vec![None; events.len()];
    // The last Data Entry (CC 6 or 38) of each group and channel: a run still
    // waiting when the clip ends is waiting on it.
    let mut data_entry: HashMap<(MidiGroup, MidiChannel), usize> = HashMap::new();
    let mut translator = Midi2Translator::new();
    for i in order {
        out[i] = translator.translate(events[i].event());
        if let Ok(Message::Midi1(
            address,
            Voice1::ControlChange {
                controller: CCNumber::DATA_ENTRY | CCNumber::DATA_ENTRY_LSB,
                ..
            },
        )) = events[i].event().decode()
        {
            data_entry.insert((address.group, address.channel), i);
        }
    }
    // Nothing follows the last event, so a CC 6 with no CC 38 after it is
    // complete: send it where the CC 6 stood, not at a later trigger that
    // never comes (a clip has no live timeout; M2-104 D.3.3).
    translator.finish(|address, ump| {
        if let Some(&i) = data_entry.get(&(address.group, address.channel)) {
            flushed[i] = Some(ump);
        }
    });
    events
        .iter()
        .zip(out.into_iter().zip(flushed))
        .flat_map(|(e, (sent, flushed))| {
            // Both at the source's beat: the run an earlier CC 6 left, then
            // this CC 6's own.
            [sent, flushed]
                .into_iter()
                .flatten()
                // The beat is the source's, finite already.
                .filter_map(|u| TimedMidiEvent::new(e.beat(), u))
        })
        .collect()
}

/// `events` with each note-off paired to its note-on (see the module docs),
/// sorted by beat, stably (events on one beat keep their order: a note-off
/// before a note-on of the same note stays first). A note's id is its note-on's index in the
/// sorted clip, so two clips with the same events and the same pairing are
/// equal. Control thread.
fn sorted(events: impl IntoIterator<Item = TimedMidiEvent>) -> Box<[ClipEvent]> {
    let mut v: Vec<(u32, ClipEvent)> = events
        .into_iter()
        .enumerate()
        .map(|(i, timed)| (i as u32, ClipEvent { timed, note: None }))
        .collect();
    // Each key's note-ons in list order, and a cursor past the taken ones at
    // its head (so notes that do not overlap pair in one step each).
    let mut ons: HashMap<NoteKey, (Vec<u32>, usize)> = HashMap::new();
    for (i, (_, e)) in v.iter().enumerate() {
        if let Class::On(key) = class_of(&e.timed) {
            ons.entry(key).or_default().0.push(i as u32);
        }
    }
    let mut taken = vec![false; v.len()];
    for i in 0..v.len() {
        let Class::Off(key) = class_of(&v[i].1.timed) else {
            continue;
        };
        let Some((list, first)) = ons.get_mut(&key) else {
            continue;
        };
        while *first < list.len() && taken[list[*first] as usize] {
            *first += 1;
        }
        let beat = v[i].1.timed.beat().get();
        // A note-on ends at a note-off that plays after it: on a later beat,
        // or on its own beat and later in the list.
        let found = list[*first..].iter().copied().find(|&o| {
            let on = v[o as usize].1.timed.beat().get();
            !taken[o as usize] && (on < beat || (on == beat && (o as usize) < i))
        });
        if let Some(o) = found {
            taken[o as usize] = true;
            let id = Some(NoteId::from_index(o));
            v[o as usize].1.note = id;
            v[i].1.note = id;
        }
    }
    // A note-on no note-off ends is a note too.
    for (i, (_, e)) in v.iter_mut().enumerate() {
        if e.note.is_none() && matches!(class_of(&e.timed), Class::On(_)) {
            e.note = Some(NoteId::from_index(i as u32));
        }
    }
    v.sort_by(|a, b| a.1.timed.beat().get().total_cmp(&b.1.timed.beat().get()));
    // List index → sorted index, to renumber each note by its sorted note-on.
    let mut to_sorted = vec![0u32; v.len()];
    for (k, &(i, _)) in v.iter().enumerate() {
        to_sorted[i as usize] = k as u32;
    }
    v.into_iter()
        .map(|(_, mut e)| {
            e.note = e
                .note
                .map(|id| NoteId::from_index(to_sorted[id.index() as usize]));
            e
        })
        .collect()
}

/// The handle a host keeps on an inserted [`MidiClipNode`]: replace the clip
/// it plays. Cheap to clone; every clone reaches the same node. Control
/// thread.
#[derive(Clone)]
pub struct MidiClipControls {
    cell: Publisher<ClipEvents>,
    /// Serializes `set`'s compare and publish (`Publisher` has no
    /// compare-and-publish), so two control threads cannot each publish
    /// against a stale read. The node never takes it.
    serial: Arc<Mutex<()>>,
    /// Raised by [`receiver_changed`](Self::receiver_changed), taken by the
    /// node once a block.
    receiver: Arc<AtomicRequestFlags>,
}

/// The one request a clip node takes: its receiver changed.
const RECEIVER_CHANGED: RequestFlag = RequestFlag::of(0);

impl std::fmt::Debug for MidiClipControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiClipControls").finish_non_exhaustive()
    }
}

impl MidiClipControls {
    /// Plays `events` from the next block on. The node ends every note the
    /// previous clip left sounding, on that block's first frame. A clip equal
    /// to the one in force (the same events, each note-off paired to the same
    /// note-on, and the same header) changes nothing: no note is cut. Control
    /// thread.
    pub fn set(&self, events: impl Into<Arc<ClipEvents>>) {
        let events = events.into();
        let _serial = self.serial.lock();
        let current = self.cell.current();
        if Arc::ptr_eq(&current, &events) || *current == *events {
            return;
        }
        self.cell.publish(events);
    }

    /// The receiver the node feeds changed (it gained a sink, or its sink was
    /// replaced): the node sends the clip's header again, then the state the
    /// clip holds at the playhead, from its next block, and goes on with the
    /// events. The notes it holds keep sounding: they are this clip's, and
    /// only the listener is new (a replaced clip's notes are ended, since they
    /// are not the new clip's). Playback that is stopped sends both at its
    /// next start anyway, so this adds nothing then. Several calls before a
    /// block make one send.
    ///
    /// The header and the chase leave the node's one event output, so every
    /// receiver of that output hears them again, not only the new one: they
    /// restate the state those receivers already hold, and the cost is a device
    /// that cuts its voices on a Program Change.
    /// Control thread; the node reads it without a lock.
    pub fn receiver_changed(&self) {
        self.receiver.raise(RECEIVER_CHANGED);
    }

    /// The clip in force: the one the node plays from its next block.
    #[must_use]
    pub fn current(&self) -> Arc<ClipEvents> {
        self.cell.current()
    }

    /// How many events the clip holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cell.current().len()
    }

    /// Whether the clip holds no event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A MIDI clip in the graph: an event source with one MIDI event output, no
/// audio. See the module docs.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_units::{Beat, SampleRate, Samples};
/// use tutti_graph::NodeKey;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
/// use tutti_fixtures::midi;
/// use tutti_ump::{MidiChannel, NoteVelocity};
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
/// let on = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
/// let clip = MidiClipNode::new([TimedMidiEvent::new(Beat::new(1.0), on).unwrap()]);
/// let controls = editor.insert(NodeKey(1), "clip", clip).controls;
/// assert_eq!(controls.len(), 1);
/// ```
pub struct MidiClipNode {
    /// The cell `clip` reads, which the controls publish to; the node never
    /// touches it.
    cell: Publisher<ClipEvents>,
    /// A reader of `cell`, registered in [`over`](Self::over): a fork's node
    /// is built over its own cell and registers there.
    clip: Reader<ClipEvents>,
    /// The publish number of the clip this node last played. A lend may return
    /// number `N` while the value is already `N + 1`, so a replacement can be
    /// seen one block late, never missed and never seen twice.
    last_seq: u64,
    play: Play,
    /// What [`MidiClipControls::receiver_changed`] raised.
    receiver: Arc<AtomicRequestFlags>,
    /// The declared event capacity, from the block the node was prepared for.
    capacity: NonZeroU32,
}

impl std::fmt::Debug for MidiClipNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiClipNode")
            .field("clip", &self.clip)
            .field("last_seq", &self.last_seq)
            .field("play", &self.play)
            .finish_non_exhaustive()
    }
}

/// Where playback was, and what it left sounding.
#[derive(Debug)]
struct Play {
    walk: Walk,
    held: NoteLedger<NoteId>,
}

impl MidiClipNode {
    const fn declared(capacity: NonZeroU32) -> Shape {
        Shape::EMPTY
            .events(0, 1)
            .event_capacity(capacity)
            // Event-only: a walk over the block's events, nothing per frame
            // (measured about 0.3 µs a block at any length; a gain's 256
            // frames, 0.2).
            .cost(tutti_graph::Cost::Light)
    }
    const OUT: OutEvents = Self::declared(MIN_EVENT_PORT_CAPACITY)
        .event_output(0)
        .expect("declared");

    /// A node playing `events` (in any order: they are sorted by beat).
    pub fn new(events: impl IntoIterator<Item = TimedMidiEvent>) -> Self {
        Self::over(Publisher::new(events.into_iter().collect::<ClipEvents>()))
    }

    /// A node over `cell`, reading it through a reader registered here.
    /// Control thread: registering may allocate.
    fn over(cell: Publisher<ClipEvents>) -> Self {
        Self {
            clip: cell.reader(),
            last_seq: cell.seq(),
            cell,
            play: Play {
                walk: Walk::default(),
                held: NoteLedger::new(LEDGER_CAPACITY),
            },
            receiver: Arc::default(),
            capacity: MIN_EVENT_PORT_CAPACITY,
        }
    }
}

impl Beated for ClipEvent {
    fn beat(&self) -> Beat {
        self.timed.beat()
    }
}

/// The clip's side of the walk: send each event, and end what is held
/// wherever playback stops or jumps (a seek, a loop wrap), then chase the
/// channel state the clip set before the jump's beat; where it starts (or its
/// receiver changed), send the header first.
struct Playing<'a, 'w, 'c> {
    sending: Sending<'a, 'w, NoteId>,
    chase: &'c [Box<[Setter]>],
    header: &'c [Box<[TimedMidiEvent]>],
    /// The port's declared capacity: what a header message's room is read from.
    capacity: usize,
}

impl Playing<'_, '_, '_> {
    /// Sends the header at `at`, message by message, each whole or not at all.
    /// At the first message the port has no room for, every packet from it on
    /// is counted lost and nothing more is sent.
    fn send_header(&mut self, at: Offset) {
        for (i, message) in self.header.iter().enumerate() {
            if message.len() > self.capacity.saturating_sub(self.sending.written()) {
                let lost = self.header[i..].iter().map(|m| m.len()).sum();
                self.sending.count_lost(lost);
                return;
            }
            for packet in message {
                self.sending.send(at, packet.event(), None);
            }
        }
    }

    /// Sends, for each state the clip set before `beat`, the last message that
    /// set it, at `at`.
    fn send_chase(&mut self, at: Offset, beat: Beat) {
        for setters in self.chase {
            let before = setters.partition_point(|s| s.beat < beat);
            if let Some(last) = before.checked_sub(1).and_then(|k| setters.get(k)) {
                self.sending.send(at, last.ump, None);
            }
        }
    }
}

impl Visit<ClipEvent> for Playing<'_, '_, '_> {
    fn stop(&mut self, at: Option<Offset>) {
        // Stopped (or no usable tempo): nothing plays, and nothing may keep
        // sounding or stay held down.
        if let Some(o) = at {
            self.sending.release_all(o);
        }
    }

    /// Ends what is held, then sends, for each state the clip set before
    /// `beat`, the last message that set it: playback starting there (a
    /// seek, a loop wrap, a start, a bounce's first block) hears the
    /// programs and controllers the timeline holds at that beat (the chase). An event on `beat` itself, or less than a frame behind
    /// it, the walk plays as well; that repeats a state, which is harmless.
    ///
    /// A port without room for the whole chase refuses the rest, counted as
    /// the graph counts any refused event (`Editor::take_event_losses`): a
    /// clip that sets many states on many channels can have its chase cut
    /// short on a short block, and the states it set first (lower groups and
    /// channels, a channel's program first) are the ones that land.
    fn jump(&mut self, at: Option<Offset>, beat: Beat, _fpb: FramesPerBeat, _events: &[ClipEvent]) {
        let Some(o) = at else { return };
        self.sending.release_all(o);
        self.send_chase(o, beat);
    }

    /// Ends what is held, then the header, then the chase: the order a
    /// replacement sends them in.
    fn start(&mut self, at: Option<Offset>, beat: Beat, _: FramesPerBeat, _: &[ClipEvent]) {
        let Some(o) = at else { return };
        self.sending.release_all(o);
        self.send_header(o);
        self.send_chase(o, beat);
    }

    /// The header, then the chase, with what is held left sounding: the notes
    /// are the same clip's, only the receiver is new.
    fn resend(&mut self, at: Option<Offset>, beat: Beat, _: FramesPerBeat, _: &[ClipEvent]) {
        if let Some(o) = at {
            self.send_header(o);
            self.send_chase(o, beat);
        }
    }

    fn event(&mut self, at: Offset, e: &ClipEvent) {
        self.sending.send(at, e.timed.event(), e.note);
    }
}

impl<F: Float> Node<F> for MidiClipNode {
    /// No audio, one MIDI event output of [`CLIP_EVENT_DENSITY`] per frame of
    /// the block it was prepared for.
    fn shape(&self) -> Shape {
        Self::declared(self.capacity)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.capacity = CLIP_EVENT_DENSITY.capacity(p);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let clip = self.clip.lend();
        let Some(first) = cx.env.offset(0) else {
            return Status::Modified;
        };
        let seq = clip.seq();
        let replaced = std::mem::replace(&mut self.last_seq, seq) != seq;
        let Play { walk, held } = &mut self.play;
        let mut playing = Playing {
            sending: held.block(io.event_out(Self::OUT), first),
            chase: &clip.chase,
            header: &clip.header,
            capacity: self.capacity.get() as usize,
        };
        if replaced {
            playing.sending.release_all(first);
        }
        let changed = !self.receiver.take().is_empty();
        if replaced || changed {
            walk.resend();
        }
        for (start, seg) in cx.env.segments() {
            walk.segment(&clip.events, cx.env, start, &seg, &mut playing);
        }
        Status::Modified
    }

    /// Forget what was sounding and where playback was: a reset starts the
    /// node over, and whatever it fed is reset with it.
    fn reset(&mut self) {
        self.play.held.forget();
        self.play.walk.forget();
    }
}

impl<F: Float> IntoNode<F> for MidiClipNode {
    type Controls = MidiClipControls;

    fn into_parts(self) -> NodeParts<MidiClipControls, F> {
        let controls = MidiClipControls {
            cell: self.cell.clone(),
            serial: Arc::default(),
            receiver: Arc::clone(&self.receiver),
        };
        let fork = ClipFork {
            cell: self.cell.clone(),
        };
        NodeParts::new(Box::new(self), controls.clone(), Some(Box::new(fork)), None)
            .published::<MidiClipNode, ClipEventsKind>(Arc::new(controls))
    }
}

/// The kind of a clip node's events: the clip a session writes through
/// `clip.events()`.
#[derive(Clone, Copy, Debug)]
pub enum ClipEventsKind {}

impl PublishedKind for ClipEventsKind {
    type Value = Arc<ClipEvents>;
}

impl HasPublished<ClipEventsKind> for MidiClipNode {}

/// The clip a session writes: [`set`](MidiClipControls::set), which an equal
/// clip leaves in force.
impl Published for MidiClipControls {
    type Value = Arc<ClipEvents>;

    fn current(&self) -> Arc<ClipEvents> {
        Self::current(self)
    }

    fn replace(&self, value: &Arc<ClipEvents>) {
        self.set(Arc::clone(value));
    }
}

/// A clip node's fork: a fresh node over the clip as it is when the fork is
/// taken, so an export plays what was there at its start while the live clip
/// is edited. The fork reads its render's `Env`.
struct ClipFork {
    cell: Publisher<ClipEvents>,
}

impl<F: Float> ForkSource<F> for ClipFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        let cell = Publisher::from_arc(self.cell.current());
        Ok(Forked::new(Box::new(MidiClipNode::over(cell))))
    }
}

/// Frees what the control side retired and no block holds, without publishing
/// (the clips a replacement left behind); control thread, from the owner's tick.
impl tutti_rt::Reclaim for MidiClipControls {
    fn reclaim(&self) -> usize {
        self.cell.reclaim()
    }
}

#[cfg(test)]
mod tests;
