//! Song Position Pointer and timecode as values of time (M1 pp.30 and 53).

use tutti_midi_protocol::{SmpteFrameRate, SmpteTimecode, song_position};
use tutti_ump::U14;
use tutti_units::{Beat, Seconds};

/// M1 p.30: a Song Position Pointer counts MIDI beats, each a sixteenth note,
/// so a quarter note is 4 and beat 2.0 is 8; it rounds to the nearest.
#[test]
fn a_song_position_counts_sixteenths_rounded_to_the_nearest() {
    assert_eq!(song_position(Beat::new(0.0)).get(), 0);
    assert_eq!(song_position(Beat::new(2.0)).get(), 8);
    assert_eq!(song_position(Beat::new(0.37)).get(), 1);
    assert_eq!(song_position(Beat::new(0.38)).get(), 2);
}

/// The pointer is 14 bits: a position before the start or NaN is the start,
/// and one past 16383 sixteenths sticks at the largest.
#[test]
fn a_song_position_outside_the_14_bit_field_sticks_at_its_ends() {
    assert_eq!(song_position(Beat::new(-3.0)).get(), 0);
    assert_eq!(song_position(Beat::new(f64::NAN)).get(), 0);
    assert_eq!(song_position(Beat::new(4096.0)), U14::MAX);
    assert_eq!(song_position(Beat::new(1.0e9)), U14::MAX);
}

/// 30 fps counts 30 frames a second, so 3661.5 s is `01:01:01:15`; 25 fps
/// counts 25, so the same time is frame 12.5 of the second, which rounds up to
/// `01:01:01:13`.
#[test]
fn a_time_is_the_label_of_the_nearest_frame() {
    let at = |s, rate| SmpteTimecode::from_seconds(Seconds::new(s), rate).to_string();
    assert_eq!(at(3661.5, SmpteFrameRate::Fps30), "01:01:01:15");
    assert_eq!(at(3661.5, SmpteFrameRate::Fps25), "01:01:01:13");
    assert_eq!(at(0.5 / 24.0 + 1e-9, SmpteFrameRate::Fps24), "00:00:00:01");
    assert_eq!(at(0.5 / 24.0 - 1e-9, SmpteFrameRate::Fps24), "00:00:00:00");
}

/// A time a float's width short of a frame boundary is that frame, a negative
/// or NaN time is the origin, and one past 24 hours wraps its hours.
#[test]
fn a_time_just_short_of_a_frame_rounds_up_and_out_of_range_times_are_bounded() {
    let rate = SmpteFrameRate::Fps30;
    let at = |s: f64| SmpteTimecode::from_seconds(Seconds::new(s), rate);
    assert_eq!(at(1.0 - 1e-12).to_string(), "00:00:01:00");
    assert_eq!(at(-5.0).to_string(), "00:00:00:00");
    assert_eq!(at(f64::NAN).to_string(), "00:00:00:00");
    assert_eq!(at(86_400.0 + 1.0).to_string(), "00:00:01:00");
}

/// Drop-frame 29.97 labels skip `;00` and `;01` at a minute not divisible by
/// ten: frame 1800 is the label `00:01:00:02`.
#[test]
fn a_time_at_drop_frame_is_labelled_by_the_dropped_count() {
    let tc = SmpteTimecode::from_seconds(
        Seconds::new(1800.0 * 1001.0 / 30_000.0),
        SmpteFrameRate::Fps2997Df,
    );
    assert_eq!(tc.to_string(), "00:01:00:02");
    assert_eq!(SmpteFrameRate::Fps25.frames_at(Seconds::new(2.0)), 50.0);
}
