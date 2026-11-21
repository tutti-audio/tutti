//! The MIDI note number encoding of a `Note`: number 0 is C-1 and 69 is A4.

use tutti_music::{Note, PitchClass};
use tutti_ump::{NotOnMidiScale, NoteNumber};

#[test]
fn note_number_69_is_a4_and_0_is_c_minus_1_and_127_is_g9() {
    assert_eq!(Note::from(NoteNumber::of(69)), Note::A4);
    assert_eq!(Note::from(NoteNumber::of(60)), Note::C4);
    assert_eq!(Note::from(NoteNumber::of(0)), Note::new(PitchClass::C, -1));
    assert_eq!(Note::from(NoteNumber::of(127)), Note::new(PitchClass::G, 9));
}

#[test]
fn every_note_number_round_trips_through_a_note() {
    for raw in 0u8..=127 {
        let number = NoteNumber::of(raw);
        assert_eq!(NoteNumber::try_from(Note::from(number)), Ok(number));
    }
}

#[test]
fn a_note_above_g9_or_below_c_minus_1_is_not_on_the_midi_scale() {
    let above = Note::new(PitchClass::GSharp, 9);
    assert_eq!(NoteNumber::try_from(above), Err(NotOnMidiScale(above)));
    let a9 = Note::new(PitchClass::A, 9);
    assert_eq!(NoteNumber::try_from(a9), Err(NotOnMidiScale(a9)));
    let below = Note::new(PitchClass::B, -2);
    assert_eq!(NoteNumber::try_from(below), Err(NotOnMidiScale(below)));
}
