//! The device layer without a sound card: the sample-format matrix the output
//! driver converts through, and its allocation gate. The binary installs
//! `AllocDisabler`, so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod support;

mod callback_no_alloc;
mod output_block;
