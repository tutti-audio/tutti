//! The engine's allocation gates and its load meter. The binary installs
//! `RtAllocCheck`, so an allocation inside an `assert_no_alloc` closure aborts.

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/mod.rs"]
mod support;

mod block_no_alloc;
mod fault_records;
mod load;
mod rt_no_alloc_engine;
