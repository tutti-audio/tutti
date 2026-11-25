//! Weighted edges, mix, mode, param ports and audio fan-in.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod azimuth_arc;
mod base_smoothing;
mod declared_cells;
mod declared_cells_config;
mod declared_cells_entries;
mod declared_published;
mod declared_reads;
mod declared_settings;
mod edge_fade;
mod edge_gain;
mod edge_gain_driven;
mod f64_lanes;
mod fan_in;
mod fan_in_views;
mod indexed_params;
mod mix;
mod mix_driven;
mod mode;
mod param_mod;
mod rendezvous;
