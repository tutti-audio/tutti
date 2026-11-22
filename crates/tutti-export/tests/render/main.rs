//! The render and encode path: cancel, clipping, event loss, fork failure, gain and outputs.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/mod.rs"]
mod support;

mod cancel;
mod clipping;
mod event_loss;
mod fork_failure;
mod gain;
mod outputs;
mod render;
mod write_buffers_fold;
