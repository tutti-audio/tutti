//! `Engine` over the graph: blocks, clocks, loop region, tempo and main output channel layouts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/mod.rs"]
mod support;

mod block_clock;
mod block_meter;
mod engine_graph;
mod env_beat;
mod graph_contract;
mod loop_region;
mod main_output_channel_layouts;
mod main_output_crossfade;
mod rt_log;
mod tempo_ramps;
mod tempo_steps;
