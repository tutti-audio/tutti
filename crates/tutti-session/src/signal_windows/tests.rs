use super::*;
use tutti_tempo::TempoMap;
use tutti_units::{Beat, BeatDuration, Bpm, SampleRate};

const SECOND: usize = 48_000;

fn region(start: usize, end: usize) -> Region {
    Region::new(Samples::new(start), Samples::new(end)).expect("a region ends after it starts")
}

/// At 120 BPM a second is two beats, so signal over seconds 1..3 and 4..5 is
/// a window at beat 2 for 4 beats and one at beat 8 for 2.
#[test]
fn each_signal_region_is_a_window_at_its_start_for_its_length_in_beats() {
    let map = TempoMap::constant(Bpm::of(120.0)).unwrap();
    let windows = windows_from_signal(
        &[region(SECOND, 3 * SECOND), region(4 * SECOND, 5 * SECOND)],
        &mut map.view(SampleRate::SR_48K),
    );
    assert_eq!(
        windows,
        [
            VoiceWindow::span(Beat::new(2.0), BeatDuration::new(4.0)),
            VoiceWindow::span(Beat::new(8.0), BeatDuration::new(2.0)),
        ]
    );
}

/// A scan with no signal is all silence, which no voice plays.
#[test]
fn no_signal_gives_no_windows() {
    let map = TempoMap::constant(Bpm::of(120.0)).unwrap();
    assert!(windows_from_signal(&[], &mut map.view(SampleRate::SR_48K)).is_empty());
}

/// Beats 0..1 last 0.5 s at 120 BPM and beats 1.. last 0.25 s at 240, so a
/// region over seconds 0.5..0.75 is one beat long, starting at beat 1.
#[test]
fn a_window_follows_the_tempo_map() {
    let map = TempoMap::builder(Bpm::of(120.0))
        .step_to(Beat::new(1.0), Bpm::of(240.0))
        .build()
        .unwrap();
    let windows = windows_from_signal(
        &[region(SECOND / 2, SECOND / 2 + SECOND / 4)],
        &mut map.view(SampleRate::SR_48K),
    );
    assert_eq!(
        windows,
        [VoiceWindow::span(Beat::new(1.0), BeatDuration::new(1.0))]
    );
}
