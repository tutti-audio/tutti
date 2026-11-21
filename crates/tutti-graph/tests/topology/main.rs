//! The topology value, the compiler and the editor: building, validating, compiling, replacing and forking.
//!
//! The binary installs `RtAllocCheck`, so an allocation inside an `assert_no_alloc`
//! closure aborts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod builder;
mod bus_edges;
mod bus_no_alloc;
mod buses;
mod compile_passes;
mod cost_model;
mod fork;
mod graph_value;
mod limits;
mod replace;
mod reprepare;
mod revision;
mod root_width;
mod set_latency;
mod set_tail;
mod unwired;
