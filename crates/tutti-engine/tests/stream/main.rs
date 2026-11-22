//! The block seam under every clock, without a sound card: the output block, the
//! stream lifecycle and the restart path, run through `ManualStreamDriver`, and
//! the timer driver on its own thread.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod fixtures;

mod f64_render;
mod lifecycle;
mod output_block;
mod process_audio;
mod timer;
