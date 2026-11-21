//! The device layer without a sound card: the sample-format matrix the output
//! driver converts through, and its allocation gate. The binary installs
//! `RtAllocCheck`, so an allocation inside an `assert_no_alloc` closure aborts.

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod support;

mod callback_no_alloc;
mod output_block;
