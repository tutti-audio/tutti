//! The runtime primitives' allocation gates. The binary installs `AllocDisabler`,
//! so an allocation inside an `assert_no_alloc` closure aborts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod rt_no_alloc;
