//! What the SoundFont 2.04 specification requires of the player.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod spec;
#[path = "../support/mod.rs"]
mod support;
