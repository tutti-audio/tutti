//! The dynamics nodes as the graph runs them.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod dynamic_eq;
mod dynamics_nodes;
mod key_filter;
mod leveller;
mod transient_shaper;
