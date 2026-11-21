use super::*;

/// `LowestPitch` keeps the range-checked shape: `new` refuses a pitch that cannot size a
/// line (zero, negative, under 1 Hz, not finite) and never clamps, 1 Hz is accepted.
#[test]
fn a_lowest_pitch_keeps_the_range_checked_shape() {
    let c = tutti_fixtures::RangeContract {
        new: LowestPitch::new,
        get: LowestPitch::get,
        of: Some(LowestPitch::of),
        valid: &[Hz::new(1.0), Hz::new(20.0), Hz::new(1_000.0)],
        invalid: &[
            Hz::ZERO,
            Hz::new(0.999_999),
            Hz::new(-20.0),
            Hz::new(f64::NAN),
            Hz::new(f64::INFINITY),
            Hz::new(f64::NEG_INFINITY),
        ],
    };
    tutti_fixtures::check_range_contract(&c);
}
