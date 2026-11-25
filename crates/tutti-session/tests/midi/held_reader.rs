//! `Midi::held`: the keys and controllers an instrument's event input holds,
//! folded from an event tap. Sequences are written by hand from MIDI 1.0 (Note
//! On at velocity 0 is a Note Off; CC 64 at 64 or more is the pedal down) and
//! M2-104 (a Note On, a Control Change and a Pitch Bend as UMP words).

use crate::common::{Seen, Tap, blocks};
use tutti_graph::{
    Cx, Event, EventCapacity, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Stamp, Status,
};
use tutti_session::{HeldReader, Key, Session};
use tutti_ump::{Held, MidiChannel, NoteNumber, NoteVelocity, Ump};

/// Sends what it is mailed on event output 0.
struct Src(tutti_fixtures::Shared<Vec<[u32; 4]>>);
impl Src {
    const SHAPE: Shape = Shape::EMPTY.events(0, 1);
    const OUT: tutti_graph::OutEvents = Self::SHAPE
        .event_output(0)
        .expect("one event output is declared");
}
impl Node for Src {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for w in self.0.lock().unwrap().drain(..) {
            let _ = io
                .event_out(Self::OUT)
                .push(Event::midi(Offset::ZERO, Ump(w)));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
impl IntoNode for Src {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}

type Mail = tutti_fixtures::Shared<Vec<[u32; 4]>>;

const NOTE_ON_60: [u32; 4] = [0x4090_3c00, 0x8000_0000, 0, 0];
const NOTE_OFF_60: [u32; 4] = [0x4080_3c00, 0, 0, 0];
const PEDAL_DOWN: [u32; 4] = [0x40b0_4000, 0xffff_ffff, 0, 0];
const PEDAL_UP: [u32; 4] = [0x40b0_4000, 0, 0, 0];
const BEND_UP: [u32; 4] = [0x40e0_0000, 0xc000_0000, 0, 0];
const BEND_CENTRE: [u32; 4] = [0x40e0_0000, 0x8000_0000, 0, 0];

/// An instrument standing in as a node with one event input, a source wired
/// into it, and a reader on it.
struct Rig {
    s: Session,
    instrument: Key<Tap>,
    src: Key<Src>,
    mail: Mail,
    reader: HeldReader,
    _seen: Seen,
}

fn rig(capacity: u32) -> Rig {
    let mut s = crate::common::offline();
    let seen = tutti_fixtures::Record::new();
    let instrument = s.graph().add(Tap(seen.clone()));
    let mail = Mail::default();
    let src = s.graph().add(Src(mail.clone()));
    s.graph()
        .connect(src.event_output(0), instrument.event_input(0))
        .unwrap();
    let reader = s
        .midi()
        .held(instrument, EventCapacity::of(capacity))
        .expect("the instrument is in the graph");
    let _ = s.update();
    Rig {
        s,
        instrument,
        src,
        mail,
        reader,
        _seen: seen,
    }
}

impl Rig {
    fn send(&self, words: [u32; 4]) {
        self.mail.lock().unwrap().push(words);
    }

    /// Renders two blocks (the first delivers the mail) and reads.
    fn read(&mut self) -> (&Held, tutti_units::Losses) {
        blocks(&mut self.s, 2);
        self.reader.update()
    }
}

fn notes(held: &Held) -> Vec<u8> {
    held.keys().iter().map(|k| k.note().get()).collect()
}

/// **A key held through an edge ends when the edge is cut**: the executor
/// sends the Note Off, the pedal-up and the centred bend of a source that
/// goes, and a tap of the input hears them.
#[test]
fn a_cut_edge_empties_what_an_input_tap_holds() {
    let mut r = rig(64);
    for w in [NOTE_ON_60, PEDAL_DOWN, BEND_UP] {
        r.send(w);
    }
    let (held, _) = r.read();
    assert_eq!(notes(held), [60]);
    assert_eq!(held.controllers().len(), 2);

    r.s.graph()
        .disconnect(r.src.event_output(0), r.instrument.event_input(0))
        .unwrap();
    let _ = r.s.update();
    let (held, losses) = r.read();
    assert!(held.is_empty(), "the cut ended everything: {held:?}");
    assert_eq!(losses.dropped(), 0);
}

#[test]
fn note_on_holds_its_key_and_note_off_ends_it() {
    let mut r = rig(64);
    r.send(NOTE_ON_60);
    assert_eq!(notes(r.read().0), [60]);
    r.send(NOTE_OFF_60);
    assert!(r.read().0.is_empty());
}

/// MIDI 1.0 Detailed Specification, Channel Voice Messages: a Note On with
/// velocity 0 is a Note Off.
#[test]
fn a_midi1_note_on_with_velocity_zero_ends_the_key() {
    let mut r = rig(64);
    r.send([0x2090_3c40, 0, 0, 0]);
    assert_eq!(notes(r.read().0), [60]);
    r.send([0x2090_3c00, 0, 0, 0]);
    assert!(r.read().0.is_empty());
}

#[test]
fn the_pedal_and_a_bend_are_held_until_they_return() {
    let mut r = rig(64);
    r.send(PEDAL_DOWN);
    r.send(BEND_UP);
    let (held, _) = r.read();
    assert!(held.keys().is_empty());
    assert_eq!(held.controllers().len(), 2);

    r.send(PEDAL_UP);
    assert_eq!(
        r.read().0.controllers().len(),
        1,
        "the bend is still off centre"
    );
    r.send(BEND_CENTRE);
    assert!(r.read().0.is_empty());
}

/// Every source of the port is merged: a wired source and the keys handle.
#[test]
fn two_sources_into_one_port_are_both_held() {
    let mut r = rig(64);
    let mut keys = r.s.midi().input(r.instrument).expect("the live input");
    let _ = r.s.update();
    r.send(NOTE_ON_60);
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(64),
        NoteVelocity::new(0x8000),
    )
    .unwrap();
    assert_eq!(notes(r.read().0), [60, 64]);
}

/// A burst of 40 note-ons into a tap of one event: the tap refuses what does
/// not fit, and the reading says so.
#[test]
fn a_tap_too_small_for_a_burst_reports_what_it_lost() {
    let mut r = rig(1);
    for note in 0..40u32 {
        r.send([0x4090_0000 | (note << 8), 0x8000_0000, 0, 0]);
    }
    let (held, losses) = r.read();
    assert!(
        losses.lost > 0,
        "a lossless reading of a burst that overflowed"
    );
    assert!(held.keys().len() < 40);
}

#[test]
fn a_node_not_in_the_graph_fails_at_the_open() {
    let mut s = crate::common::offline();
    let gone = s.graph().add(Tap(tutti_fixtures::Record::new()));
    s.remove(gone).unwrap();
    let _ = s.update();
    assert!(matches!(
        s.midi().held(gone, EventCapacity::of(8)),
        Err(tutti_session::MidiError::Tap(_))
    ));
}

/// **A note is one key whichever protocol carries it** (M2-104 Appendix D.3
/// translates a MIDI 1.0 and a MIDI 2.0 Note On and Note Off field for field):
/// a MIDI 1.0 Note On is ended by a MIDI 2.0 Note Off of the same Group,
/// Channel and note, and the other way round.
#[test]
fn a_note_begun_in_one_protocol_ends_by_an_off_in_the_other() {
    let mut r = rig(64);
    r.send([0x2090_3c40, 0, 0, 0]);
    assert_eq!(notes(r.read().0), [60]);
    r.send(NOTE_OFF_60);
    assert!(
        r.read().0.is_empty(),
        "the MIDI 2.0 off ends the MIDI 1.0 on"
    );

    r.send(NOTE_ON_60);
    assert_eq!(notes(r.read().0), [60]);
    r.send([0x2080_3c00, 0, 0, 0]);
    assert!(
        r.read().0.is_empty(),
        "the MIDI 1.0 off ends the MIDI 2.0 on"
    );
}

/// **A command scheduled for a later frame is held from that frame**: the tap
/// hears the event in the block that plays it, not when it is sent.
#[test]
fn a_note_scheduled_ahead_is_held_once_its_block_plays() {
    let mut r = rig(64);
    let mut keys = r.s.midi().input(r.instrument).expect("the live input");
    let _ = r.s.update();
    let due = tutti_units::Frame::new(512 * 20);
    keys.note_on(
        Stamp::Frame(due),
        MidiChannel::FIRST,
        NoteNumber::of(67),
        NoteVelocity::new(0x8000),
    )
    .unwrap();
    assert!(r.read().0.is_empty(), "held before its frame");
    blocks(&mut r.s, 20);
    assert_eq!(notes(r.reader.update().0), [67]);
}

/// **A reader whose instrument is removed keeps what it last read**: the node
/// feeds the tap no more, so the set stays as it was and reading it loses
/// nothing.
#[test]
fn a_removed_instrument_leaves_the_set_as_it_was() {
    let mut r = rig(64);
    r.send(NOTE_ON_60);
    assert_eq!(notes(r.read().0), [60]);
    r.s.remove(r.instrument).unwrap();
    let _ = r.s.update();
    r.send([0x4090_4000, 0x8000_0000, 0, 0]);
    let (held, losses) = r.read();
    assert_eq!(notes(held), [60]);
    assert_eq!(losses.dropped(), 0);
}
