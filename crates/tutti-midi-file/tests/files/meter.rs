//! The meter a file's Time Signature events declare (`meter_map`), read by
//! bar and beat: SMF 1.0 section 3.3 gives the numerator and the denominator as a
//! power of two.

use crate::common::{metrical, sequences};
use tutti_midi_file::MidiFile;
use tutti_music::{BeatsPerBar, Meter, TimeSignature};
use tutti_units::{BarNumber, Beat};

/// An SMF of `format` over 480 ticks a quarter, one MTrk per `events` list,
/// each ended by End of Track.
fn smf(format: u16, tracks: &[&[u8]]) -> MidiFile {
    let mut bytes = b"MThd\0\0\0\x06".to_vec();
    bytes.extend(format.to_be_bytes());
    bytes.extend((tracks.len() as u16).to_be_bytes());
    bytes.extend(480u16.to_be_bytes());
    for events in tracks {
        let body: Vec<u8> = events.iter().copied().chain([0, 0xFF, 0x2F, 0]).collect();
        bytes.extend(b"MTrk");
        bytes.extend((body.len() as u32).to_be_bytes());
        bytes.extend(body);
    }
    MidiFile::from_bytes(&bytes).expect("reads")
}

/// 3/4 at tick 0, then 6/8 from tick 1440 (beat 3, the downbeat of bar 2).
const TWO_METERS: &[u8] = &[
    0, 0xFF, 0x58, 4, 3, 2, 24, 8, // 3/4
    0x8B, 0x20, 0xFF, 0x58, 4, 6, 3, 24, 8, // delta 1440, 6/8
];

#[test]
fn a_time_signature_at_tick_zero_is_the_meter_from_beat_zero() {
    let file = smf(0, &[&[0, 0xFF, 0x58, 4, 3, 2, 24, 8]]);
    let map = metrical(&file).meter_map();
    let at = map.bar_at(Beat(4.0));
    assert_eq!(map.at(Beat(0.0)), TimeSignature::of(3, 4));
    // Bars of 3 quarters: beat 4 is the second beat of bar 2.
    assert_eq!((at.bar, at.beat), (BarNumber(2), BeatsPerBar::of(2)));
    assert!(metrical(&file).refused_time_signatures().is_empty());
}

#[test]
fn a_later_time_signature_changes_the_meter_at_its_beat() {
    let file = smf(0, &[TWO_METERS]);
    let map = metrical(&file).meter_map();
    assert_eq!(map.at(Beat(2.9)), TimeSignature::of(3, 4));
    assert_eq!(map.at(Beat(3.0)), TimeSignature::of(6, 8));
    assert_eq!(map.bar_start(BarNumber(2)), Beat(3.0));
    // Bars of 6 eighths are 3 quarters: beat 4.5 is the fourth eighth of bar 3.
    let at = map.bar_at(Beat(4.5));
    assert_eq!((at.bar, at.beat), (BarNumber(2), BeatsPerBar::of(4)));
}

#[test]
fn a_sequences_meter_is_its_own_tracks() {
    let file = smf(2, &[TWO_METERS, &[0, 0xFF, 0x58, 4, 5, 2, 24, 8]]);
    let view = sequences(&file);
    let meters: Vec<_> = view.sequences().map(|s| s.meter_map()).collect();
    assert_eq!(meters[0].at(Beat(3.0)), TimeSignature::of(6, 8));
    assert_eq!(meters[1].at(Beat(3.0)), TimeSignature::of(5, 4));
}

/// A numerator of 0 is no time signature: it is no change, and it is listed.
#[test]
fn a_time_signature_no_meter_holds_is_listed_and_makes_no_change() {
    let file = smf(0, &[&[0x87, 0x40, 0xFF, 0x58, 4, 0, 2, 24, 8]]);
    let view = metrical(&file);
    assert_eq!(view.meter_map().at(Beat(9.0)), TimeSignature::of(4, 4));
    let refused = view.refused_time_signatures();
    assert_eq!(refused.len(), 1);
    assert_eq!((refused[0].beat, refused[0].numerator), (Beat(2.0), 0));
}
