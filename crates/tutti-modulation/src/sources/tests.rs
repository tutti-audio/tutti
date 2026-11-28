use super::*;
use crate::{Adsr, RateAnchor};
use tutti_units::{Amplitude, Seconds};

/// The fields of a [`TimeSource::Lfo`]: `(shape, seed, clock)`.
fn lfo_parts(source: TimeSource) -> (LfoShape, u64, LayerClock) {
    match source {
        TimeSource::Lfo {
            wave: Waveform::Shape { shape, seed },
            clock,
        } => (shape, seed, clock),
        other => panic!("not a shaped LFO: {other:?}"),
    }
}

/// Each named constructor's shape, at the defaults: seed 0, one cycle a
/// second, phase 0, no anchor.
#[test]
fn each_lfo_constructor_is_its_shape_at_one_hertz() {
    let rows = [
        (Lfo::sine(), LfoShape::Sine),
        (Lfo::triangle(), LfoShape::Triangle),
        (Lfo::square(), LfoShape::Square),
        (Lfo::saw(), LfoShape::Sawtooth),
        (Lfo::new(LfoShape::RandomSmooth), LfoShape::RandomSmooth),
    ];
    for (lfo, want) in rows {
        let (shape, seed, clock) = lfo_parts(lfo.into());
        assert_eq!(shape, want);
        assert_eq!(seed, 0);
        assert_eq!(
            clock,
            LayerClock::Seconds {
                rate: Hz::new(1.0),
                phase: PhaseIncrement::new(0.0),
                anchor: None,
            },
            "{want:?}"
        );
    }
}

/// A `rate` of beats is a beat clock with the phase; a `rate` of `Hz` is a
/// seconds clock with the phase and no anchor; `seed` reaches the wave. The
/// last `rate` wins.
#[test]
fn an_lfos_clock_and_seed_are_pinned_field_by_field() {
    let beats = Lfo::saw()
        .rate(Hz::new(4.0))
        .rate(BeatsPerCycle::of(0.5))
        .phase(PhaseIncrement::new(0.25))
        .seed(7);
    let (shape, seed, clock) = lfo_parts(beats.into());
    assert_eq!(shape, LfoShape::Sawtooth);
    assert_eq!(seed, 7);
    assert_eq!(
        clock,
        LayerClock::Beats {
            per_cycle: BeatsPerCycle::of(0.5),
            phase: PhaseIncrement::new(0.25),
        }
    );

    let seconds = Lfo::sine()
        .rate(BeatsPerCycle::of(2.0))
        .rate(Hz::new(3.0))
        .phase(PhaseIncrement::new(0.5));
    let (_, _, clock) = lfo_parts(seconds.into());
    assert_eq!(
        clock,
        LayerClock::Seconds {
            rate: Hz::new(3.0),
            phase: PhaseIncrement::new(0.5),
            anchor: None::<RateAnchor>,
        }
    );
}

/// A macro is its value.
#[test]
fn a_macro_is_its_value() {
    assert_eq!(
        TimeSource::from(Macro(-0.4)),
        TimeSource::Macro { value: -0.4 }
    );
}

/// A follower's times are a `Ballistics`: a negative, NaN or infinite time has no
/// value, and the pair is the follower the compiler reads.
#[test]
fn a_follower_is_a_ballistics_and_a_bad_time_has_none() {
    let b = Ballistics::new(Seconds::new(0.02), Seconds::new(0.3)).expect("valid times");
    assert_eq!(SignalKind::from(b), SignalKind::Follower(b));
    for (attack, release) in [
        (-0.1, 0.1),
        (0.1, -0.1),
        (f64::NAN, 0.1),
        (0.1, f64::INFINITY),
    ] {
        assert!(
            Ballistics::new(Seconds::new(attack), Seconds::new(release)).is_none(),
            "{attack} / {release}"
        );
    }
}

/// A note envelope carries its stages unchanged.
#[test]
fn a_note_envelope_carries_its_stages() {
    let adsr = Adsr {
        attack: Seconds::new(0.01),
        decay: Seconds::new(0.2),
        sustain: Amplitude::new(0.6),
        release: Seconds::new(0.5),
    };
    let e = NoteEnvelope::new(adsr).expect("valid stages");
    assert_eq!(e.adsr(), adsr);
    assert_eq!(SignalKind::from(e), SignalKind::NoteEnvelope(e));
}

/// A note envelope's times are finite and not negative and its sustain is in
/// `[0, 1]`; the constructor has no value for anything else, and the bounds
/// themselves are valid.
#[test]
fn a_note_envelope_refuses_stages_out_of_their_domain() {
    let valid = Adsr::default();
    let with = |f: fn(&mut Adsr)| {
        let mut a = valid;
        f(&mut a);
        NoteEnvelope::new(a)
    };
    assert!(NoteEnvelope::new(valid).is_some());
    assert!(with(|a| a.sustain = Amplitude::new(1.5)).is_none());
    assert!(with(|a| a.sustain = Amplitude::new(-0.1)).is_none());
    assert!(with(|a| a.sustain = Amplitude::new(f64::NAN)).is_none());
    assert!(with(|a| a.attack = Seconds::new(-0.01)).is_none());
    assert!(with(|a| a.decay = Seconds::new(f64::INFINITY)).is_none());
    assert!(with(|a| a.release = Seconds::new(f64::NAN)).is_none());
    assert!(with(|a| a.sustain = Amplitude::new(0.0)).is_some());
    assert!(with(|a| a.sustain = Amplitude::new(1.0)).is_some());
    assert!(with(|a| a.attack = Seconds::new(0.0)).is_some());
}
