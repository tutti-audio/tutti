//! The engine's allocation gates and its load meter. The binary installs
//! `AllocDisabler`, so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[path = "../support/mod.rs"]
mod support;

mod block_no_alloc;
mod fault_records;
mod load;
mod rt_no_alloc_engine;
