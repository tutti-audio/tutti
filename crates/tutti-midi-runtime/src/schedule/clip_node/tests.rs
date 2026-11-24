use super::*;

use std::sync::Arc;

use tutti_fixtures::midi;
use tutti_ump::{CCNumber, ControlValue, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7};
use tutti_units::Beat;

/// **A clip a block still held when a publish replaced it is freed by
/// the next `reclaim`**, with no further publish.
#[test]
fn reclaim_frees_a_clip_a_block_held_across_its_replacement() {
    let on = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    let mut node = MidiClipNode::new([TimedMidiEvent::new(Beat::new(1.0), on).unwrap()]);
    let controls = MidiClipControls {
        cell: node.cell.clone(),
        serial: Arc::default(),
    };
    let gone = Arc::downgrade(&node.cell.current());
    {
        // A block in progress across the replacement.
        let _held = node.clip.lend();
        controls.set(ClipEvents::default());
    }
    assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
    assert_eq!(tutti_rt::Reclaim::reclaim(&controls), 0);
    assert!(gone.upgrade().is_none(), "freed by the reclaim");
}

/// **A clip node's held-note table is 16 KiB**, allocated with the node.
#[test]
fn a_clip_nodes_table_is_16_kib() {
    let node = MidiClipNode::new([]);
    assert_eq!(node.play.held.heap_bytes(), 2_048 * 8);
}

/// **A clip is translated as it is loaded, in beat order, and keeps its list
/// order**. An RPN 0 run listed backwards (its CC 38 first,
/// its selects last) is assembled in beat order into one Registered
/// Controller at the CC 38's beat; a MIDI 1.0 Note On at velocity 0 becomes a
/// Note Off (M2-104 D.3.1); and the events keep the list's order, which the
/// note pairing reads, not the beats'.
#[test]
fn a_clip_is_translated_in_beat_order_and_keeps_its_list_order() {
    let note = NoteNumber::of(60);
    let on0 = Ump::from(Message::midi1(
        MidiChannel::FIRST,
        Voice1::note_on(note, U7::of(0)),
    ));
    let events = [
        TimedMidiEvent::new(Beat::new(3.0), on0).unwrap(),
        TimedMidiEvent::new(
            Beat::new(2.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::DATA_ENTRY_LSB, U7::of(50)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat::new(1.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::DATA_ENTRY, U7::of(12)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat::new(0.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::RPN_MSB, U7::of(0)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat::new(0.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::RPN_LSB, U7::of(0)),
            ),
        )
        .unwrap(),
    ];
    let out = translated(events);
    let decoded: Vec<(f64, tutti_ump::Message)> = out
        .iter()
        .map(|e| (e.beat().get(), e.event().decode().unwrap()))
        .collect();
    let address = tutti_ump::Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::FIRST,
    };
    assert_eq!(
        decoded,
        vec![
            (
                3.0,
                tutti_ump::Message::Midi2(
                    address,
                    tutti_ump::Voice2::NoteOff {
                        note,
                        velocity: NoteVelocity::CENTER,
                        attribute: tutti_ump::Attribute::None,
                    }
                )
            ),
            (
                2.0,
                tutti_ump::Message::Midi2(
                    address,
                    tutti_ump::Voice2::Registered {
                        bank: tutti_ump::U7::of(0),
                        index: tutti_ump::U7::of(0),
                        // Pitch bend sensitivity 12 semitones 50 cents,
                        // zero-extended (M2-115 section 4.1).
                        value: ControlValue::new(((12 << 7) | 50) << 18),
                    }
                )
            ),
        ]
    );
}

fn note_event(beat: f64, on: bool, key: u8, velocity: u16) -> TimedMidiEvent {
    let note = NoteNumber::of(key);
    let voice = if on {
        tutti_ump::Voice2::note_on(note, NoteVelocity::new(velocity))
    } else {
        tutti_ump::Voice2::note_off(note, NoteVelocity::new(velocity))
    };
    TimedMidiEvent::new(
        Beat::new(beat),
        Message::midi2(
            tutti_ump::Address {
                group: MidiGroup::FIRST,
                channel: MidiChannel::of(3),
            },
            voice,
        ),
    )
    .unwrap()
}

/// **A clip's notes come back in the order of their note-ons, each with its
/// start, its length (note-off beat minus note-on beat), key, MIDI 2.0
/// velocity, channel and group**; a note-on no note-off ends has no length.
#[test]
fn a_clips_notes_are_listed_by_note_on_with_their_lengths_and_open_ones_have_none() {
    let clip: ClipEvents = [
        note_event(2.0, true, 64, 0x4000),
        note_event(0.0, true, 60, 0x8000),
        note_event(1.5, false, 60, 0),
        note_event(3.0, false, 64, 0),
        note_event(4.0, true, 67, 0xFFFF),
    ]
    .into_iter()
    .collect();
    let (group, channel) = (MidiGroup::FIRST, MidiChannel::of(3));
    let note = |key, velocity, start, length| ClipNote {
        group,
        channel,
        note: NoteNumber::of(key),
        velocity: NoteVelocity::new(velocity),
        start: Beat::new(start),
        length,
    };
    assert_eq!(
        clip.notes().collect::<Vec<_>>(),
        [
            note(60, 0x8000, 0.0, Some(BeatDuration::new(1.5))),
            note(64, 0x4000, 2.0, Some(BeatDuration::new(1.0))),
            note(67, 0xFFFF, 4.0, None),
        ]
    );
}

/// **A MIDI 1.0 note is listed at its MIDI 2.0 velocity**: the clip stores
/// the translation, so velocity 127 is the full 16 bits.
#[test]
fn a_midi_1_notes_velocity_is_listed_at_midi_2_width() {
    let on = Message::midi1(
        MidiChannel::FIRST,
        Voice1::note_on(NoteNumber::of(60), U7::of(127)),
    );
    let clip: ClipEvents = [TimedMidiEvent::new(Beat::new(1.0), on).unwrap()]
        .into_iter()
        .collect();
    let notes: Vec<_> = clip.notes().collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].velocity, NoteVelocity::new(0xFFFF));
}

/// **A note-off that ends no note-on is not a note**, and a clip with no
/// notes lists none.
#[test]
fn a_note_off_with_no_note_on_is_not_a_note() {
    let clip: ClipEvents = [note_event(1.0, false, 60, 0)].into_iter().collect();
    assert_eq!(clip.len(), 1);
    assert_eq!(clip.notes().count(), 0);
    assert_eq!(ClipEvents::default().notes().count(), 0);
}

/// **Two overlapping notes of one key pair first on, first off**, each with
/// its own length.
#[test]
fn overlapping_notes_of_one_key_pair_first_on_first_off() {
    let clip: ClipEvents = [
        note_event(0.0, true, 60, 1),
        note_event(1.0, true, 60, 2),
        note_event(2.0, false, 60, 0),
        note_event(4.0, false, 60, 0),
    ]
    .into_iter()
    .collect();
    let lengths: Vec<_> = clip.notes().map(|n| n.length).collect();
    assert_eq!(
        lengths,
        [Some(BeatDuration::new(2.0)), Some(BeatDuration::new(3.0))]
    );
}
