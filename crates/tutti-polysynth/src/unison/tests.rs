use super::*;
use crate::UnisonVoices;

/// **A driven detune below zero is floored at 0 cents.**
#[test]
fn a_driven_detune_is_floored_at_zero() {
    let mut engine = UnisonEngine::new(
        UnisonConfig::new()
            .voice_count(UnisonVoices::of(3))
            .detune_cents(CentsSpan::of(12.0)),
    );
    engine.sync_from_atomics([Some(-10.0), None]);
    assert_eq!(engine.config().detune_cents, CentsSpan::ZERO);
}

#[test]
fn a_single_unison_voice_is_centred_at_unit_gain_and_ratio() {
    let config = UnisonConfig::new().voice_count(UnisonVoices::of(1));
    let unison = UnisonEngine::new(config);

    assert_eq!(unison.voice_count(), UnisonVoices::of(1));

    let params = unison.voice_params(0);
    assert_eq!(params.freq_ratio, 1.0);
    assert!((params.pan.get() - 0.0).abs() < 0.001);
    assert!((params.amplitude.get() - 1.0).abs() < 0.001);
}

#[test]
fn unison_detune_is_symmetric_about_the_centre_voice() {
    let config = UnisonConfig::new()
        .voice_count(UnisonVoices::of(3))
        // ~1/8 semitone spread
        .detune_cents(CentsSpan::of(12.0))
        .stereo_spread(Spread::POINT)
        .phase_randomize(false);
    let unison = UnisonEngine::new(config);

    assert_eq!(unison.voice_count(), UnisonVoices::of(3));

    // Center voice should be at ratio 1.0
    let center = unison.voice_params(1);
    assert_eq!(center.freq_ratio, 1.0);

    // First voice should be detuned down
    let low = unison.voice_params(0);
    assert!(low.freq_ratio < 1.0);

    // Last voice should be detuned up
    let high = unison.voice_params(2);
    assert!(high.freq_ratio > 1.0);

    // Symmetric detune
    let low_ratio = 1.0 / low.freq_ratio;
    let high_ratio = high.freq_ratio;
    // `1 / 2^-x` and `2^x` round independently: a few ulps apart.
    assert!((low_ratio - high_ratio).abs() < 1e-12);
}

#[test]
fn a_full_spread_pans_the_outer_voices_hard_left_and_right() {
    let config = UnisonConfig::new()
        .voice_count(UnisonVoices::of(3))
        .detune_cents(CentsSpan::ZERO)
        .stereo_spread(Spread::new(1.0))
        .phase_randomize(false);
    let unison = UnisonEngine::new(config);

    let left = unison.voice_params(0);
    let center = unison.voice_params(1);
    let right = unison.voice_params(2);

    assert!((left.pan.get() - (-1.0)).abs() < 0.001);
    assert!((center.pan.get() - 0.0).abs() < 0.001);
    assert!((right.pan.get() - 1.0).abs() < 0.001);
}

#[test]
fn unison_amplitudes_square_to_unity_for_every_count() {
    for count in 1..=8 {
        let config =
            UnisonConfig::new().voice_count(UnisonVoices::new(count).expect("a unison count"));
        let unison = UnisonEngine::new(config);

        // Sum of squared amplitudes should equal 1.0 (equal power)
        let sum_sq: f64 = unison
            .all_params()
            .iter()
            .map(|p| p.amplitude.get() * p.amplitude.get())
            .sum();

        assert!(
            (sum_sq - 1.0).abs() < 0.01,
            "Equal power failed for {} voices: {}",
            count,
            sum_sq
        );
    }
}

#[test]
fn randomized_phases_lie_in_range_and_differ() {
    let config = UnisonConfig::new()
        .voice_count(UnisonVoices::of(4))
        .phase_randomize(true);
    let mut unison = UnisonEngine::new(config);

    unison.randomize_phases();

    // Phases should be different
    let phases: Vec<f64> = unison
        .all_params()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect();

    // Check phases are in valid range
    for phase in &phases {
        assert!(*phase >= 0.0 && *phase <= 1.0);
    }

    // Check they're not all the same
    let first = phases[0];
    let all_same = phases.iter().all(|p| (*p - first).abs() < 0.001);
    assert!(!all_same, "Phases should be randomized");
}

/// Two engines of one config draw the same phases, and the voices of one draw have distinct ones.
#[test]
fn drawn_phases_are_the_same_for_a_seed_and_distinct_per_voice() {
    let draw = || -> Vec<f64> {
        UnisonEngine::new(
            UnisonConfig::new()
                .voice_count(UnisonVoices::of(8))
                .phase_randomize(true),
        )
        .randomize_phases()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect()
    };
    let (a, b) = (draw(), draw());
    assert_eq!(a, b, "a seed reproduces its phases");
    for (i, x) in a.iter().take(8).enumerate() {
        for y in &a[..i] {
            assert_ne!(x, y, "two voices drew one phase");
        }
    }
}

#[test]
fn the_detune_and_spread_atomics_update_the_config() {
    let mut unison = UnisonEngine::new(UnisonConfig::new().voice_count(UnisonVoices::of(5)));
    assert_eq!(unison.voice_count(), UnisonVoices::of(5));

    unison.sync_from_atomics([Some(20.0), Some(0.5)]);
    assert_eq!(unison.config().detune_cents, CentsSpan::of(20.0));
    assert!((unison.config().stereo_spread.get() - 0.5).abs() < 0.001);
}

/// `UnisonVoices` keeps the range-checked shape: `new` refuses 0
/// and anything past 16 and never clamps, `TryFrom` agrees and carries the
/// refused raw, `of` panics. The checked shape is serde's, which the type has
/// none of in the library: the check runs on the same `try_from` shape
/// written here.
#[test]
fn unison_voices_keep_the_range_checked_shape() {
    use serde::{Deserialize, Serialize};
    use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Checked(UnisonVoices);
    impl Serialize for Checked {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.get().serialize(s)
        }
    }
    impl<'de> Deserialize<'de> for Checked {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            UnisonVoices::try_from(u8::deserialize(d)?)
                .map(Checked)
                .map_err(serde::de::Error::custom)
        }
    }
    impl TryFrom<u8> for Checked {
        type Error = crate::UnisonVoicesError;
        fn try_from(raw: u8) -> Result<Self, Self::Error> {
            UnisonVoices::try_from(raw).map(Checked)
        }
    }
    impl From<Checked> for u8 {
        fn from(c: Checked) -> u8 {
            c.0.get()
        }
    }
    let c = RangeContract {
        new: |raw| UnisonVoices::new(raw).map(Checked),
        get: |c| c.0.get(),
        of: Some(|raw| Checked(UnisonVoices::of(raw))),
        valid: &[1, 2, 16],
        invalid: &[0, 17, 100, u8::MAX],
    };
    check_range_contract(&c);
    check_range_serde(&c);
}

/// Recomputing detune/spread keeps the drawn phases.
///
/// `sync_from_atomics` recomputes whenever a modulated detune or spread
/// moves, and in `tick` that runs after MIDI is polled — so a recompute
/// that reset the phases wiped the note-on's draw before anything used it.
#[test]
fn recompute_keeps_the_drawn_phases() {
    let mut unison = UnisonEngine::new(
        UnisonConfig::new()
            .voice_count(UnisonVoices::of(4))
            .phase_randomize(true),
    );
    let drawn: Vec<f64> = unison
        .randomize_phases()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect();
    unison.sync_from_atomics([Some(25.0), None]);
    let kept: Vec<f64> = unison
        .all_params()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect();
    assert_eq!(drawn, kept, "a detune change wiped the drawn phases");
}
