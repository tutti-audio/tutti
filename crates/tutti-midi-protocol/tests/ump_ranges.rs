//! The range-checked fields of `tutti-ump` keep the range contract: the crate
//! names no other workspace crate, so the check runs from here.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::{FunctionBlockCount, TicksPerQuarterNote};

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

/// An Endpoint declares at most 32 Function Blocks (M2-104 §6.1.1, §7.1.2);
/// the count type holds exactly that range, so a sender cannot write a
/// reserved count.
/// Spec: m2-104/6.1.1#2
#[test]
fn a_function_block_count_keeps_the_range_contract() {
    check_range_contract(&RangeContract {
        new: FunctionBlockCount::new,
        get: FunctionBlockCount::get,
        of: Some(FunctionBlockCount::of),
        valid: &[0, 1, 31, 32],
        invalid: &[33, 64, 127, 128, 255],
    });
}
