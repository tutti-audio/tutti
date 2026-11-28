//! The SoundFont unit as a graph node: sample-accuracy contract, event input, state chase and declared ring-out.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod chase;
mod contract;
mod f64_unit;
mod graph_node;
mod node_config;
mod ring_out;
#[path = "../support/mod.rs"]
mod support;
