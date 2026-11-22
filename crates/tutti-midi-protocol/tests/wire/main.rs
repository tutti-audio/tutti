//! Wire values: Universal System Exclusive, the MIDI Tuning Standard, time
//! values, Flex data and the range contract of `tutti-ump`'s fields.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod flex;
mod persisted;
mod time;
mod tuning;
mod ump_ranges;
mod universal;
