//! `Engine` over the graph: blocks, clocks, loop region, tempo and root channel layouts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[path = "../support/mod.rs"]
mod support;

mod block_clock;
mod block_meter;
mod engine_graph;
mod env_beat;
mod graph_contract;
mod loop_region;
mod root_channel_layouts;
mod root_crossfade;
mod rt_log;
mod tempo_ramps;
mod tempo_steps;
