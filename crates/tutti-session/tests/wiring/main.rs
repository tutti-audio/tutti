//! The session's graph: wiring, buses, edge gain, latency, the main output and the scene render.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]
#![allow(
    unreachable_pub,
    reason = "a node declared with the cells form is the `Node` of its pub config, so it is pub"
)]

/// Inert outside `assert_no_alloc`, so every test here may also gate a closure on
/// allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

// The offline sessions the files share.
mod block_dates;
mod build_frame;
mod bus_edges;
// A node's published cells, through a MIDI clip kind.
#[cfg(feature = "midi")]
mod cells_node;
mod compensation;
mod connect;
mod declick;
mod edge_gain;
mod edit_log;
mod engine;
mod event_ports;
mod f64_session;
mod failure_rule;
mod fan_in;
mod gesture;
mod graph;
mod latency_removal;
mod loop_region;
mod main_output_crossfade;
mod main_output_defer;
mod main_output_reopen;
mod main_output_topology;
#[path = "../support/offline.rs"]
mod offline;
mod output_policy;
mod replace_seeding;
mod rewired;
mod saved;
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
mod watch;

// Starts a bounce, awaits its render and applies it.
#[cfg(feature = "export")]
#[path = "../support/bounced.rs"]
mod bounced;
