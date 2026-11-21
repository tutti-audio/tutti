//! The VST2 host against the shared traits, its shell enumeration and its realtime safety, on the
//! reference plugins.
//!
//! The binary's global allocator is armed (`RtAllocCheck`), so its no-alloc gates bite.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[path = "../support/active.rs"]
mod active;

// `assert_no_alloc` is inert unless `RtAllocCheck` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod vst2_host_process_no_alloc;
mod vst2_shell;
mod vst2_traits;
