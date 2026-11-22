//! Exports of a graph: analytic figures, the sampler and surround.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/mod.rs"]
mod support;

mod graph_source;
mod sampler_to_export;
mod surround_export;
