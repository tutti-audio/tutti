//! `TempoMap::stepped_to` against hand-computed beats and seconds.

use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_units::{Beat, Bpm};

fn bpm(x: f64) -> Bpm {
    Bpm::of(x)
}

/// Tempo linear in time, 60 to 180 over beats 0..8: with `k` bpm per second,
/// 180 = 60 + kT and beats = T + kT^2 / 120 = 2T = 8 give T = 4 s, k = 30.
/// Beat 3 is then at t with t + t^2 / 4 = 3, t = 2 s, where the tempo is 120.
fn ramp_60_to_180() -> TempoMap {
    TempoMap::builder(bpm(60.0))
        .ramp_time_to(Beat::new(8.0), bpm(180.0))
        .step_to(Beat::new(16.0), bpm(90.0))
        .build()
        .unwrap()
}

/// A step on a constant 120 at beat 3 is 1.5 s in and keeps every earlier
/// time; beat 5 is then 2 beats at 90 bpm (2/3 s each beat) later, 1.5 + 4/3 s.
#[test]
fn a_step_on_a_constant_map_is_one_point() {
    let old = TempoMap::constant(bpm(120.0)).unwrap();
    let new = old.stepped_to(Beat::new(3.0), bpm(90.0)).unwrap().unwrap();
    assert_eq!(
        new.points(),
        [
            TempoPoint::step(Beat::new(0.0), bpm(120.0)),
            TempoPoint::step(Beat::new(3.0), bpm(90.0)),
        ]
    );
    assert_eq!(new.seconds_at(Beat::new(3.0)).get(), 1.5);
    assert!((new.seconds_at(Beat::new(5.0)).get() - (1.5 + 4.0 / 3.0)).abs() < 1e-12);
}

/// A point after the step is dropped: the map ends on the step.
#[test]
fn a_step_drops_the_points_after_it() {
    let old = TempoMap::builder(bpm(100.0))
        .step_to(Beat::new(4.0), bpm(110.0))
        .step_to(Beat::new(8.0), bpm(120.0))
        .build()
        .unwrap();
    let new = old.stepped_to(Beat::new(5.0), bpm(70.0)).unwrap().unwrap();
    let beats: Vec<f64> = new.points().iter().map(|p| p.beat.get()).collect();
    assert_eq!(beats, [0.0, 4.0, 5.0]);
    assert_eq!(new.tempo_at(Beat::new(100.0)), bpm(70.0));
}

/// A step inside the ramp cuts it at beat 3 (120 bpm, 2 s): the ramp is kept
/// to there and ends at 120, the step is at that same beat with no sliver
/// between, and the ramp's own end and the point after it are gone.
#[test]
fn a_ramp_a_step_lands_in_is_cut_at_its_beat() {
    let at = Beat::new(3.0);
    let new = ramp_60_to_180()
        .stepped_to(at, bpm(100.0))
        .unwrap()
        .unwrap();
    assert_eq!(
        new.points(),
        [
            TempoPoint {
                transition: Transition::RampTime {
                    end: Some(bpm(120.0))
                },
                ..TempoPoint::step(Beat::new(0.0), bpm(60.0))
            },
            TempoPoint::step(at, bpm(100.0)),
        ]
    );
    let below = new.tempo_at(Beat::new(at.get().next_down())).get();
    assert!((below - 120.0).abs() < 1e-9, "the ramp's own: {below}");
    assert_eq!(new.tempo_at(at), bpm(100.0));
    assert!((new.seconds_at(at).get() - 2.0).abs() < 1e-12);
    // 2 beats at 100 bpm, 0.6 s each.
    assert!((new.seconds_at(Beat::new(5.0)).get() - 3.2).abs() < 1e-9);
}

/// A step at the end of the ramp (beat 8, a point of the map) keeps the whole
/// ramp: it reaches 180 at beat 8 and the step is at that beat.
#[test]
fn a_step_at_a_ramps_end_keeps_the_ramp() {
    let new = ramp_60_to_180()
        .stepped_to(Beat::new(8.0), bpm(100.0))
        .unwrap()
        .unwrap();
    assert_eq!(
        new.points(),
        [
            TempoPoint {
                transition: Transition::RampTime {
                    end: Some(bpm(180.0))
                },
                ..TempoPoint::step(Beat::new(0.0), bpm(60.0))
            },
            TempoPoint::step(Beat::new(8.0), bpm(100.0)),
        ]
    );
    assert!((new.seconds_at(Beat::new(8.0)).get() - 4.0).abs() < 1e-12);
}

/// A step at a later step point replaces that point; the earlier ramp stays.
#[test]
fn a_step_at_a_step_point_replaces_it() {
    let new = ramp_60_to_180()
        .stepped_to(Beat::new(16.0), bpm(75.0))
        .unwrap()
        .unwrap();
    assert_eq!(
        new.points(),
        [
            TempoPoint::ramp_time(Beat::new(0.0), bpm(60.0)),
            TempoPoint::step(Beat::new(8.0), bpm(180.0)),
            TempoPoint::step(Beat::new(16.0), bpm(75.0)),
        ]
    );
}

/// A ramp linear in beats is cut with its kind kept: 60 to 180 over 8 beats,
/// tempo(5) = 60 + 15 * 5 = 135.
#[test]
fn a_beat_linear_ramp_is_cut_too() {
    let old = TempoMap::builder(bpm(60.0))
        .ramp_beats_to(Beat::new(8.0), bpm(180.0))
        .build()
        .unwrap();
    let new = old.stepped_to(Beat::new(5.0), bpm(100.0)).unwrap().unwrap();
    assert_eq!(
        new.points()[0].transition,
        Transition::RampBeats {
            end: Some(bpm(135.0))
        }
    );
    assert_eq!(
        new.points()[1],
        TempoPoint::step(Beat::new(5.0), bpm(100.0))
    );
    assert_eq!(new.points().len(), 2);
    let below = new.tempo_at(Beat::new(5.0_f64.next_down())).get();
    assert!((below - 135.0).abs() < 1e-9, "the ramp's own: {below}");
    assert_eq!(new.tempo_at(Beat::new(5.0)), bpm(100.0));
}

/// A step at beat 0, or before it, replaces the first tempo.
#[test]
fn a_step_at_the_origin_replaces_the_first_tempo() {
    for at in [Beat::new(0.0), Beat::new(-2.0)] {
        let new = ramp_60_to_180().stepped_to(at, bpm(75.0)).unwrap().unwrap();
        assert_eq!(new.points(), [TempoPoint::step(Beat::new(0.0), bpm(75.0))]);
    }
}

/// The tempo already in force from the beat on, with nothing after, is no
/// change; with a point after it the step truncates, so it is one.
#[test]
fn a_repeated_tempo_is_no_change() {
    let old = TempoMap::constant(bpm(120.0)).unwrap();
    assert!(
        old.stepped_to(Beat::new(4.0), bpm(120.0))
            .unwrap()
            .is_none()
    );
    let tail = TempoMap::builder(bpm(120.0))
        .step_to(Beat::new(8.0), bpm(90.0))
        .build()
        .unwrap();
    assert!(
        tail.stepped_to(Beat::new(4.0), bpm(120.0))
            .unwrap()
            .is_some()
    );
}

/// A tempo whose segment duration overflows an `f64` is refused.
#[test]
fn a_tempo_the_map_cannot_hold_is_refused() {
    let old = TempoMap::constant(bpm(120.0)).unwrap();
    let slowest = Bpm::new(f64::from_bits(1)).expect("positive and finite");
    assert!(old.stepped_to(Beat::new(4.0), slowest).is_err());
}

/// `transition_to` is the three named steps chosen by value.
#[test]
fn transition_to_is_the_named_call_for_its_kind() {
    for (kind, named) in [
        (
            Transition::Step,
            TempoMap::builder(bpm(60.0)).step_to(Beat::new(4.0), bpm(90.0)),
        ),
        (
            Transition::RampTime { end: None },
            TempoMap::builder(bpm(60.0)).ramp_time_to(Beat::new(4.0), bpm(90.0)),
        ),
        (
            Transition::RampBeats { end: None },
            TempoMap::builder(bpm(60.0)).ramp_beats_to(Beat::new(4.0), bpm(90.0)),
        ),
    ] {
        let by_value = TempoMap::builder(bpm(60.0)).transition_to(kind, Beat::new(4.0), bpm(90.0));
        assert_eq!(
            by_value.build().unwrap(),
            named.build().unwrap(),
            "{kind:?}"
        );
    }
}
