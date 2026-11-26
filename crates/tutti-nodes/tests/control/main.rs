//! Control lanes and modulation sources.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod automation_segments;
mod azimuth_lane;
mod control_lane;
mod corners;
mod graph_param_mod;
mod live_controls_reach_the_node;
mod param_set_routes;
mod signal_sources;
