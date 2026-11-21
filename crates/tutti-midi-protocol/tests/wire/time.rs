//! Song Position Pointer and timecode as values of time (M1 pp.30 and 53).

use tutti_midi_protocol::{SmpteFrameRate, SmpteTimecode, song_position};
use tutti_ump::U14;
use tutti_units::{Beat, Seconds};

/// M1 p.30: a Song Position Pointer counts MIDI beats, each a sixteenth note,
/// so a quarter note is 4 and beat 2.0 is 8; it rounds to the nearest.
#[test]
fn a_song_position_counts_sixteenths_rounded_to_the_nearest() {
    let at = |beat: f64| song_position(Beat::new(beat)).map(U14::get);
    assert_eq!(at(0.0), Some(0));
    assert_eq!(at(2.0), Some(8));
    assert_eq!(at(0.37), Some(1));
    assert_eq!(at(0.38), Some(2));
}

/// The pointer is 14 bits, 0 to 16383 sixteenths: the first and the last fit.
#[test]
fn a_song_position_is_some_at_both_edges_of_the_field() {
    assert_eq!(song_position(Beat::new(0.0)), Some(U14::of(0)));
    assert_eq!(song_position(Beat::new(16_383.0 / 4.0)), Some(U14::MAX));
    assert_eq!(U14::MAX.get(), 16_383);
}

/// A beat the 14-bit field cannot hold has no pointer: past the last
/// sixteenth, before the start, NaN and infinite, none clamped.
#[test]
fn a_song_position_is_none_past_the_field_before_zero_and_for_nan() {
    assert_eq!(song_position(Beat::new(4096.0)), None);
    assert_eq!(song_position(Beat::new(16_384.0 / 4.0 - 0.01)), None);
    assert_eq!(song_position(Beat::new(1.0e9)), None);
    assert_eq!(song_position(Beat::new(-3.0)), None);
    assert_eq!(song_position(Beat::new(-0.01)), None);
    assert_eq!(song_position(Beat::new(f64::NAN)), None);
    assert_eq!(song_position(Beat::new(f64::INFINITY)), None);
    assert_eq!(song_position(Beat::new(f64::NEG_INFINITY)), None);
}

/// 30 fps counts 30 frames a second, so 3661.5 s is `01:01:01:15`; 25 fps
/// counts 25, so the same time is frame 12.5 of the second, which rounds up to
/// `01:01:01:13`.
#[test]
fn a_time_is_the_label_of_the_nearest_frame() {
    let at = |s, rate| {
        SmpteTimecode::from_seconds(Seconds::new(s), rate)
            .expect("a time of day")
            .to_string()
    };
    assert_eq!(at(3661.5, SmpteFrameRate::Fps30), "01:01:01:15");
    assert_eq!(at(3661.5, SmpteFrameRate::Fps25), "01:01:01:13");
    assert_eq!(at(0.5 / 24.0 + 1e-9, SmpteFrameRate::Fps24), "00:00:00:01");
    assert_eq!(at(0.5 / 24.0 - 1e-9, SmpteFrameRate::Fps24), "00:00:00:00");
}

/// A time a float's width short of a frame boundary is that frame.
#[test]
fn a_time_just_short_of_a_frame_rounds_up_to_it() {
    let at = |s: f64| SmpteTimecode::from_seconds(Seconds::new(s), SmpteFrameRate::Fps30);
    assert_eq!(at(1.0 - 1e-12).unwrap().to_string(), "00:00:01:00");
}

/// A time no label names has none: before zero, NaN, infinite, and 24 hours
/// or more; the last frame of the day is `Some`.
#[test]
fn a_time_is_none_before_zero_for_nan_and_from_24_hours() {
    for rate in [
        SmpteFrameRate::Fps24,
        SmpteFrameRate::Fps25,
        SmpteFrameRate::Fps2997Df,
        SmpteFrameRate::Fps2997Ndf,
        SmpteFrameRate::Fps30,
    ] {
        let at = |s: f64| SmpteTimecode::from_seconds(Seconds::new(s), rate);
        assert_eq!(at(-5.0), None, "{rate:?}");
        assert_eq!(at(-1e-12), None, "{rate:?}");
        assert_eq!(at(f64::NAN), None, "{rate:?}");
        assert_eq!(at(f64::INFINITY), None, "{rate:?}");
        assert_eq!(at(86_400.0), None, "{rate:?}");
        assert_eq!(at(86_400.0 + 1.0), None, "{rate:?}");
        // Under half a frame short of 24 h rounds up to the 24th hour. (Non-drop
        // 29.97 counts its labels at face value: its 24th hour is later.)
        if rate != SmpteFrameRate::Fps2997Ndf {
            assert_eq!(at(86_400.0 - 1e-9), None, "{rate:?}");
        }
        assert!(at(0.0).is_some(), "{rate:?}");
        assert!(at(86_399.0).is_some(), "{rate:?}");
    }
    let last = SmpteTimecode::from_seconds(Seconds::new(86_399.9), SmpteFrameRate::Fps30);
    assert_eq!(last.unwrap().to_string(), "23:59:59:27");
}

/// MIDI Time Code's hours count 0 to 23 and start again (M1 p.53): the
/// wrapping form carries a time past a day, and still refuses what is no time.
#[test]
fn a_wrapping_time_carries_hours_past_24_and_refuses_before_zero_and_nan() {
    let rate = SmpteFrameRate::Fps30;
    let at = |s: f64| SmpteTimecode::from_seconds_wrapping(Seconds::new(s), rate);
    assert_eq!(at(86_400.0 + 1.0).unwrap().to_string(), "00:00:01:00");
    assert_eq!(at(86_400.0 - 1e-9).unwrap().to_string(), "00:00:00:00");
    assert_eq!(at(-1.0), None);
    assert_eq!(at(f64::NAN), None);
    assert_eq!(at(f64::INFINITY), None);
}

/// Drop-frame 29.97 labels skip `;00` and `;01` at a minute not divisible by
/// ten: frame 1800 is the label `00:01:00:02`.
#[test]
fn a_time_at_drop_frame_is_labelled_by_the_dropped_count() {
    let tc = SmpteTimecode::from_seconds(
        Seconds::new(1800.0 * 1001.0 / 30_000.0),
        SmpteFrameRate::Fps2997Df,
    )
    .expect("a time of day");
    assert_eq!(tc.to_string(), "00:01:00:02");
    assert_eq!(SmpteFrameRate::Fps25.frames_at(Seconds::new(2.0)), 50.0);
}
