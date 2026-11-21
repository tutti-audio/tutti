use super::*;

// What is deliberately *absent* is as load-bearing as what is present.
// These must not compile; the ledger is here so a future contributor does
// not "helpfully" add them.
//
// - `FrequencyRatio * FrequencyRatio` — the product can overflow or underflow out of the
//   range; multiply through `Hz`: `f * a * b`.
// - `Hz / Hz -> FrequencyRatio` — `Hz / Hz` is the bare `f64`; the checked conversion is
//   `FrequencyRatio::new(f / base)`.
// - `FrequencyRatio + FrequencyRatio`, `FrequencyRatio::from(f64)` — `new` is the checked
//   constructor.
// - `FrequencyRatio: Unit` — a configuration value, not a parameter cell.
// - `ModulationIndex - ModulationIndex` — the difference can be negative.
// - `ModulationIndex + ModulationIndex` — not yet used;
//   `ModulationIndex::new(a.get() + b.get())`.
// - `ModulationIndex * f64`, `/ f64` — `ModulationIndex::scaled` is the replacement.
// - `ModulationIndex::from(f64)` — `new` is the checked constructor; `from_raw` is
//   `clamped`.

/// **A frequency times a ratio is the analytic product**, in either order.
#[test]
fn a_frequency_times_a_ratio_is_the_product_in_either_order() {
    let f = Hz::new(440.0);
    assert_eq!(f * FrequencyRatio::of(1.5), Hz::new(660.0));
    assert_eq!(FrequencyRatio::of(1.5) * f, Hz::new(660.0));
    assert_eq!(f * FrequencyRatio::of(0.5), Hz::new(220.0));
}

/// **`OCTAVE` doubles and `UNISON` keeps the frequency.**
#[test]
fn octave_doubles_and_unison_keeps() {
    let f = Hz::new(110.0);
    assert_eq!(f * FrequencyRatio::OCTAVE, Hz::new(220.0));
    assert_eq!(f * FrequencyRatio::UNISON, f);
    assert!(FrequencyRatio::UNISON < FrequencyRatio::OCTAVE);
}

/// **`clamped` maps NaN and a negative to zero** and keeps a valid value; infinity
/// saturates.
#[test]
fn clamped_maps_nan_and_negatives_to_zero_and_keeps_a_valid_value() {
    assert_eq!(ModulationIndex::clamped(f64::NAN), ModulationIndex::ZERO);
    assert_eq!(ModulationIndex::clamped(-0.5), ModulationIndex::ZERO);
    assert_eq!(
        ModulationIndex::clamped(f64::NEG_INFINITY),
        ModulationIndex::ZERO
    );
    assert_eq!(ModulationIndex::clamped(3.0), ModulationIndex::of(3.0));
    assert_eq!(
        ModulationIndex::clamped(f64::INFINITY),
        ModulationIndex::of(f64::MAX)
    );
}

/// **`scaled` refuses what `new` refuses**: a negative or NaN factor and an overflow.
#[test]
fn scaled_refuses_a_product_that_is_not_an_index() {
    assert_eq!(
        ModulationIndex::of(2.0).scaled(1.5),
        Some(ModulationIndex::of(3.0))
    );
    assert_eq!(
        ModulationIndex::of(2.0).scaled(0.0),
        Some(ModulationIndex::ZERO)
    );
    assert_eq!(ModulationIndex::of(2.0).scaled(-1.0), None);
    assert_eq!(ModulationIndex::of(2.0).scaled(f64::NAN), None);
    assert_eq!(ModulationIndex::of(f64::MAX).scaled(2.0), None);
}

/// **`to_radians` keeps the value**: an index of `beta` is `beta` radians of peak deviation.
#[test]
fn to_radians_keeps_the_value() {
    assert_eq!(ModulationIndex::of(2.5).to_radians(), Radians::new(2.5));
    assert_eq!(ModulationIndex::ZERO.to_radians(), Radians::new(0.0));
}

/// **`from_raw` saturates** like `clamped`, `to_raw` reads the value back, and the default
/// is zero.
#[test]
fn from_raw_saturates_and_default_is_zero() {
    assert_eq!(ModulationIndex::from_raw(-1.0), ModulationIndex::ZERO);
    assert_eq!(ModulationIndex::from_raw(f64::NAN), ModulationIndex::ZERO);
    assert_eq!(
        ModulationIndex::from_raw(f64::INFINITY),
        ModulationIndex::of(f64::MAX)
    );
    assert_eq!(ModulationIndex::from_raw(1.25).to_raw(), 1.25);
    assert_eq!(ModulationIndex::default(), ModulationIndex::ZERO);
    assert!(ModulationIndex::of(1.0) < ModulationIndex::of(2.0));
}
