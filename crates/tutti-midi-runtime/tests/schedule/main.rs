//! What a clip and a harmony lane send, and what they count when they lose.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../common/mod.rs"]
mod common;

mod clip_node;
mod event_loss;
mod harmony_node;
