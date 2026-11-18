//! A pitch bend as a position on `-1..=1`, centre-exact (M2-104 section 7.4.13,
//! "an equal range of pitch bend up"): `k / 2^31` below the centre and
//! `k / (2^31 - 1)` from it up, with `k = get - 2^31`; and the rounded inverse.

use tutti_ump::BendValue;
use tutti_units::Bipolar;

const DOWN: f64 = 2_147_483_648.0;
const UP: f64 = 2_147_483_647.0;

fn samples() -> impl Iterator<Item = u32> {
    let edges = [
        0,
        1,
        2,
        0x4000_0000,
        0x7FFF_FFFF,
        0x8000_0000,
        0x8000_0001,
        0xC000_0000,
        u32::MAX - 1,
        u32::MAX,
    ];
    edges.into_iter().chain((0..=u32::MAX).step_by(65_521))
}

#[test]
fn the_bend_ends_and_centre_are_minus_one_zero_and_plus_one() {
    assert_eq!(BendValue::MIN.bipolar(), Bipolar::MIN);
    assert_eq!(BendValue::CENTER.bipolar(), Bipolar::ZERO);
    assert_eq!(BendValue::MAX.bipolar(), Bipolar::MAX);
    assert_eq!(BendValue::new(0x8000_0001).bipolar().get(), 1.0 / UP);
    assert_eq!(BendValue::new(0x7FFF_FFFF).bipolar().get(), -1.0 / DOWN);
    assert_eq!(BendValue::new(0x4000_0000).bipolar().get(), -0.5);
}

#[test]
fn a_bend_is_its_distance_from_the_centre_over_its_sides_extent() {
    for raw in samples() {
        let k = f64::from(raw) - DOWN;
        let want = if k < 0.0 { k / DOWN } else { k / UP };
        assert_eq!(BendValue::new(raw).bipolar().get(), want, "{raw:#x}");
    }
}

#[test]
fn a_bend_round_trips_through_bipolar_exactly() {
    for raw in samples() {
        let bend = BendValue::new(raw);
        assert_eq!(BendValue::from_bipolar(bend.bipolar()), bend, "{raw:#x}");
    }
}

#[test]
fn plus_one_is_the_top_word_and_the_top_word_is_plus_one() {
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(1.0)),
        BendValue::new(u32::MAX)
    );
    assert_eq!(BendValue::new(u32::MAX).bipolar(), Bipolar::of(1.0));
    assert_eq!(BendValue::from_bipolar(Bipolar::of(-1.0)), BendValue::MIN);
    assert_eq!(BendValue::from_bipolar(Bipolar::ZERO), BendValue::CENTER);
}

#[test]
fn half_either_way_is_a_quarter_of_the_wire_range_from_the_centre() {
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(-0.5)),
        BendValue::new(0x4000_0000)
    );
    // `0.5 * (2^31 - 1)` is `2^30 - 0.5`, which rounds half away from zero.
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(0.5)),
        BendValue::new(0xC000_0000)
    );
}

#[test]
fn a_position_between_two_words_rounds_to_the_nearer_on_each_side() {
    let (down, up) = (1.0 / DOWN, 1.0 / UP);
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(0.25 * up)),
        BendValue::CENTER
    );
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(0.75 * up)),
        BendValue::new(0x8000_0001)
    );
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(-0.25 * down)),
        BendValue::CENTER
    );
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(-0.75 * down)),
        BendValue::new(0x7FFF_FFFF)
    );
}
