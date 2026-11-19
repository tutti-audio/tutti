//! The platform edges' allocation gates. The binary installs `AllocDisabler`,
//! so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod platform_no_alloc;
