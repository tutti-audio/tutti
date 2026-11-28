//! The map against closed forms, an independent integral and long sessions.
//!
//! Every figure here comes from IEEE `+ - * / sqrt` and `libm`'s pure-Rust
//! `log1p`, `expm1` and `exp`, and a test that needs `ln` calls `libm::log`
//! too. None of it reaches the platform's libm, so the figures hold on any
//! target.

use crate::common::{R, Rng, pt};
use tutti_tempo::{TempoError, TempoMap, TempoPoint, Transition};
use tutti_units::{Beat, Bpm, Frame, Seconds};

/// 4 beats at 120 are 2.0 s, frame 96 000; a step reads back the tempo it was
/// given, not `bpm / 60 * 60`.
#[test]
fn step_is_beats_over_rate() {
    let m = TempoMap::constant(Bpm::of(120.0)).unwrap();
    assert_eq!(m.seconds_at(Beat::new(4.0)).get(), 2.0);
    assert_eq!(
        m.view(R).frame_at_or_after(Beat::new(4.0)),
        Frame::new(96_000)
    );
    assert_eq!(m.view(R).beat_at_frame(Frame::new(96_000)), Beat::new(4.0));
    assert_eq!(
        m.seconds_at(Beat::new(-2.0)).get(),
        -1.0,
        "before beat 0 at the first tempo"
    );
    assert_eq!(
        m.view(R).frame_at_or_after(Beat::new(-2.0)),
        Frame::new(0),
        "no frame before 0"
    );
    for bpm in [60.8, 61.3, 61.9, 133.3] {
        let m = TempoMap::constant(Bpm::of(bpm)).unwrap();
        assert_eq!(m.tempo_at(Beat::new(3.0)), Bpm::of(bpm));
        assert_eq!(m.tempo_at_seconds(Seconds::new(1.0)), Bpm::of(bpm));
    }
}

/// One beat at 120 BPM is 0.5 s, 24 000 frames at 48 kHz, and the distance is
/// signed. Across a step to 240 at beat 4, beats 2 to 6 are 2 beats at 0.5 s
/// plus 2 at 0.25 s, 1.5 s, 72 000 frames: the tempo is integrated over the
/// span, not read at an end (which would say 48 000 or 96 000).
#[test]
fn frames_between_integrates_the_tempo_over_the_span() {
    let m = TempoMap::constant(Bpm::of(120.0)).unwrap();
    assert_eq!(
        m.view(R).frames_between(Beat::new(0.0), Beat::new(1.0)),
        24_000
    );
    assert_eq!(
        m.view(R).frames_between(Beat::new(1.0), Beat::new(0.0)),
        -24_000
    );
    assert_eq!(m.view(R).frames_between(Beat::new(3.0), Beat::new(3.0)), 0);
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::Step),
        pt(4.0, 240.0, Transition::Step),
    ])
    .unwrap();
    assert_eq!(
        m.view(R).frames_between(Beat::new(2.0), Beat::new(6.0)),
        72_000
    );
}

/// A span within `FRAME_TOLERANCE` (1e-6 frame) past a whole frame is that
/// frame: 5e-7 frame past 1 lands on 1, where a bare ceiling would say 2. A
/// span 2e-6 frame past, beyond the tolerance, is the next frame.
#[test]
fn frames_between_lands_on_a_frame_within_the_tolerance() {
    let m = TempoMap::constant(Bpm::of(120.0)).unwrap();
    let within = Beat::new((1.0 + 5e-7) / 24_000.0);
    let beyond = Beat::new((1.0 + 2e-6) / 24_000.0);
    assert_eq!(m.view(R).frames_between(Beat::new(0.0), within), 1);
    assert_eq!(m.view(R).frames_between(Beat::new(0.0), beyond), 2);
}

/// 120 -> 180 over 4 beats, tempo linear in time. `T = 2*4/(2+3) = 1.6 s`
/// exactly = 76 800 frames; `a = (3-2)/1.6`; `beats(0.8 s) = 2*0.8 + a*0.32 = 1.8`.
#[test]
fn time_ramp_matches_closed_form() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::RampTime { end: None }),
        pt(4.0, 180.0, Transition::Step),
    ])
    .unwrap();
    assert_eq!(
        m.view(R).frame_at_or_after(Beat::new(4.0)),
        Frame::new(76_800)
    );
    assert!((m.seconds_at(Beat::new(4.0)).get() - 1.6).abs() < 1e-15);
    assert!((m.beat_at_seconds(Seconds::new(0.8)).get() - 1.8).abs() < 1e-15);
    assert!((m.tempo_at_seconds(Seconds::new(0.8)).get() - 150.0).abs() < 1e-12);
    assert!((m.tempo_at(Beat::new(1.8)).get() - 150.0).abs() < 1e-12);
    // After the ramp: 180 BPM = 3 beats/s.
    assert!((m.seconds_at(Beat::new(7.0)).get() - (1.6 + 1.0)).abs() < 1e-15);
    // Frame 38 400 is 0.8 s.
    assert!((m.view(R).beat_at_frame(Frame::new(38_400)).get() - 1.8).abs() < 1e-14);
}

/// 120 -> 180 over 4 beats, tempo linear in beats. `T = ln(1.5)/g`, `g = 0.25`
/// (beats/s per beat), so `T = 4 ln 1.5` s; the beat where the rate is 2.5
/// (beat 2) is `ln(1.25)/0.25` s in.
#[test]
fn beat_ramp_matches_closed_form() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::RampBeats { end: None }),
        pt(4.0, 180.0, Transition::Step),
    ])
    .unwrap();
    let t4 = 4.0 * libm::log(1.5);
    assert!(
        (m.seconds_at(Beat::new(4.0)).get() - t4).abs() < 1e-14,
        "{}",
        m.seconds_at(Beat::new(4.0)).get()
    );
    // 4 ln 1.5 s is 77 849.3 frames: the first frame at or after is 77 850.
    assert_eq!(
        m.view(R).frame_at_or_after(Beat::new(4.0)),
        Frame::new(77_850)
    );
    assert!((m.seconds_at(Beat::new(2.0)).get() - 4.0 * libm::log(1.25)).abs() < 1e-14);
    assert!((m.tempo_at(Beat::new(2.0)).get() - 150.0).abs() < 1e-12);
    assert!(
        (m.tempo_at_seconds(Seconds::new(4.0 * libm::log(1.25)))
            .get()
            - 150.0)
            .abs()
            < 1e-11
    );
    assert!((m.beat_at_seconds(Seconds::new(t4)).get() - 4.0).abs() < 1e-13);
}

/// Independent oracle: integrate `dt = db / r(b)` by Simpson using only
/// `tempo_at` (a different formula from `seconds_at`), over a mixed map.
#[test]
fn seconds_agree_with_numeric_integration() {
    let m = TempoMap::new([
        pt(0.0, 90.0, Transition::RampTime { end: None }),
        pt(8.0, 150.0, Transition::RampBeats { end: None }),
        pt(20.0, 70.0, Transition::Step),
        pt(30.0, 200.0, Transition::RampTime { end: None }),
        pt(31.0, 50.0, Transition::Step),
    ])
    .unwrap();
    let simpson = |a: f64, b: f64| {
        let n = 200_000;
        let h = (b - a) / n as f64;
        // The interval's end belongs to the next segment: read just inside it.
        let f = |x: f64| {
            60.0 / m
                .tempo_at(Beat::new(if x == b { b.next_down() } else { x }))
                .get()
        };
        let mut s = f(a) + f(b);
        for i in 1..n {
            s += f(a + i as f64 * h) * if i % 2 == 1 { 4.0 } else { 2.0 };
        }
        s * h / 3.0
    };
    let mut worst: f64 = 0.0;
    for (a, b) in [
        (0.0, 8.0),
        (8.0, 20.0),
        (20.0, 30.0),
        (30.0, 31.0),
        (31.0, 40.0),
    ] {
        let want = simpson(a, b);
        let got = m.seconds_at(Beat::new(b)).get() - m.seconds_at(Beat::new(a)).get();
        worst = worst.max((got - want).abs());
    }
    assert!(worst < 1e-10, "worst {worst:e} s");
}

/// A 5.5-hour session of 20 000 mixed points: beat to seconds to beat, and
/// frame to beat to frame, over 200 000 random queries.
#[test]
fn long_session_round_trip() {
    let mut rng = Rng::new(7);
    let mut pts = vec![];
    for i in 0..20_000 {
        let shape = match (rng.next() * 3.0) as u32 {
            0 => Transition::Step,
            1 => Transition::RampTime { end: None },
            _ => Transition::RampBeats { end: None },
        };
        pts.push(pt(
            i as f64 * 2.0,
            60.0 + rng.next() * 140.0,
            if i == 19_999 { Transition::Step } else { shape },
        ));
    }
    let m = TempoMap::new(pts).unwrap();
    let total = m.seconds_at(Beat::new(40_000.0)).get();
    assert!(total > 3600.0 * 4.0, "a session of hours: {total} s");
    let (mut worst_beat, mut worst_sec) = (0.0f64, 0.0f64);
    let mut bad = 0;
    for _ in 0..200_000 {
        let b = rng.next() * 40_000.0;
        let s = m.seconds_at(Beat::new(b)).get();
        let back = m.beat_at_seconds(Seconds::new(s)).get();
        worst_beat = worst_beat.max((back - b).abs());
        worst_sec = worst_sec.max((m.seconds_at(Beat::new(back)).get() - s).abs());
        let f = Frame::new((rng.next() * total * 48_000.0) as u64);
        if m.view(R).frame_at_or_after(m.view(R).beat_at_frame(f)) != f {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "frame -> beat -> frame mismatches");
    assert!(
        worst_beat < 1e-9,
        "beat -> seconds -> beat worst {worst_beat:e} beats"
    );
    assert!(
        worst_sec * 48_000.0 < 1e-4,
        "worst {:e} frames",
        worst_sec * 48_000.0
    );
}

/// Anchors over 100 000 segments: cycle 100, 120, 150, 200 BPM over 1-beat
/// steps (0.6, 0.5, 0.4, 0.3 s), so the anchor of segment `4m` is `9m/5` s. A
/// naive running sum is off by about 1e-8 s; the compensated one by under 1e-10.
#[test]
fn anchors_do_not_drift() {
    let n = 100_000usize;
    let tempo = |i: usize| [100.0, 120.0, 150.0, 200.0][i % 4];
    let m = TempoMap::new((0..n).map(|i| pt(i as f64, tempo(i), Transition::Step))).unwrap();
    let want = |k: usize| (9 * (k / 4)) as f64 / 5.0;
    let mut worst: f64 = 0.0;
    for k in (0..n).step_by(4) {
        worst = worst.max((m.seconds_at(Beat::new(k as f64)).get() - want(k)).abs());
    }
    assert!(worst < 1e-10, "worst {worst:e} s");
    // The test can fail: a naive sum drifts past the bound.
    let (mut naive, mut naive_worst) = (0.0f64, 0.0f64);
    for k in 0..n {
        if k % 4 == 0 {
            naive_worst = naive_worst.max((naive - want(k)).abs());
        }
        naive += 60.0 / tempo(k);
    }
    assert!(
        naive_worst > 1e-10,
        "the naive sum no longer drifts: {naive_worst:e} s"
    );
}

/// An edit builds a new map: beats before it keep their seconds, beats after it
/// move, and the old map is untouched. Adding at an existing beat replaces.
#[test]
fn edit_in_the_past() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::Step),
        pt(16.0, 120.0, Transition::Step),
    ])
    .unwrap();
    let m2 = m.with_point(pt(8.0, 60.0, Transition::Step)).unwrap();
    assert_eq!(
        m.seconds_at(Beat::new(4.0)).get(),
        m2.seconds_at(Beat::new(4.0)).get()
    );
    // 8 beats at 120 (4 s) then 8 beats at 60 (8 s) = 12 s at beat 16, vs 8 s.
    assert_eq!(m2.seconds_at(Beat::new(16.0)).get(), 12.0);
    assert_eq!(m.seconds_at(Beat::new(16.0)).get(), 8.0);
    // Beat 20 (was 10 s) is now 12 + 4 beats at 120 (2 s) = 14 s.
    assert_eq!(m2.seconds_at(Beat::new(20.0)).get(), 14.0);
    // The same beat again replaces the point rather than adding one.
    let m3 = m2.with_point(pt(8.0, 240.0, Transition::Step)).unwrap();
    assert_eq!(m3.points().len(), 3);
    assert_eq!(m3.seconds_at(Beat::new(16.0)).get(), 4.0 + 2.0);
    assert_eq!(m2.points().len(), 3);
    assert_eq!(
        m2.tempo_at(Beat::new(9.0)),
        Bpm::of(60.0),
        "the earlier map is unchanged"
    );
}

/// Every refusal, with the index it names. A non-finite or non-positive tempo
/// never gets here: `Bpm` refuses it.
#[test]
fn refusals() {
    use Transition::Step;
    let (ramp_time, ramp_beats) = (
        Transition::RampTime { end: None },
        Transition::RampBeats { end: None },
    );
    assert_eq!(TempoMap::new([]).unwrap_err(), TempoError::Empty);
    assert_eq!(
        TempoMap::new([pt(1.0, 120.0, Step)]).unwrap_err(),
        TempoError::FirstNotAtZero
    );
    assert_eq!(
        TempoMap::new([pt(f64::NAN, 120.0, Step)]).unwrap_err(),
        TempoError::FirstNotAtZero
    );
    // A zero, negative or non-finite tempo is not a `Bpm`, so a map is never
    // handed one and checks none.
    for bad in [
        0.0,
        -120.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
    ] {
        assert_eq!(Bpm::new(bad), None, "{bad}");
    }
    // The smallest tempos the type holds: a beat's rate underflows to zero for
    // the smallest, which the map refuses as unrepresentable, and the next
    // one up builds.
    assert_eq!(
        TempoMap::constant(Bpm::of(f64::from_bits(1))).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    assert!(TempoMap::constant(Bpm::of(f64::MIN_POSITIVE)).is_ok());
    for beat in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            TempoMap::new([pt(0.0, 120.0, Step), pt(beat, 90.0, Step)]).unwrap_err(),
            TempoError::BadOrder(1),
            "{beat}"
        );
    }
    assert_eq!(
        TempoMap::new([
            pt(0.0, 120.0, Step),
            pt(4.0, 90.0, Step),
            pt(4.0, 80.0, Step)
        ])
        .unwrap_err(),
        TempoError::BadOrder(2)
    );
    assert_eq!(
        TempoMap::new([pt(0.0, 120.0, ramp_time)]).unwrap_err(),
        TempoError::RampWithoutEnd
    );
    assert_eq!(
        TempoMap::new([pt(0.0, 120.0, ramp_beats)]).unwrap_err(),
        TempoError::RampWithoutEnd
    );
    // A tempo whose square overflows, a span whose duration overflows.
    assert_eq!(
        TempoMap::new([pt(0.0, 1e200, ramp_time), pt(4.0, 1e200 * 2.0, Step)]).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    assert_eq!(
        TempoMap::new([pt(0.0, 1e-300, Step), pt(1e300, 120.0, Step)]).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    // Squares overflow while the ramp's rate of change stays finite: only the
    // check that the closed forms agree end to end sees it.
    assert_eq!(
        TempoMap::new([pt(0.0, 9e155, ramp_time), pt(1.0, 9.009e155, Step)]).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    // The error reads as a sentence and is a `std::error::Error`.
    let e: Box<dyn std::error::Error> = Box::new(TempoError::Unrepresentable(2));
    assert!(e.to_string().contains("point 2"));
}

/// A view's hint never changes a result: monotone, backward and random queries
/// through one view agree with cold lookups, bit for bit.
#[test]
fn a_views_hint_agrees_with_cold_lookup() {
    let mut rng = Rng::new(11);
    let pts: Vec<_> = (0..300)
        .map(|i| {
            let shape = [
                Transition::Step,
                Transition::RampTime { end: None },
                Transition::RampBeats { end: None },
            ][(rng.next() * 3.0) as usize];
            pt(
                i as f64 * 1.5,
                70.0 + rng.next() * 100.0,
                if i == 299 { Transition::Step } else { shape },
            )
        })
        .collect();
    let m = TempoMap::new(pts).unwrap();
    let mut view = m.view(R);
    let mut queries: Vec<f64> = (0..2000).map(|i| i as f64 * 0.25).collect(); // monotone
    queries.extend((0..2000).rev().map(|i| i as f64 * 0.25)); // backward
    queries.extend((0..2000).map(|_| rng.next() * 500.0)); // random
    for b in queries {
        let beat = Beat::new(b);
        assert_eq!(
            view.seconds_at(beat).get(),
            m.seconds_at(beat).get(),
            "beat {b}"
        );
        let s = m.seconds_at(beat).get();
        assert_eq!(
            view.beat_at_seconds(Seconds::new(s)),
            m.beat_at_seconds(Seconds::new(s)),
            "seconds {s}"
        );
        assert_eq!(
            view.frame_at_or_after(beat),
            m.view(R).frame_at_or_after(beat)
        );
    }
}

/// A point's own beat belongs to that point, and seconds are continuous across
/// it: the end of a ramp is the start of the next segment.
#[test]
fn a_point_belongs_to_the_segment_it_starts() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::RampTime { end: None }),
        pt(4.0, 180.0, Transition::Step),
        pt(8.0, 90.0, Transition::RampBeats { end: None }),
        pt(12.0, 60.0, Transition::Step),
    ])
    .unwrap();
    assert_eq!(m.tempo_at(Beat::new(4.0)), Bpm::of(180.0));
    assert_eq!(m.tempo_at(Beat::new(8.0)), Bpm::of(90.0));
    assert_eq!(m.tempo_at(Beat::new(12.0)), Bpm::of(60.0));
    assert!(
        (m.tempo_at(Beat::new(4.0_f64.next_down())).get() - 180.0).abs() < 1e-9,
        "the ramp arrives at 180"
    );
    for b in [4.0_f64, 8.0, 12.0] {
        let below = m.seconds_at(Beat::new(b.next_down())).get();
        assert!(
            (m.seconds_at(Beat::new(b)).get() - below).abs() < 1e-12,
            "seconds jump at beat {b}"
        );
        let s = m.seconds_at(Beat::new(b)).get();
        assert!(
            (m.beat_at_seconds(Seconds::new(s)).get() - b).abs() < 1e-12,
            "beat {b}"
        );
    }
}

/// Before beat 0 the map runs at the first tempo, even when the first segment
/// is a ramp (a count-in has a time, and `sqrt` of a negative is not asked).
#[test]
fn a_first_ramp_extrapolates_at_its_first_tempo() {
    for shape in [
        Transition::RampTime { end: None },
        Transition::RampBeats { end: None },
    ] {
        let m = TempoMap::new([pt(0.0, 120.0, shape), pt(4.0, 240.0, Transition::Step)]).unwrap();
        assert_eq!(m.seconds_at(Beat::new(-1.0)).get(), -0.5);
        assert_eq!(m.beat_at_seconds(Seconds::new(-0.5)), Beat::new(-1.0));
        assert_eq!(m.tempo_at(Beat::new(-100.0)), Bpm::of(120.0));
        assert_eq!(m.tempo_at_seconds(Seconds::new(-3.0)), Bpm::of(120.0));
    }
}

/// A tempo is given back to the bit at its own point, ramp or step: the given
/// tempo plus a change of 0, not `60 * (bpm / 60)` (which differs by an ulp
/// for about 5% of tempos).
#[test]
fn a_tempo_at_its_point_is_the_given_tempo_to_the_bit() {
    let mut rng = Rng::new(3);
    let n = 10_000;
    let pts: Vec<_> = (0..n)
        .map(|i| {
            let shape = [
                Transition::Step,
                Transition::RampTime { end: None },
                Transition::RampBeats { end: None },
            ][i % 3];
            pt(
                i as f64 * 1.5,
                40.0 + rng.next() * 400.0,
                if i == n - 1 { Transition::Step } else { shape },
            )
        })
        .collect();
    let inexact = pts
        .iter()
        .filter(|p| (p.tempo.get() / 60.0) * 60.0 != p.tempo.get())
        .count();
    assert!(
        inexact > 100,
        "the case has tempos that `/ 60 * 60` changes: {inexact}"
    );
    let m = TempoMap::new(pts.clone()).unwrap();
    for p in &pts {
        assert_eq!(
            m.tempo_at(p.beat).get().to_bits(),
            p.tempo.get().to_bits(),
            "tempo_at {:?}",
            p
        );
        let sec = m.seconds_at(p.beat).get();
        assert_eq!(
            m.tempo_at_seconds(Seconds::new(sec)).get().to_bits(),
            p.tempo.get().to_bits(),
            "tempo_at_seconds {:?}",
            p
        );
    }
}

/// `-0.0` is beat 0: an edit there replaces the first point.
#[test]
fn an_edit_at_negative_zero_replaces_the_first_point() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::Step),
        pt(8.0, 90.0, Transition::Step),
    ])
    .unwrap();
    let m2 = m.with_point(pt(-0.0, 100.0, Transition::Step)).unwrap();
    assert_eq!(m2.points().len(), 2);
    assert_eq!(m2.tempo_at(Beat::new(0.0)), Bpm::of(100.0));
    assert_eq!(
        m2.points()[0].beat.get().to_bits(),
        0.0f64.to_bits(),
        "stored as +0.0"
    );
}

/// `constant_tempo` is the one point's tempo to the bit, and `None` for any
/// map that changes, whatever its tempos: two points of the same tempo are
/// still a map with a step in it as far as a reader that follows one tempo
/// goes.
#[test]
fn constant_tempo_is_the_single_points_tempo_to_the_bit() {
    let odd = Bpm::of(97.123_456_789_012_35);
    assert_eq!(TempoMap::constant(odd).unwrap().constant_tempo(), Some(odd));
    for shape in [
        Transition::Step,
        Transition::RampTime { end: None },
        Transition::RampBeats { end: None },
    ] {
        let two = TempoMap::new([
            TempoPoint {
                beat: Beat::new(0.0),
                tempo: odd,
                transition: shape,
            },
            TempoPoint {
                beat: Beat::new(4.0),
                tempo: odd,
                transition: Transition::Step,
            },
        ])
        .unwrap();
        assert_eq!(two.constant_tempo(), None, "{shape:?}");
    }
}

/// The tempo on a frame is the curve's instantaneous tempo, which is the
/// tempo at that second: 120 -> 180 linear in time reads 150 at 0.8 s (frame
/// 38 400) and 120 on frame 0 to the bit; linear in beats (`g = 0.25`) it reads
/// `120 e^(0.25 x 1)` at 1 s (frame 48 000), written with `libm::exp`.
#[test]
fn tempo_on_a_frame_is_the_tempo_at_that_second() {
    let time = TempoMap::new([
        pt(0.0, 120.0, Transition::RampTime { end: None }),
        pt(4.0, 180.0, Transition::Step),
    ])
    .unwrap();
    assert_eq!(time.view(R).tempo_at_frame(Frame::new(0)), Bpm::of(120.0));
    assert!((time.view(R).tempo_at_frame(Frame::new(38_400)).get() - 150.0).abs() < 1e-12);
    let beats = TempoMap::new([
        pt(0.0, 120.0, Transition::RampBeats { end: None }),
        pt(4.0, 180.0, Transition::Step),
    ])
    .unwrap();
    let want = 120.0 * libm::exp(0.25);
    assert!((beats.view(R).tempo_at_frame(Frame::new(48_000)).get() - want).abs() < 1e-12);
}

/// 120 -> 180 over four beats (tempo linear in time, 1.6 s), 180 to beat 8
/// (until 1.6 + 4 / 3 s), then 60: inside the ramp the slope is 60 BPM / 1.6 s
/// = 37.5 BPM per second whatever the span; a span that runs past the ramp's
/// end is held to it; a step segment has none, and the jump down to 60 at
/// its end is not part of a span that crosses it; an empty or reversed span
/// has none.
#[test]
fn the_slope_is_held_inside_the_segment_it_starts_in() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::RampTime { end: None }),
        pt(4.0, 180.0, Transition::Step),
        pt(8.0, 60.0, Transition::Step),
    ])
    .unwrap();
    let mut view = m.view(R);
    for (from, to) in [(0.0, 0.5), (0.25, 1.0), (1.0, 1.6)] {
        let got = view.tempo_slope(Seconds::new(from), Seconds::new(to));
        assert!((got - 37.5).abs() < 1e-9, "{from}..{to}: {got}");
    }
    // The ramp ends at 1.6 s; a span to 2.0 s is held to that end.
    let held = view.tempo_slope(Seconds::new(1.0), Seconds::new(2.0));
    assert!((held - 37.5).abs() < 1e-9, "{held}");
    // A step segment, though its end (2.933 s) is crossed: no slope.
    assert_eq!(view.tempo_slope(Seconds::new(2.0), Seconds::new(3.5)), 0.0);
    // Empty and reversed spans.
    assert_eq!(view.tempo_slope(Seconds::new(0.5), Seconds::new(0.5)), 0.0);
    assert_eq!(view.tempo_slope(Seconds::new(0.5), Seconds::new(0.2)), 0.0);
    // A beat ramp's tempo is exponential in time (120 e^(0.25 s)), so a span
    // past its end (4 ln 1.5 = 1.6219 s) is the chord to that end, not to the
    // span's own: (180 - 120 e^0.125) / (1.6219 - 0.5).
    let beats = TempoMap::new([
        pt(0.0, 120.0, Transition::RampBeats { end: None }),
        pt(4.0, 180.0, Transition::Step),
    ])
    .unwrap();
    let end = 4.0 * libm::log(1.5);
    let want = (180.0 - 120.0 * libm::exp(0.125)) / (end - 0.5);
    let got = beats
        .view(R)
        .tempo_slope(Seconds::new(0.5), Seconds::new(5.0));
    assert!((got - want).abs() < 1e-9, "{got} vs {want}");
}

/// One view, asked in order across a step and a ramp, answers the closed
/// forms. Constant 90 BPM at 48 kHz: beat `b` is frame `b * 60 / 90 * 48 000`.
/// Then 120 to 180 BPM over beats 4 to 8 (linear in time): its rate in beats
/// per second is `2 + 0.625 t` over `t` seconds from beat 4, so beat 8 is
/// `t = 1.6` s later, and `t = 0.8` s in is beat `4 + 1.6 + 0.2`.
#[test]
fn a_view_answers_the_closed_forms_across_a_step_and_a_ramp() {
    let m = TempoMap::new([
        pt(0.0, 90.0, Transition::Step),
        pt(4.0, 120.0, Transition::RampTime { end: None }),
        pt(8.0, 180.0, Transition::Step),
    ])
    .unwrap();
    let mut view = m.view(R);
    assert_eq!(view.rate(), R);
    // 4 beats at 90 BPM: 2.666.. s.
    assert_eq!(view.frame_at_or_after(Beat::new(3.0)), Frame::new(96_000));
    assert_eq!(view.beat_at_frame(Frame::new(96_000)), Beat::new(3.0));
    assert_eq!(view.tempo_at_frame(Frame::new(96_000)), Bpm::of(90.0));
    let ramp_start = 4.0 * 60.0 / 90.0;
    let on = Frame::new((ramp_start * 48_000.0) as u64);
    assert_eq!(view.tempo_at_frame(on), Bpm::of(120.0));
    let at = Frame::new(((ramp_start + 0.8) * 48_000.0) as u64);
    assert!((view.beat_at_frame(at).get() - 5.8).abs() < 1e-9);
    assert!((view.tempo_at_frame(at).get() - 150.0).abs() < 1e-9);
    let end = view.frame_at_or_after(Beat::new(8.0));
    assert_eq!(end, Frame::new(((ramp_start + 1.6) * 48_000.0) as u64));
}

/// A builder writes the same map as its points do, and a ramp it writes always
/// has a point after it: the last point is a step whatever was called last.
/// Its beats are still checked, by the map's own rule.
#[test]
fn the_builder_writes_the_points_and_never_ends_on_a_ramp() {
    let built = TempoMap::builder(Bpm::of(120.0))
        .ramp_time_to(Beat::new(4.0), Bpm::of(180.0))
        .ramp_beats_to(Beat::new(8.0), Bpm::of(90.0))
        .build()
        .unwrap();
    assert_eq!(
        built.points(),
        [
            TempoPoint::ramp_time(Beat::new(0.0), Bpm::of(120.0)),
            TempoPoint::ramp_beats(Beat::new(4.0), Bpm::of(180.0)),
            TempoPoint::step(Beat::new(8.0), Bpm::of(90.0)),
        ]
    );
    assert!(matches!(
        TempoMap::builder(Bpm::of(120.0))
            .step_to(Beat::new(0.0), Bpm::of(90.0))
            .build(),
        Err(TempoError::BadOrder(1))
    ));
    // The list form still refuses the trailing ramp.
    assert!(matches!(
        TempoMap::new([TempoPoint::ramp_time(Beat::new(0.0), Bpm::of(120.0))]),
        Err(TempoError::RampWithoutEnd)
    ));
}

/// 120 -> 180 BPM linear in time over beats 0..4 lasts `2 * 4 / (2 + 3)` = 1.6 s
/// and ends in a step to 90 at beat 4, in one point. The ramp's tempo against
/// beats is `60 sqrt(r0^2 + 2 k b)` with `r0` = 2 beats/s and
/// `k` = (3 - 2) / 1.6 = 0.625 beats/s^2.
fn ramp_ending_in_a_step() -> TempoMap {
    TempoMap::builder(Bpm::of(120.0))
        .ramp_time_to(Beat::new(4.0), Bpm::of(180.0))
        .then_step(Bpm::of(90.0))
        .build()
        .unwrap()
}

/// **A ramp that ends in a step has no plateau**: the tempo one ulp below the
/// point is the ramp's analytic value (180 there, `60 sqrt(7.75)` at beat 3),
/// at the point it is the step's 90, and the map holds exactly two points.
#[test]
fn a_ramp_ending_in_a_step_leaves_no_plateau() {
    let map = ramp_ending_in_a_step();
    assert_eq!(map.points().len(), 2);
    let below = map.tempo_at(Beat::new(4.0f64.next_down())).get();
    assert!((below - 180.0).abs() < 1e-9, "{below}");
    assert_eq!(map.tempo_at(Beat::new(4.0)), Bpm::of(90.0));
    let mid = map.tempo_at(Beat::new(3.0)).get();
    assert!((mid - 60.0 * 7.75f64.sqrt()).abs() < 1e-9, "{mid}");
    let in_time = map.tempo_at_seconds(Seconds::new(1.6f64.next_down())).get();
    assert!((in_time - 180.0).abs() < 1e-6, "{in_time}");
    assert_eq!(map.tempo_at_seconds(Seconds::new(1.6)), Bpm::of(90.0));
    assert_eq!(map.tempo_at(Beat::new(100.0)), Bpm::of(90.0));
}

/// **A step changes the tempo, not the time**: beat 4 is at the ramp's integral,
/// 1.6 s; after it the tempo is 90 (1.5 beats/s), so beat 5.5 is at 2.6 s. A
/// ramp linear in beats from 120 to 180 over 4 beats (`g` = 0.25) lasts
/// `4 ln 1.5` the same way.
#[test]
fn a_step_after_a_ramp_keeps_the_ramps_time() {
    let map = ramp_ending_in_a_step();
    assert!((map.seconds_at(Beat::new(4.0)).get() - 1.6).abs() < 1e-12);
    assert!((map.seconds_at(Beat::new(5.5)).get() - 2.6).abs() < 1e-12);
    let near = map.seconds_at(Beat::new(4.0f64.next_down())).get();
    assert!((near - 1.6).abs() < 1e-12, "continuous below: {near}");

    let beats = TempoMap::builder(Bpm::of(120.0))
        .ramp_beats_to(Beat::new(4.0), Bpm::of(180.0))
        .then_step(Bpm::of(60.0))
        .build()
        .unwrap();
    let want = 4.0 * libm::log(1.5);
    assert!((beats.seconds_at(Beat::new(4.0)).get() - want).abs() < 1e-12);
    assert_eq!(beats.tempo_at(Beat::new(4.0)), Bpm::of(60.0));
    assert!((beats.seconds_at(Beat::new(5.0)).get() - want - 1.0).abs() < 1e-12);
}

/// **Time to beat round-trips across the point**, below it, on it and after it.
#[test]
fn time_and_beat_round_trip_across_a_ramps_step() {
    let map = ramp_ending_in_a_step();
    for b in [0.0, 1.0, 3.999, 4.0, 4.001, 5.5, 20.0] {
        let back = map.beat_at_seconds(map.seconds_at(Beat::new(b))).get();
        assert!((back - b).abs() < 1e-12, "beat {b}: {back}");
    }
    assert!((map.beat_at_seconds(Seconds::new(1.6)).get() - 4.0).abs() < 1e-12);
    assert!((map.beat_at_seconds(Seconds::new(2.6)).get() - 5.5).abs() < 1e-12);
}

/// **One map has one representation**: a ramp whose end is the next point's
/// tempo is the same map as one with no end (the end folds to `None`), and an
/// end that differs is kept.
#[test]
fn a_ramp_end_equal_to_the_next_tempo_folds_away() {
    let ending = |end| Transition::RampTime { end };
    let point = |end| {
        [
            TempoPoint {
                transition: ending(end),
                ..pt(0.0, 120.0, Transition::Step)
            },
            pt(4.0, 180.0, Transition::Step),
        ]
    };
    let plain = TempoMap::new(point(None)).unwrap();
    let folded = TempoMap::new(point(Some(Bpm::of(180.0)))).unwrap();
    assert_eq!(folded, plain);
    assert_eq!(folded.points()[0].transition, ending(None));
    let kept = TempoMap::new(point(Some(Bpm::of(150.0)))).unwrap();
    assert_eq!(kept.points()[0].transition, ending(Some(Bpm::of(150.0))));
    assert_ne!(kept, plain);
}

/// **A second `then_step` replaces the departure and keeps the arrival**: the
/// ramp still ends at 180 (beat 4 is at 1.6 s, the tempo just below it 180) and
/// the tempo from beat 4 is the last value, 60, not 90.
#[test]
fn a_second_then_step_keeps_the_arrival() {
    let map = TempoMap::builder(Bpm::of(120.0))
        .ramp_time_to(Beat::new(4.0), Bpm::of(180.0))
        .then_step(Bpm::of(90.0))
        .then_step(Bpm::of(60.0))
        .build()
        .unwrap();
    assert_eq!(
        map.points()[0].transition,
        Transition::RampTime {
            end: Some(Bpm::of(180.0))
        }
    );
    assert_eq!(map.tempo_at(Beat::new(4.0)), Bpm::of(60.0));
    let below = map.tempo_at(Beat::new(4.0f64.next_down())).get();
    assert!((below - 180.0).abs() < 1e-9, "{below}");
    assert!((map.seconds_at(Beat::new(4.0)).get() - 1.6).abs() < 1e-12);
    assert!((map.seconds_at(Beat::new(5.0)).get() - 2.6).abs() < 1e-12);
}

/// A beat-linear ramp with an end: 120 to 180 over beats 0..4 (`g` = 0.25
/// beats/s per beat) reads `120 + 15 b` BPM, 165 at beat 3 and 180 just below
/// beat 4, though the next point's own tempo is 60.
#[test]
fn a_beat_ramp_with_an_end_follows_its_own_line() {
    let map = TempoMap::new([
        TempoPoint {
            transition: Transition::RampBeats {
                end: Some(Bpm::of(180.0)),
            },
            ..pt(0.0, 120.0, Transition::Step)
        },
        pt(4.0, 60.0, Transition::Step),
    ])
    .unwrap();
    let mid = map.tempo_at(Beat::new(3.0)).get();
    assert!((mid - 165.0).abs() < 1e-9, "{mid}");
    let below = map.tempo_at(Beat::new(4.0f64.next_down())).get();
    assert!((below - 180.0).abs() < 1e-9, "{below}");
    assert_eq!(map.tempo_at(Beat::new(4.0)), Bpm::of(60.0));
}

/// **`then_step` changes only the last segment**: after a step at beat 2 to 90
/// and a ramp to 180 at beat 6, a `then_step` to 60 leaves every beat at the same
/// time and every earlier tempo as it was; the ramp still ends at 180 and the
/// tempo from beat 6 is 60.
#[test]
fn then_step_on_a_map_with_earlier_points_changes_only_the_last_segment() {
    let base = || {
        TempoMap::builder(Bpm::of(120.0))
            .step_to(Beat::new(2.0), Bpm::of(90.0))
            .ramp_time_to(Beat::new(6.0), Bpm::of(180.0))
    };
    let (plain, stepped) = (
        base().build().unwrap(),
        base().then_step(Bpm::of(60.0)).build().unwrap(),
    );
    for b in [0.0, 1.0, 2.0, 4.0, 6.0] {
        assert_eq!(
            plain.seconds_at(Beat::new(b)),
            stepped.seconds_at(Beat::new(b)),
            "beat {b}"
        );
    }
    assert_eq!(stepped.points().len(), 3);
    assert_eq!(stepped.points()[0], plain.points()[0]);
    assert_eq!(stepped.tempo_at(Beat::new(1.0)), Bpm::of(120.0));
    assert_eq!(stepped.tempo_at(Beat::new(2.0)), Bpm::of(90.0));
    assert_eq!(stepped.tempo_at(Beat::new(6.0)), Bpm::of(60.0));
    assert_eq!(plain.tempo_at(Beat::new(6.0)), Bpm::of(180.0));
    let below = stepped.tempo_at(Beat::new(6.0f64.next_down())).get();
    assert!((below - 180.0).abs() < 1e-9, "{below}");
}

/// **The next point is strictly after the beat**: a point on the beat is the one the beat is
/// on, so it is not "after"; past the last point there is none.
#[test]
fn point_after_is_the_first_point_strictly_after_the_beat() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Transition::Step),
        pt(4.0, 240.0, Transition::Step),
    ])
    .unwrap();
    assert_eq!(m.point_after(Beat::new(0.0)), Some(Beat::new(4.0)));
    assert_eq!(m.point_after(Beat::new(3.5)), Some(Beat::new(4.0)));
    assert_eq!(
        m.point_after(Beat::new(4.0)),
        None,
        "a point on the beat is not after it"
    );
}
