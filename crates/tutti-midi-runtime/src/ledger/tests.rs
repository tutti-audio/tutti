use super::*;
use tutti_fixtures::midi;
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber};

/// **The off the ledger writes is the MIDI 2.0 Note Off at velocity 0 that
/// `note_off` builds**, on the key's group, channel and note.
#[test]
fn an_owed_off_is_the_note_off_the_wire_types_build() {
    let address = tutti_ump::Address::new(MidiGroup::of(3), MidiChannel::of(9));
    let key = NoteKey::new(address, NoteNumber::of(61));
    let off = owed_off(key, Offset::ZERO);
    let want = midi::note_off(address, 61, 0);
    assert_eq!(off, Event::midi(Offset::ZERO, want));
}

/// **A table entry is 8 bytes for the clip's tag and 6 for none**, so
/// 2,048 keys hold 16 KiB and a relay's 128 hold under 1 KiB.
#[test]
fn the_table_costs_a_few_bytes_a_key() {
    assert_eq!(size_of::<Entry<std::num::NonZeroU32>>(), 8);
    assert_eq!(size_of::<Entry<()>>(), 6);
    assert_eq!(NoteLedger::<()>::new(128).heap_bytes(), 768);
}

/// **A note is one key whichever protocol carried it**: a MIDI 1.0 Note On and a
/// MIDI 2.0 Note Off of one Group, Channel and Note Number name the same key,
/// and another channel's note does not (M2-104 section 7.4.2 ends a note by
/// its Note Number on a Channel).
#[test]
fn a_midi_1_on_and_a_midi_2_off_of_one_note_are_one_key() {
    let address = tutti_ump::Address::new(MidiGroup::of(2), MidiChannel::of(5));
    let on = Class::of(midi::m1_note_on(address, 60, tutti_ump::U7::of(100)));
    let off = Class::of(midi::note_off(address, 60, 0));
    let (Class::On(on), Class::Off(off)) = (on, off) else {
        panic!("an on and an off");
    };
    assert_eq!(on, off);
    let elsewhere = tutti_ump::Address::new(MidiGroup::of(2), MidiChannel::of(6));
    let Class::Off(other) = Class::of(midi::note_off(elsewhere, 60, 0)) else {
        panic!("an off");
    };
    assert_ne!(on, other);
}
