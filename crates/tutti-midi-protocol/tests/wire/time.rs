//! Song Position Pointer and timecode as values of time (M1 pp.30 and 53).

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_midi_protocol::{
    SmpteFrameRate, SmpteTimecode, TimecodeHours, TimecodeMinutes, TimecodeSeconds, song_position,
};
use tutti_ump::U14;
use tutti_units::{Beat, Seconds, TimecodeFrames};

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

/// A time no label names has none: before zero, NaN and infinite, and past the
/// last label of the day, each rate at its own edge. The label count of a day
/// is 24, 25 or 30 labels a second for 86 400 s; drop-frame 29.97 skips two
/// labels a minute but ten, so its day is 2 589 408 frames; non-drop 29.97
/// counts 30 labels a second at 30 000/1001 frames a second, so its labels
/// last 2 592 000 frames, to 86 486.4 s.
#[test]
fn a_time_is_none_before_zero_for_nan_and_past_the_last_label_of_the_day() {
    for (rate, frames_in_day, last) in [
        (SmpteFrameRate::Fps24, 24 * 86_400u64, "23:59:59:23"),
        (SmpteFrameRate::Fps25, 25 * 86_400, "23:59:59:24"),
        (SmpteFrameRate::Fps30, 30 * 86_400, "23:59:59:29"),
        (SmpteFrameRate::Fps2997Df, 2_589_408, "23:59:59:29"),
        (SmpteFrameRate::Fps2997Ndf, 2_592_000, "23:59:59:29"),
    ] {
        let at = |s: f64| SmpteTimecode::from_seconds(Seconds::new(s), rate);
        let (num, den) = rate.ratio();
        let frame = |n: u64| n as f64 * den as f64 / num as f64;
        assert_eq!(at(-5.0), None, "{rate:?}");
        assert_eq!(at(-1e-12), None, "{rate:?}");
        assert_eq!(at(f64::NAN), None, "{rate:?}");
        assert_eq!(at(f64::INFINITY), None, "{rate:?}");
        assert!(at(0.0).is_some(), "{rate:?}");
        assert_eq!(
            at(frame(frames_in_day - 1)).unwrap().to_string(),
            last,
            "{rate:?}"
        );
        assert_eq!(at(frame(frames_in_day)), None, "{rate:?}");
        assert_eq!(at(frame(frames_in_day) + 1.0), None, "{rate:?}");
    }
    // Non-drop 29.97 still names 24 hours of wall clock: its label for 86 400 s.
    let ndf = SmpteFrameRate::Fps2997Ndf;
    assert!(SmpteTimecode::from_seconds(Seconds::new(86_400.0), ndf).is_some());
    assert_eq!(
        SmpteTimecode::from_seconds(Seconds::new(86_486.4), ndf),
        None
    );
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
    // The wrapping form agrees with `from_seconds` on every time `from_seconds`
    // names, at the day's last label of non-drop 29.97 included.
    let ndf = SmpteFrameRate::Fps2997Ndf;
    for s in [0.0, 3_661.5, 86_399.0, 86_400.0, 86_486.3] {
        assert_eq!(
            SmpteTimecode::from_seconds_wrapping(Seconds::new(s), ndf),
            SmpteTimecode::from_seconds(Seconds::new(s), ndf),
            "{s}"
        );
    }
    let past = SmpteTimecode::from_seconds_wrapping(Seconds::new(86_486.4), ndf);
    assert_eq!(past.unwrap().to_string(), "00:00:00:00");
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
    assert_eq!(
        SmpteFrameRate::Fps25.frames_at(Seconds::new(2.0)),
        TimecodeFrames::new(50.0)
    );
}

/// A frame lasts `den / num` seconds over the exact ratio, and `frames_at` counts
/// those periods: 1001/30000 s for both 29.97 rates, 1/24 s at film rate.
#[test]
fn a_frame_period_is_the_exact_ratio_inverted() {
    for rate in SmpteFrameRate::ALL {
        let (num, den) = rate.ratio();
        assert_eq!(rate.frame_period(), Seconds::new(den as f64 / num as f64));
        assert_eq!(rate.frames_at(rate.frame_period()).round(), 1, "{rate:?}");
    }
    assert_eq!(
        SmpteFrameRate::Fps2997Df.frame_period(),
        Seconds::new(1001.0 / 30_000.0)
    );
    assert_eq!(SmpteFrameRate::Fps24.frame_period().get(), 1.0 / 24.0);
}

/// The hours of a timecode are `0..=23`, so a clock's 24th hour is unwritable.
#[test]
fn timecode_hours_keep_the_range_contract() {
    check_range_contract(&RangeContract {
        new: TimecodeHours::new,
        get: TimecodeHours::get,
        of: Some(TimecodeHours::of),
        valid: &[0, 1, 22, 23],
        invalid: &[24, 31, 255],
    });
}

/// The minutes of a timecode are `0..=59`.
#[test]
fn timecode_minutes_keep_the_range_contract() {
    check_range_contract(&RangeContract {
        new: TimecodeMinutes::new,
        get: TimecodeMinutes::get,
        of: Some(TimecodeMinutes::of),
        valid: &[0, 1, 58, 59],
        invalid: &[60, 63, 255],
    });
}

/// The seconds of a timecode are `0..=59`.
#[test]
fn timecode_seconds_keep_the_range_contract() {
    check_range_contract(&RangeContract {
        new: TimecodeSeconds::new,
        get: TimecodeSeconds::get,
        of: Some(TimecodeSeconds::of),
        valid: &[0, 1, 58, 59],
        invalid: &[60, 63, 255],
    });
}
