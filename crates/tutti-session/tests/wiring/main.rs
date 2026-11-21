//! The session's graph: wiring, buses, edge gain, latency, the root and the scene render.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// Inert outside `assert_no_alloc`, so every test here may also gate a closure on
/// allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod block_dates;
mod build_frame;
mod bus_edges;
mod compensation;
mod connect;
mod declick;
mod edge_gain;
mod engine;
mod event_ports;
mod f64_session;
mod failure_rule;
mod fan_in;
mod graph;
mod latency_removal;
mod loop_region;
mod output_policy;
mod rewired;
mod root_crossfade;
mod root_defer;
mod root_reopen;
mod root_topology;
mod scene_render;
mod session_targets;
mod staged_values;
mod system_clock;
#[path = "../support/tempo.rs"]
mod tempo;
mod transport_handle;
mod transport_verbs;
mod typed_params;
mod unwired;
mod values;
mod vocabulary;
