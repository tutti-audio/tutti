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

/// `to_unit` maps the end points to 0 and 1 and a word `w` to `w / (2^32 - 1)`:
/// 0 is 0, `u32::MAX` is 1, and `2^31` is `2^31 / (2^32 - 1)`, just over a half
/// (0.5 + 1.16e-10, which `f32` rounds to exactly 0.5).
#[test]
fn to_unit_maps_the_word_range_onto_zero_to_one() {
    assert_eq!(Xorshift32::to_unit(0), 0.0);
    assert_eq!(Xorshift32::to_unit(u32::MAX), 1.0);
    let half = Xorshift32::to_unit(1 << 31);
    assert!((f64::from(half) - 2_147_483_648.0 / 4_294_967_295.0).abs() < 1e-7);
    let third = Xorshift32::to_unit(u32::MAX / 3);
    assert!((f64::from(third) - 1.0 / 3.0).abs() < 1e-7, "{third}");
}
