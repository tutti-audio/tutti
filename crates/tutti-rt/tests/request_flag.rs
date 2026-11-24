//! `RequestFlag`'s range: every index below 32 is a flag and every other is
//! refused at every door.

#![cfg(not(loom))]

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_rt::{AtomicRequestFlags, RequestFlag, RequestFlagOutOfRange, RequestFlags};

#[test]
fn the_range_is_held_at_every_door() {
    let valid: Vec<u8> = (0..32).collect();
    check_range_contract(&RangeContract {
        new: RequestFlag::new,
        get: RequestFlag::get,
        of: Some(RequestFlag::of),
        valid: &valid,
        invalid: &[32, 33, u8::MAX],
    });
}

#[test]
fn the_refusal_carries_the_raw() {
    assert_eq!(RequestFlag::try_from(40), Err(RequestFlagOutOfRange(40)));
}

/// A set holds every one of its 32 flags apart: raising each in turn and
/// taking returns exactly that flag, and all of them when all are raised.
#[test]
fn every_bit_is_taken_alone_and_together() {
    let flags = AtomicRequestFlags::new();
    for index in 0..32 {
        let flag = RequestFlag::of(index);
        flags.raise(flag);
        let taken = flags.take();
        for other in 0..32 {
            assert_eq!(taken.contains(RequestFlag::of(other)), other == index);
        }
    }
    for index in 0..32 {
        flags.raise(RequestFlag::of(index));
    }
    let all = flags.take();
    assert!((0..32).all(|index| all.contains(RequestFlag::of(index))));
    assert_eq!(flags.take(), RequestFlags::NONE);
}
