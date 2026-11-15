use tutti_fixtures::{RangeContract, check_range_contract};

use super::*;

/// `MaxDecay` keeps the range-checked shape: `new` refuses and
/// never clamps, `TryFrom` agrees and carries the refused raw.
#[test]
fn a_max_decay_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: MaxDecay::new,
        get: MaxDecay::get,
        of: Some(MaxDecay::of),
        valid: &[Seconds::new(0.001), Seconds::new(8.0), MaxDecay::CAP],
        invalid: &[
            Seconds::new(0.0),
            Seconds::new(-1.0),
            Seconds::new(60.001),
            Seconds::new(1e9),
            Seconds::new(f64::NAN),
            Seconds::new(f64::INFINITY),
        ],
    });
}
