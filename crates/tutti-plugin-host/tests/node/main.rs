//! The generic plugin node: a chunked processor, and an in-process VST2 node's realtime safety and retirement.
//!
//! The binary's global allocator is armed (`RtAllocCheck`), so its no-alloc gates bite.

// `assert_no_alloc` is inert unless `RtAllocCheck` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod chunked_node;
mod vst2_in_process_no_alloc;
mod vst2_in_process_retire;
