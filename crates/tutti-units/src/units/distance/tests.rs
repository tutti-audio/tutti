use super::*;

// What is deliberately *absent* is as load-bearing as what is present.
// These must not compile; the ledger is here so a future contributor does
// not "helpfully" add them.
//
// - `Metres - Metres` — the difference can be negative, which no `Metres` is;
//   `Metres::abs_diff` is the replacement.
// - `Metres * f64`, `Metres / f64` — a negative or NaN factor leaves the range;
//   `Metres::scaled` is the replacement and returns `None` for it.
// - `-Metres`, `-Speed` — there is no signed distance or velocity type.
// - `Metres::from(f64)`, `Speed::from(f64)`, `Unit::from_raw` — each could build
//   a value the type refuses; `new` is the checked constructor.
// - `Speed * Seconds -> Metres` — not yet used.
// - `Metres / Seconds -> Speed` — not yet used.
// - `Speed + Speed`, `Speed * f64` — the speed of sound is one medium's
//   constant, not an amount.

/// **A distance over the speed of sound is the delay `r / c`**: 343 m at 343 m/s
/// is one second, and the delay is linear in the distance.
#[test]
fn a_distance_over_the_speed_of_sound_is_its_delay() {
    assert_eq!(
        Metres::of(343.0) / Speed::SOUND_IN_AIR_20C,
        Seconds::new(1.0)
    );
    assert_eq!(
        Metres::of(171.5) / Speed::SOUND_IN_AIR_20C,
        Seconds::new(0.5)
    );
    assert_eq!(Metres::ZERO / Speed::SOUND_IN_AIR_20C, Seconds::new(0.0));
}

/// **The ratio of two distances is the bare number `a / b`**, the `1 / r` of an
/// inverse-distance gain.
#[test]
fn a_ratio_of_distances_is_a_bare_number() {
    assert_eq!(Metres::of(1.0) / Metres::of(4.0), 0.25);
    assert_eq!(Metres::of(6.0) / Metres::of(3.0), 2.0);
}

/// **`abs_diff` is the gap in either order**, and `Add` sums and stays in range.
#[test]
fn abs_diff_is_the_gap_and_add_keeps_the_range() {
    let (a, b) = (Metres::of(5.0), Metres::of(2.0));
    assert_eq!(a.abs_diff(b), Metres::of(3.0));
    assert_eq!(b.abs_diff(a), Metres::of(3.0));
    assert_eq!(a.abs_diff(a), Metres::ZERO);
    assert_eq!(a + b, Metres::of(7.0));
    let mut c = a;
    c += b;
    assert_eq!(c, Metres::of(7.0));
    assert_eq!(
        Metres::of(f64::MAX) + Metres::of(f64::MAX),
        Metres::of(f64::MAX)
    );
}

/// **`scaled` refuses what `new` refuses**: a negative or NaN factor and an
/// overflow.
#[test]
fn scaled_refuses_a_product_that_is_not_a_distance() {
    assert_eq!(Metres::of(2.0).scaled(1.5), Some(Metres::of(3.0)));
    assert_eq!(Metres::of(2.0).scaled(0.0), Some(Metres::ZERO));
    assert_eq!(Metres::of(2.0).scaled(-1.0), None);
    assert_eq!(Metres::of(2.0).scaled(f64::NAN), None);
    assert_eq!(Metres::of(f64::MAX).scaled(2.0), None);
}

/// A distance orders and bounds in its own unit.
#[test]
fn a_distance_orders_and_bounds() {
    let (lo, hi) = (Metres::of(1.0), Metres::of(3.0));
    assert!(lo < hi);
    assert_eq!(lo.max(hi), hi);
    assert_eq!(lo.min(hi), lo);
    assert_eq!(Metres::of(9.0).clamp(lo, hi), hi);
    assert!(Speed::of(300.0) < Speed::SOUND_IN_AIR_20C);
    assert_eq!(Metres::default(), Metres::ZERO);
}
