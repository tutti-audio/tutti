//! `RequestBit`'s range: every index below 32 is a bit and every other is
//! refused at every door.

#![cfg(not(loom))]

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_rt::{RequestBit, RequestBitOutOfRange, RequestBits, RequestWord};

#[test]
fn the_range_is_held_at_every_door() {
    let valid: Vec<u8> = (0..32).collect();
    check_range_contract(&RangeContract {
        new: RequestBit::new,
        get: RequestBit::get,
        of: Some(RequestBit::of),
        valid: &valid,
        invalid: &[32, 33, u8::MAX],
    });
}

#[test]
fn the_refusal_carries_the_raw() {
    assert_eq!(RequestBit::try_from(40), Err(RequestBitOutOfRange(40)));
}

/// A word holds every one of its 32 bits apart: raising each in turn and
/// taking returns exactly that bit, lowest first when all are raised.
#[test]
fn every_bit_is_taken_alone_and_together() {
    let word = RequestWord::new();
    for index in 0..32 {
        let bit = RequestBit::of(index);
        word.raise(bit);
        let taken = word.take();
        assert_eq!(taken.iter().collect::<Vec<_>>(), [bit]);
    }
    for index in 0..32 {
        word.raise(RequestBit::of(index));
    }
    let all: Vec<u8> = word.take().iter().map(RequestBit::get).collect();
    assert_eq!(all, (0..32).collect::<Vec<_>>());
    assert_eq!(word.take(), RequestBits::NONE);
}
