//! The synth as a graph node: the sample-accuracy contract, cost classes, event input and param lanes.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod contract;
mod cost_classes;
mod graph_node;
mod param_lanes;
