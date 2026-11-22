//! The synth's audio: voices, per-channel and per-key state, heard at the output.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod channel_state;
mod f64_synth;
#[path = "../support/mod.rs"]
mod support;
mod synth_audio;
