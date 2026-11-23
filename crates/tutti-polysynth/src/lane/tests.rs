use super::*;
use tutti_units::Real;

/// A group counting `0, 1, 2, ...` across its lanes.
fn counting<L: Lane>() -> L {
    let mut v = L::ZERO;
    for (k, lane) in v.as_mut_slice().iter_mut().enumerate() {
        *lane = L::Scalar::from_design(k as f64);
    }
    v
}

/// A comparison's mask selects exactly the lanes it holds for, and reports them as bits
/// with lane 0 lowest: lanes `0..3` are below 3, so the mask is `0b111`.
fn masks_select_and_count_lanes<L: Lane>() {
    let c: L = counting();
    let three = L::splat(L::Scalar::from_design(3.0));
    let below = c.lt(three);
    assert_eq!(below.bits(), 0b111);
    assert!(below.any());
    assert!(!c.lt(L::ZERO).any(), "no lane is below zero");

    let picked = below.pick(L::ONE, L::ZERO);
    let want: Vec<f64> = (0..L::LANES).map(|k| f64::from(u8::from(k < 3))).collect();
    let got: Vec<f64> = picked.as_slice().iter().map(|&x| x.into()).collect();
    assert_eq!(got, want);
    assert_eq!(c.ge(three).bits(), !0b111 & ((1 << L::LANES) - 1));
    assert_eq!(c.ne(three).bits().count_ones() as usize, L::LANES - 1);
    assert_eq!(c.gt(three).bits().count_ones() as usize, L::LANES - 4);
}

/// The lanes' sum is `0 + 1 + ... + (LANES - 1)`, exact at both widths.
fn sum_adds_every_lane<L: Lane>() {
    let c: L = counting();
    let n = L::LANES as f64;
    assert_eq!(c.sum().into(), n * (n - 1.0) / 2.0);
}

/// `mul_add` is `a * b + c` in every lane.
fn mul_add_is_a_product_and_a_sum<L: Lane>() {
    let c: L = counting();
    let two = L::splat(L::Scalar::from_design(2.0));
    let ten = L::splat(L::Scalar::from_design(10.0));
    let got: Vec<f64> = c
        .mul_add(two, ten)
        .as_slice()
        .iter()
        .map(|&x| x.into())
        .collect();
    let want: Vec<f64> = (0..L::LANES).map(|k| 2.0 * k as f64 + 10.0).collect();
    assert_eq!(got, want);
}

#[test]
fn a_comparison_selects_and_counts_its_lanes_at_both_widths() {
    masks_select_and_count_lanes::<f32x8>();
    masks_select_and_count_lanes::<f64x4>();
}

#[test]
fn the_sum_adds_every_lane_at_both_widths() {
    sum_adds_every_lane::<f32x8>();
    sum_adds_every_lane::<f64x4>();
}

#[test]
fn mul_add_is_a_product_and_a_sum_at_both_widths() {
    mul_add_is_a_product_and_a_sum::<f32x8>();
    mul_add_is_a_product_and_a_sum::<f64x4>();
}

#[test]
fn a_group_is_eight_lanes_at_f32_and_four_at_f64() {
    assert_eq!(f32x8::LANES, 8);
    assert_eq!(f64x4::LANES, 4);
    assert_eq!(f32x8::ZERO.as_slice().len(), f32x8::LANES);
    assert_eq!(f64x4::ZERO.as_slice().len(), f64x4::LANES);
}
