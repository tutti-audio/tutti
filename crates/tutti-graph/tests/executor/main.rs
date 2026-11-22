//! The executor, the worker pool, the sample-accuracy contract and the reader ends of taps and inlets.
//!
//! The binary installs `RtAllocCheck`, so an allocation inside an `assert_no_alloc`
//! closure aborts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod by_hand;
mod chase;
mod contract;
mod cx_meter;
mod executor;
mod f64_render;
mod inlet_no_alloc;
mod inlet_retire;
mod main_output_fade;
mod meter;
mod no_denormals;
mod parallel;
mod parked_poll;
mod pool_spawn_refusal;
mod render_mode_scope;
mod rt_log;
mod rt_no_alloc;
mod tap;
mod tap_audio_in;
mod wake;
mod wired;
