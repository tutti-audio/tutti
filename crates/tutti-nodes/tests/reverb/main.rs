//! The reverb nodes and the design node's ports.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod design_node_ports;
mod reverb_fdn_node;
mod reverb_node;
mod reverb_plate_node;
mod reverb_shimmer_node;
