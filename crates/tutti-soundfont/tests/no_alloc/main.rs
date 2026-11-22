//! The soundfont's realtime paths allocate nothing.

#[path = "../support/mod.rs"]
mod support;

mod rt_no_alloc;

/// Aborts inside an `RtScope` or an `assert_no_alloc` region; inert elsewhere.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;
