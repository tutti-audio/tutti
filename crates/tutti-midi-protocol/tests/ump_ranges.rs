//! The range-checked fields of `tutti-ump` keep the range contract: the crate
//! names no other workspace crate, so the check runs from here.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::{FirstGroup, FunctionBlockCount, FunctionBlockId, GroupSpan, TicksPerQuarterNote};

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

/// Block numbers are 0 to 31 (M2-104 §7.1.7); a number above is read as a
/// reserved field, never held by the type.
/// Spec: m2-104/7.1.7#1
#[test]
fn a_function_block_id_keeps_the_range_contract() {
    check_range_contract(&RangeContract {
        new: FunctionBlockId::new,
        get: FunctionBlockId::get,
        of: Some(FunctionBlockId::of),
        valid: &[0, 1, 30, 31],
        invalid: &[32, 127, 128, 254, 255],
    });
}

/// A block starts on any of the 16 Groups and spans one to sixteen of them
/// (M2-104 §6.1.1).
/// Spec: m2-104/6.1.1#1
#[test]
fn a_first_group_and_a_group_span_keep_the_range_contract() {
    check_range_contract(&RangeContract {
        new: FirstGroup::new,
        get: FirstGroup::get,
        of: Some(FirstGroup::of),
        valid: &[0, 1, 14, 15],
        invalid: &[16, 17, 255],
    });
    check_range_contract(&RangeContract {
        new: GroupSpan::new,
        get: GroupSpan::get,
        of: Some(GroupSpan::of),
        valid: &[1, 2, 15, 16],
        invalid: &[0, 17, 255],
    });
}
