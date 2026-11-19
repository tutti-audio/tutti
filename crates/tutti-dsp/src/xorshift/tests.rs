use super::*;

fn seed(s: u32) -> Xorshift32 {
    Xorshift32::new(NonZeroU32::new(s).unwrap())
}

/// From state 1 the first word is worked by hand from Marsaglia's shifts:
/// `1 ^ (1 << 13) = 8193`, unchanged by `>> 17` (8193 >> 17 = 0), then
/// `8193 ^ (8193 << 5) = 8193 ^ 262176 = 270369`. The next two follow the same
/// three steps.
#[test]
fn the_recurrence_from_state_one_is_marsaglias() {
    let mut g = seed(1);
    assert_eq!(
        [g.next_u32(), g.next_u32(), g.next_u32()],
        [270_369, 67_634_689, 2_647_435_461]
    );
}

/// The same recurrence from the seed 0x12345678 (three steps of shifts 13, 17,
/// 5): 2274908837, 358294691, 1210119364.
#[test]
fn the_recurrence_from_the_export_seed_is_marsaglias() {
    let mut g = seed(0x1234_5678);
    assert_eq!(
        [g.next_u32(), g.next_u32(), g.next_u32()],
        [2_274_908_837, 358_294_691, 1_210_119_364]
    );
}

/// `to_unit` maps a word `w` to `w / 2^32` truncated to the width's resolution, so it is exact
/// at `f64` (`2^-32` steps), the multiple of `2^-24` at or below `w / 2^32` at `f32`, and under
/// 1 at both widths for the largest word.
#[test]
fn to_unit_is_the_word_over_two_to_the_32_truncated_to_the_width() {
    assert_eq!(Xorshift32::to_unit::<f64>(0), 0.0);
    assert_eq!(Xorshift32::to_unit::<f64>(1 << 31), 0.5);
    assert_eq!(Xorshift32::to_unit::<f64>(u32::MAX), 1.0 - 2f64.powi(-32));
    assert_eq!(Xorshift32::to_unit::<f32>(0), 0.0);
    assert_eq!(Xorshift32::to_unit::<f32>(u32::MAX), 1.0 - 2f32.powi(-24));
    for w in [1_u32, 255, 256, 0x1234_5678, 2_274_908_837, u32::MAX / 3] {
        let truncated = f64::from(w >> 8) * 2f64.powi(-24);
        assert_eq!(f64::from(Xorshift32::to_unit::<f32>(w)), truncated, "{w}");
        assert_eq!(
            Xorshift32::to_unit::<f64>(w),
            f64::from(w) * 2f64.powi(-32),
            "{w}"
        );
    }
}

/// The generator's words are Marsaglia's recurrence (`x ^= x << 13; x ^= x >> 17;
/// x ^= x << 5`) applied to the previous word, written out here, from a stated
/// seed over a thousand steps.
#[test]
fn a_thousand_words_follow_the_published_recurrence() {
    let mut g = seed(0xDEAD_BEEF);
    let mut x = 0xDEAD_BEEF_u32;
    for i in 0..1000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        assert_eq!(g.next_u32(), x, "word {i}");
    }
}

/// `to_bipolar` is `2 u - 1` of `to_unit`: the word 0 is -1, `2^31` is 0 and `3 * 2^30` is a
/// half, exactly, at both widths.
#[test]
fn to_bipolar_maps_the_word_range_onto_minus_one_to_one() {
    assert_eq!(Xorshift32::to_bipolar::<f32>(0), -1.0);
    assert_eq!(Xorshift32::to_bipolar::<f32>(1 << 31), 0.0);
    assert_eq!(Xorshift32::to_bipolar::<f32>(3 << 30), 0.5);
    assert_eq!(
        Xorshift32::to_bipolar::<f64>(u32::MAX),
        1.0 - 2f64.powi(-31)
    );
    assert_eq!(Xorshift32::to_bipolar::<f64>(1 << 30), -0.5);
}
