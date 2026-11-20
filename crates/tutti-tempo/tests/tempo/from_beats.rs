//! `TempoMap::from_beats` against the tempo each beat interval implies:
//! 60 * rate / interval BPM, and beat `n` on the `n`-th position.

use tutti_tempo::{FromBeatsError, TempoMap};
use tutti_units::{Beat, Bpm, Frame, SampleRate, Samples};

fn frames(positions: &[usize]) -> Vec<Samples> {
    positions.iter().copied().map(Samples::new).collect()
}

#[test]
fn a_beat_every_half_second_at_48k_is_120_bpm_with_beat_k_on_frame_24000k() {
    let beats = frames(&[0, 24_000, 48_000, 72_000, 96_000]);
    let map = TempoMap::from_beats(&beats, SampleRate::SR_48K).unwrap();
    let mut view = map.view(SampleRate::SR_48K);
    for k in 0..5 {
        assert_eq!(map.tempo_at(Beat::new(k as f64)), Bpm::of(120.0));
        assert_eq!(
            view.frame_at_or_after(Beat::new(k as f64)),
            Frame::new(24_000 * k)
        );
    }
}

/// Intervals of 24 000, 16 000 and 12 000 frames at 48 kHz are 120, 180 and
/// 240 BPM (60 * 48 000 / interval); the last interval's tempo holds after the
/// last beat. Beat 3 is at 52 000 frames.
#[test]
fn each_interval_is_a_segment_at_its_own_bpm() {
    let beats = frames(&[0, 24_000, 40_000, 52_000]);
    let map = TempoMap::from_beats(&beats, SampleRate::SR_48K).unwrap();
    assert_eq!(map.tempo_at(Beat::new(0.0)), Bpm::of(120.0));
    assert_eq!(map.tempo_at(Beat::new(1.0)), Bpm::of(180.0));
    assert_eq!(map.tempo_at(Beat::new(2.0)), Bpm::of(240.0));
    assert_eq!(map.tempo_at(Beat::new(10.0)), Bpm::of(240.0));
    let mut view = map.view(SampleRate::SR_48K);
    assert_eq!(view.frame_at_or_after(Beat::new(2.0)), Frame::new(40_000));
    assert_eq!(view.frame_at_or_after(Beat::new(3.0)), Frame::new(52_000));
}

/// Beat 0 is the first position, so a first beat off frame 0 is the map's
/// origin: the second beat is one interval later.
#[test]
fn beat_zero_is_the_first_position() {
    let beats = frames(&[1_000, 25_000]);
    let map = TempoMap::from_beats(&beats, SampleRate::SR_48K).unwrap();
    let mut view = map.view(SampleRate::SR_48K);
    assert_eq!(view.frame_at_or_after(Beat::new(0.0)), Frame::new(0));
    assert_eq!(view.frame_at_or_after(Beat::new(1.0)), Frame::new(24_000));
}

#[test]
fn fewer_than_two_beats_are_refused_with_their_count() {
    for n in 0..2 {
        let beats = frames(&[0][..n]);
        assert_eq!(
            TempoMap::from_beats(&beats, SampleRate::SR_48K),
            Err(FromBeatsError::TooFewBeats(n))
        );
    }
}

#[test]
fn a_repeated_or_earlier_beat_is_refused_at_its_index() {
    for beats in [frames(&[0, 100, 100]), frames(&[0, 100, 50])] {
        assert_eq!(
            TempoMap::from_beats(&beats, SampleRate::SR_48K),
            Err(FromBeatsError::NotIncreasing(2))
        );
    }
    assert_eq!(
        TempoMap::from_beats(&frames(&[5, 5]), SampleRate::SR_48K),
        Err(FromBeatsError::NotIncreasing(1))
    );
}
