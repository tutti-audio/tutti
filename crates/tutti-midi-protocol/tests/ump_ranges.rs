//! The range-checked fields of `tutti-ump` keep the range contract: the crate
//! names no other workspace crate, so the check runs from here.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::TicksPerQuarterNote;

/// Ticks per quarter note run 1 to 65535, with 0 reserved (M2-104 §7.2.3.1);
/// the type holds exactly that range, so a sender cannot write the reserved 0.
/// Spec: m2-104/7.2.3.1#1
#[test]
fn a_ticks_per_quarter_note_keeps_the_range_contract() {
    check_range_contract(&RangeContract {
        new: TicksPerQuarterNote::new,
        get: TicksPerQuarterNote::get,
        of: Some(TicksPerQuarterNote::of),
        valid: &[1, 2, 480, 65534, 65535],
        invalid: &[0],
    });
}
