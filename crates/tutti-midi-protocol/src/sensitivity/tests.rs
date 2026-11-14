use super::*;

#[test]
fn pitch_bend_sensitivity_rpn_roundtrip() {
    for s in [2u8, 24, 48, 96] {
        let sens = PitchBendSensitivity::of(s);
        assert_eq!(
            PitchBendSensitivity::from_rpn_bits(sens.to_rpn_bits()),
            sens
        );
        assert_eq!(sens.semitones().get(), f32::from(s));
    }
}

/// **A whole-semitone count is 7 integer bits**: up to 127 it is exact, above
/// it `new` refuses rather than shifting bits out (128 was 0, 200 was 72).
/// Spec: m2-104/7.4.13.2#1
#[test]
fn a_whole_semitone_count_above_127_is_refused() {
    for s in 0..=PitchBendSensitivity::MAX_SEMITONES {
        let sens = PitchBendSensitivity::new(s).expect("in range");
        assert_eq!(sens.semitones().get(), f32::from(s), "{s} semitones");
        assert_eq!(PitchBendSensitivity::of(s), sens);
    }
    for s in 128..=u8::MAX {
        assert_eq!(PitchBendSensitivity::new(s), None, "{s} semitones");
    }
}

#[test]
#[should_panic(expected = "at most 127 semitones")]
fn of_panics_above_127() {
    let _ = PitchBendSensitivity::of(128);
}

/// **Every `u32` is a sensitivity** (M2-104 §7.4.13.3: up to almost 128
/// semitones), so a wire value is never refused and never changes.
#[test]
fn every_rpn_word_is_in_range() {
    for bits in [0, 1, 1 << 24, 48 << 25, 0x0C00_0000, u32::MAX - 1, u32::MAX] {
        let sens = PitchBendSensitivity::from_rpn_bits(bits);
        assert_eq!(sens.to_rpn_bits(), bits);
        // The top words round to exactly 128.0 in f32; the bits are the truth.
        assert!(sens.semitones().get() <= 128.0);
    }
}

#[cfg(feature = "serde")]
#[test]
fn serde_carries_the_bare_bits() {
    let sens = PitchBendSensitivity::of(48);
    let json = serde_json::to_string(&sens).unwrap();
    assert_eq!(json, (48u32 << 25).to_string());
    assert_eq!(
        serde_json::from_str::<PitchBendSensitivity>(&json).unwrap(),
        sens
    );
    let max = serde_json::to_string(&u32::MAX).unwrap();
    assert_eq!(
        serde_json::from_str::<PitchBendSensitivity>(&max).unwrap(),
        PitchBendSensitivity::from_rpn_bits(u32::MAX)
    );
}

/// **RPN #00/00's data is whole semitones then cents** (M2-104 section 7.4.7.1,
/// M2-115 section 4.1): 2 semitones 50 cents is 2.5 semitones, not the 2.39 the
/// same word is as a binary fraction (50 / 128).
/// Spec: m2-104/7.4.7.1
#[test]
fn channel_data_is_semitones_and_cents() {
    let data = RegisteredData::new(U7::of(2), U7::of(50));
    assert_eq!(data.to_word(), 2 << 25 | 50 << 18);
    let sens = PitchBendSensitivity::from_channel_data(data);
    assert_eq!(sens.semitones().get(), 2.5);
    assert_eq!(sens.to_channel_data(), data);
    // The word read as the per-note 7.25 fixed point is another figure.
    let binary = PitchBendSensitivity::from_rpn_bits(data.to_word())
        .semitones()
        .get();
    assert_eq!(binary, 2.0 + 50.0 / 128.0);
    assert_ne!(binary, 2.5);
}

/// The cents field is hundredths of a semitone, so every (semitones, cents)
/// pair with cents below 100 comes back from its sensitivity.
#[test]
fn channel_data_round_trips_every_cent() {
    for semitones in 0..=127 {
        for cents in 0..100 {
            let data = RegisteredData::new(U7::of(semitones), U7::of(cents));
            let sens = PitchBendSensitivity::from_channel_data(data);
            assert_eq!(sens.to_channel_data(), data, "{semitones} st {cents} c");
        }
    }
}

/// A sensitivity within half a cent of 128 semitones saturates at the largest
/// channel data, and a whole-semitone range writes no cents.
#[test]
fn channel_data_saturates_at_127_semitones_99_cents() {
    let top = PitchBendSensitivity::from_rpn_bits(u32::MAX).to_channel_data();
    assert_eq!((top.msb().get(), top.lsb().get()), (127, 99));
    let whole = PitchBendSensitivity::of(48).to_channel_data();
    assert_eq!((whole.msb().get(), whole.lsb().get()), (48, 0));
}

/// The two fields are the top 14 bits, and the 18 bits under them are ignored.
#[test]
fn registered_data_reads_the_top_fourteen_bits() {
    let data = RegisteredData::from_word(0xFFFF_FFFF);
    assert_eq!((data.msb().get(), data.lsb().get()), (127, 127));
    assert_eq!(data.fourteen(), U14::MAX);
    assert_eq!(
        RegisteredData::from_fourteen(U14::of(0x2001)).to_word(),
        0x2001 << 18
    );
    assert_eq!(RegisteredData::from_word(0x0003_FFFF).to_word(), 0);
}
