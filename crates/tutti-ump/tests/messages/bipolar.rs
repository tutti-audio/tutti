//! A pitch bend as a position on `-1..=1`: `(get - 2^31) / 2^31`, and
//! `round(2^31 + b * 2^31)` back, saturating at `u32::MAX`.

use tutti_ump::BendValue;
use tutti_units::Bipolar;

const HALF_RANGE: f64 = 2_147_483_648.0;

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
fn a_bend_is_its_distance_from_the_centre_over_two_to_the_31() {
    assert_eq!(BendValue::MIN.bipolar(), Bipolar::MIN);
    assert_eq!(BendValue::CENTER.bipolar(), Bipolar::ZERO);
    assert_eq!(BendValue::MAX.bipolar().get(), 1.0 - 1.0 / HALF_RANGE);
    assert_eq!(BendValue::new(0x4000_0000).bipolar().get(), -0.5);
    assert_eq!(BendValue::new(0xC000_0000).bipolar().get(), 0.5);
    assert_eq!(
        BendValue::new(0x8000_0001).bipolar().get(),
        1.0 / HALF_RANGE
    );
    assert_eq!(
        BendValue::new(0x7FFF_FFFF).bipolar().get(),
        -1.0 / HALF_RANGE
    );
    for raw in samples() {
        assert_eq!(
            BendValue::new(raw).bipolar().get(),
            (f64::from(raw) - HALF_RANGE) / HALF_RANGE,
            "{raw:#x}"
        );
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
fn full_positive_bipolar_saturates_at_the_wire_maximum() {
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(1.0)),
        BendValue::new(u32::MAX)
    );
    assert_eq!(BendValue::from_bipolar(Bipolar::of(-1.0)), BendValue::MIN);
    assert_eq!(BendValue::from_bipolar(Bipolar::ZERO), BendValue::CENTER);
}

#[test]
fn minus_one_half_is_a_quarter_of_the_wire_range() {
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(-0.5)),
        BendValue::new(0x4000_0000)
    );
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(0.5)),
        BendValue::new(0xC000_0000)
    );
}

#[test]
fn a_position_between_two_words_rounds_to_the_nearer() {
    // Quarter and three-quarter steps of one word either side of the centre.
    let step = 1.0 / HALF_RANGE;
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(0.25 * step)),
        BendValue::CENTER
    );
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(0.75 * step)),
        BendValue::new(0x8000_0001)
    );
    assert_eq!(
        BendValue::from_bipolar(Bipolar::of(-0.75 * step)),
        BendValue::new(0x7FFF_FFFF)
    );
}
